# Releasing TcrBar.app

How a signed, notarized, self-updating `TcrBar.app` gets from this repository onto someone else's
Mac. The CLI (`tcr`) is released separately by `.github/workflows/release.yml`, which cargo-dist
generates; this document is only about the app.

Everything below is executed by `apps/macos/scripts/release-tcrbar.sh`, either on a laptop or from
`.github/workflows/release-app.yml`. Read the script if you want the detail — this is the operator's
view.

## The certificate

**Developer ID Application.** That exact one. The names are close enough that picking the wrong one
is the normal mistake, and the resulting artifact fails in a way that looks like something else:

| certificate | what it is for | what happens if you use it here |
|---|---|---|
| **Developer ID Application** | apps distributed **outside** the Mac App Store | correct |
| Developer ID Installer | signing `.pkg` installers | we ship a `.dmg`; notarization rejects the app |
| Apple Distribution | Mac App Store submissions | cannot be used for a direct download |
| Apple Development | local development builds | Gatekeeper blocks it on every Mac but yours |

The last row is the dangerous one, because a development-signed build works perfectly on the machine
that built it. `release-tcrbar.sh` asserts the class before doing anything else and aborts naming the
certificate to create; that assert is the reason this failure cannot ship silently.

### Creating it

1. <https://developer.apple.com/account/resources/certificates> → **+**
2. Software → **Developer ID Application**. Requires the **Account Holder** role on the team.
3. Upload a CSR: Keychain Access → Certificate Assistant → *Request a Certificate From a Certificate
   Authority* → "Saved to disk".
4. Download the `.cer`, double-click it. It lands in the **login** keychain along with the private
   key from step 3.
5. Confirm: `security find-identity -v -p codesigning` shows a line reading
   `"Developer ID Application: <name> (<team id>)"`.

Team ID for this project is `UJQ3GQF56Y`. That is not a secret — it is embedded in every signature
we ship. The private key, the `.p12` export and the account email are secrets and appear nowhere in
this repository.

Developer ID certificates are issued with a fixed expiry (the current one expires **2027-02-01**).
Every `codesign` call in the release path passes `--timestamp` for that reason: a timestamped
signature stays valid after its certificate expires, an untimestamped one is retroactively invalid
on every machine that already installed the app.

## Cutting a release from a laptop

```sh
# 1. bump the version — Cargo.toml is the single source of truth. The app's
#    CFBundleShortVersionString, the tag and `tcr --version` all derive from it.
$EDITOR Cargo.toml && cargo check          # refresh Cargo.lock

# 2. rehearse. Builds, signs, hardens, makes the DMG, signs the appcast entry.
#    Notarization and upload are skipped, so it needs no credentials beyond the
#    certificate.
apps/macos/scripts/release-tcrbar.sh --dry-run

# 3. tag and push. Both release workflows fire on the tag.
git tag v0.2.0 && git push origin v0.2.0
```

To do the whole thing locally instead of in CI, export the notarization variables (below) and run
`release-tcrbar.sh` with no flags.

Flags:

- `--dry-run` — everything except `notarytool submit` and `gh release upload`.
- `--skip-notarize` — build and sign the DMG but skip notarization and stapling. The result opens
  only on machines that already trust it; useful for testing an update feed, never for a release.
- `--tag vX.Y.Z` — must equal `v<Cargo.toml version>`. Mismatch aborts.
- `--verify-only <path/to/TcrBar.app>` — run the signature asserts against an existing bundle.

## What the pipeline does

1. `build-tcrbar.sh` — Swift build, bundle assembly, `tcr` bundled alongside, local signature.
2. **Assert Developer ID.** Reads the class out of `codesign -dvv` and aborts on anything else.
3. **Re-sign with `--options runtime --timestamp`.** The hardened runtime is a notarization
   prerequisite; without it `notarytool` rejects the submission with a message that does not say so.
   Nested `Contents/MacOS/tcr` is signed *before* the bundle, because the bundle's seal covers it.
4. **DMG** via `create-dmg` (`brew install create-dmg`), drag-to-Applications layout, then signed.
5. **Notarize** — `xcrun notarytool submit --wait` with an App Store Connect **API key**. Not an
   Apple ID plus app-specific password: that pair is tied to one person's account and breaks when
   their 2FA setup changes.
