# Releasing TcrBar.app

How a signed, notarized, self-updating `TcrBar.app` gets from this repository onto someone else's
Mac. The CLI (`tcr`) is released separately by `.github/workflows/release.yml`, which cargo-dist
generates; this document is only about the app.

Everything below is executed by `apps/macos/scripts/release-tcrbar.sh`. **Releases are cut locally,
from the Mac that holds the signing keys** — not from CI. Read the script if you want the detail;
this is the operator's view.

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

## Cutting a release

One command, from the Mac that holds the keys:

```sh
apps/macos/scripts/release-local.sh v0.2.0
```

`release-local.sh` is a credential wrapper and nothing else. It reads the App Store Connect API key
out of 1Password, materialises the `.p8` as a 0600 file in a private temp dir — `notarytool` takes
`--key <path>`, there is no form of the flag that accepts the key material as a value — removes it
on a `trap` so an aborted build does not leave a private key in `/tmp`, and then `exec`s
`release-tcrbar.sh --tag`. The certificate and the Sparkle private key it does *not* pass: those are
read straight from the login keychain by `release-tcrbar.sh`.

Before that, in order:

```sh
# 1. bump the version — Cargo.toml is the single source of truth. The app's
#    CFBundleShortVersionString, the tag and `tcr --version` all derive from it.
$EDITOR Cargo.toml && cargo check          # refresh Cargo.lock

# 2. rehearse. Builds, signs, hardens, makes the DMG, signs the appcast entry.
#    Notarization and upload are skipped, so it needs no credentials beyond the
#    certificate.
apps/macos/scripts/release-tcrbar.sh --dry-run

# 3. tag and push. `--tag` must agree with Cargo.toml or the release aborts.
git tag v0.2.0 && git push origin v0.2.0
```

The tag push is a separate, deliberate step: a repository ruleset named **"Protect release tags"**
restricts creating, updating and deleting `v*` tags to admins. A collaborator with push cannot
mint a version tag, which is the point — see below.

`TCRBAR_OP_ITEM` overrides the 1Password item reference (default
`op://Employee/TcrBar Release Signing`); extra arguments after the tag are passed through to
`release-tcrbar.sh`, so `release-local.sh v0.2.0 --dry-run` works.

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

## Why there are no signing secrets in GitHub

**All seven signing secrets were removed from this repository on 2026-08-09, deliberately.** Not as
cleanup, and not pending a fix — the release path is local and is meant to stay local. Three facts
compose into the reason:

- the repository is **public**, and non-admin collaborators hold **push**;
- a tag-triggered workflow executes **the workflow file as it exists on the tagged commit**, so
  whoever can reach a `v*` tag chooses the code that runs with the secrets attached;
- Sparkle's private key signs the payload that **every existing install auto-trusts**. It is not a
  key that protects a build artifact; it is the key that decides what already-installed copies will
  execute.

Nothing stored is nothing stolen. The keys exist on one Mac and in one 1Password item, so a GitHub
compromise — a leaked token, a mis-scoped collaborator, a workflow edited on a branch — reaches no
signing material at all.

The tag itself is defended separately, because the argument above turns on who can create one: a
repository ruleset named **"Protect release tags"** restricts creating, updating and deleting `v*`
tags to admins. A collaborator with push can land code; they cannot mint the tag that would make CI
sign it.

### Where each credential lives now

1Password, vault **Employee**, item **TcrBar Release Signing**:

| field | also in the login keychain? | notes |
|---|---|---|
| `APPLE_API_KEY_P8` | no | **1Password is the only copy.** Apple lets you download the `.p8` exactly once; it cannot be re-downloaded, only revoked and reissued. |
| `APPLE_API_KEY_ID` | no | the Key ID shown beside that key |
| `APPLE_API_ISSUER_ID` | no | the Issuer ID at the top of the App Store Connect API page |
| `APPLE_TEAM_ID` | — | not a secret; embedded in every signature we ship |
| `SPARKLE_ED_PRIVATE_KEY` | yes | `generate_keys` stores it in the login keychain and that is what signs. **1Password is the only backup**; lose both and no future build can update an installed copy. |
| `SPARKLE_ED_PUBLIC_KEY` | — | public by design, see below |
| `MACOS_CERT_P12_BASE64` | yes (as the cert + key) | `.p12` export of the Developer ID Application identity |
| `MACOS_CERT_PASSWORD` | — | the password chosen during that export |

The Developer ID certificate and the Sparkle key are used **from the login keychain** during a
local release; the 1Password copies are backups and the machine-rebuild path. The two that have no
other home — the `.p8` and the Sparkle private key — are the ones worth checking are still readable
before you need them.

One repository **variable** (not a secret) remains set, and should:

| variable | value |
|---|---|
| `TCRBAR_SPARKLE_PUBLIC_KEY` | `1WZWEwzSEijRarey7qE0a+n4AO/+7e4Fj/nW8Y8ZKMM=` — the public key `generate_keys` prints. It goes into `Info.plist` so an installed app can verify what it downloads. Publishing it is the point. |

### The workflow still exists, and a tag run is not broken

`.github/workflows/release-app.yml` runs on `macos-14` and still fires on `v*` tags. **Do not
"fix" it when a tag run shows a skipped signing job — that is the designed outcome.** A tag reaches
a `check-signing-secrets` job, which finds all seven unset, writes a job summary saying so, and the
signing job is **skipped** rather than failed. Setting only *some* of the seven is treated as a
misconfiguration and fails loudly, naming the missing ones: a half-configured release is likelier
to be a mistake than an intent to skip. The check must be its own job because the `secrets` context
is unavailable in a job-level `if:` (GitHub's context-availability table allows only `github`,
`needs`, `vars` and `inputs` there).

Restoring CI signing would mean setting all seven again, which re-creates exactly the exposure
described above. It must never gain a `pull_request` trigger for the same reason — it would hand
three private keys to a fork PR.

Two defences in the workflow are worth keeping if it is ever re-enabled. It imports the `.p12` into
a keychain it creates in `$RUNNER_TEMP` and deletes in an `if: always()` step — never the login
keychain — and runs `security set-key-partition-list` so `codesign` can use the key without a UI
prompt; omitting that call is the most common CI signing defect, and the job hangs until its
timeout instead of failing. And nothing in it echoes a secret: GitHub's log masking does not
reliably cover base64 fragments or substrings, so the certificate import step prints a *count* of
matching identities rather than the identity, which contains a person's name and the team id.

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

An App Store Connect API key now exists (2026-08-09) and `release-local.sh` supplies it, so the
blocker on stages 5 and 6 is gone. What has **not** happened is a completed notarized run: those two
stages are still written from Apple's documented interface rather than from a success. The first
real release is the first test of them. Run it with `--dry-run` first, and verify the result with
the section above from the downloaded DMG rather than from `build/`.