6. **Staple** — `xcrun stapler staple` then `stapler validate`. Stapling is what lets the DMG open
   on a machine with no network; without it Gatekeeper has to ask Apple at open time.
7. **Sparkle signature** — `sign_update` produces the EdDSA signature and byte length.
8. **Appcast** — a new `<item>` is inserted at the marker comment in `apps/macos/appcast.xml`.
9. **Publish** — `gh release upload` puts the DMG and `appcast.xml` on the release for the tag.

## CI secrets

`.github/workflows/release-app.yml` runs on `macos-14` and fires **only** on `v*` tags. It must
never gain a `pull_request` trigger: it holds three private keys, and a fork PR would get all of
them.

| secret | how to produce it |
|---|---|
| `MACOS_CERT_P12_BASE64` | Keychain Access → login → My Certificates → the Developer ID Application entry → right-click → Export → `.p12` (**include the private key** — export the certificate row with its disclosure triangle, not the bare key). Then `base64 -i cert.p12 \| pbcopy`. |
| `MACOS_CERT_PASSWORD` | the password chosen during that export |
| `KEYCHAIN_PASSWORD` | any random string, e.g. `openssl rand -base64 24`. It only scopes the ephemeral keychain the job creates and deletes. |
| `APPLE_API_KEY_P8` | App Store Connect → Users and Access → Integrations → App Store Connect API → **+**, role **Developer**. The `.p8` downloads **once and only once**; paste its whole contents, `-----BEGIN PRIVATE KEY-----` line included. |
| `APPLE_API_KEY_ID` | the Key ID shown beside that key |
| `APPLE_API_ISSUER_ID` | the Issuer ID shown at the top of that page |
| `SPARKLE_ED_PRIVATE_KEY` | Sparkle's `generate_keys` stores the key in the **login keychain**, not in a file. Export it for CI with `generate_keys -x -` and paste the output. |

Plus one repository **variable** (not a secret):

| variable | value |
|---|---|
| `TCRBAR_SPARKLE_PUBLIC_KEY` | the public key `generate_keys` prints. It goes into `Info.plist` so an installed app can verify what it downloads. Publishing it is the point. |

The workflow imports the `.p12` into a keychain it creates in `$RUNNER_TEMP` and deletes in an
`if: always()` step — never the login keychain — and runs `security set-key-partition-list` so
`codesign` can use the key without a UI prompt. Omitting that call is the most common CI signing
defect: the job hangs until its timeout instead of failing.

Nothing in the workflow echoes a secret. GitHub's log masking does not reliably cover base64
fragments or substrings, so the certificate import step prints a *count* of matching identities
rather than the identity, which contains a person's name and the team id.

## Verifying a release actually installs

Do this on a Mac that has never built the app, from the downloaded DMG — not from `build/`. A local
build is trusted for reasons a stranger's Mac will not reproduce.

```sh
# 1. the notarization ticket is stapled (works offline; this is the real test)
xcrun stapler validate ~/Downloads/TcrBar-0.2.0.dmg

# 2. Gatekeeper's own verdict on the app inside
spctl -a -vvv -t install /Volumes/TcrBar\ 0.2.0/TcrBar.app
#    expect: accepted / source=Notarized Developer ID

# 3. the signature is hardened, timestamped and chains to Apple
codesign -dvv --verbose=4 /Volumes/TcrBar\ 0.2.0/TcrBar.app 2>&1 \
  | grep -E 'flags|Timestamp|TeamIdentifier'
#    expect: flags=0x10000(runtime), a Timestamp= line, TeamIdentifier=UJQ3GQF56Y
```

Then drag it to `/Applications`, launch it, and check that **Check for Updates** finds the appcast.
A quarantined download that opens with no warning is the outcome all of the above exists to produce;
if macOS says *"cannot be opened because the developer cannot be verified"*, the build was signed
with the wrong certificate class and step 2 of the pipeline did not run.

## Known-unverified

Notarization and stapling (stages 5 and 6) have not been exercised — no App Store Connect API key
exists for this project yet. They are written from Apple's documented interface, not from a
successful run. The first real release is the first test of those two stages; run it with a
throwaway patch version before announcing anything.
