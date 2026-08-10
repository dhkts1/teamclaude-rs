# Contributing to teamclaude-rs

Thanks for looking at this. `tcr` is a rotating Anthropic API proxy that runs on a developer's own
machine and holds live OAuth credentials for real accounts, so a few of the rules below are stricter
than you might expect from a project this size. Each one is here because something went wrong once,
and the reasoning is given rather than asserted — if a rule looks like ceremony, read the paragraph
under it before working around it.

If you are an AI agent working in this repository, read [CLAUDE.md](CLAUDE.md) as well; it carries the
same rules at more depth plus the operational hazards of working next to a live proxy.

## The two things to read before your first commit

**This repository is public.** Everything you commit is world-readable the moment it is pushed, and
history on a public repo cannot be un-published — removing a secret requires rewriting a protected
branch, and every fork and clone keeps its own copy regardless. So: no OAuth access or refresh
tokens, no proxy API key, no real account emails, no organization or account UUIDs, no real workspace
names — not in code, not in fixtures, not in tests, not in commit messages, not in PR titles or
bodies. Use obviously-fake values (`alice@example.com`, `11111111-1111-...`); the existing tests
already do, so follow them. The live config at `~/.config/teamclaude.json` holds working credentials
for real accounts: never copy it into the repo, never paste its contents anywhere, and never point a
test at it. Tests write their own temp-file configs.

**There is probably a live proxy running.** A `tcr` server may be serving real traffic on
`127.0.0.1:3456` with active Claude Code sessions pointed at it. Never restart, kill or signal it
unless you were explicitly asked to. Anthropic's prompt cache is per-account, so a session that comes
back on a different account pays a full cold prefix — a restart is the single most expensive event in
this system, and it is a human's choice made at a quiet moment, not a step in your workflow.
Building is safe; see the note on `cp` below for the one build-adjacent thing that is not.

## Getting set up

```sh
git clone https://github.com/dhkts1/teamclaude-rs
cd teamclaude-rs
git config core.hooksPath .githooks      # do this before your first commit
brew install gitleaks                    # required — see below
cargo build --release
```

**Clone with full history.** A `--depth 1` clone builds the Rust binary fine but **cannot build the
macOS app at all**, and that is deliberate. `apps/macos/scripts/build-tcrbar.sh:107-133` refuses when
`git rev-parse --is-shallow-repository` is true, because `CFBundleVersion` is `git rev-list --count
HEAD` — the commits the clone actually has, which is `1` in a shallow clone. That is numerically lower
than any full-clone build, and macOS reads a decreasing `CFBundleVersion` as a downgrade:
LaunchServices can keep preferring the older copy and updaters conclude there is nothing to install.
The build would report success while shipping a version that goes backwards, so it fails loudly
instead. If you hit it: `git fetch --unshallow`.

**`gitleaks` is required, not optional.** The pre-commit hook refuses to run without it, because a
secret scan that silently does not run is worse than no secret scan — the commit succeeds and nothing
ever suggests the gate was skipped. Install it with `brew install gitleaks` on macOS, or from
[the project's install instructions](https://github.com/gitleaks/gitleaks#installing) elsewhere. If
you genuinely cannot install it and have read your own diff carefully,
`TCR_ALLOW_MISSING_GITLEAKS=1 git commit ...` is the documented, deliberate opt-out; it announces
itself in the hook output. `--no-verify` also works and skips every gate, which is usually not what
you want.

### Finding the binary you just built

`<repo>/target/release/tcr` is frequently **not** where the build landed. `CARGO_TARGET_DIR` redirects
cargo's output, and many environments here set it, which leaves a stale orphan binary at the path you
expected — installing it silently ships old code, and every "the fix is live" claim afterwards is
false. Resolve the real path instead:

```sh
cargo metadata --format-version 1 --no-deps | jq -r .target_directory
```

### Installing it onto your PATH

Use the script. Never `cp`.

```sh
scripts/install-cli.sh          # defaults to ~/.local/bin/tcr
```

`cp` onto a path something is currently executing is an in-place, same-inode rewrite of a live
executable. macOS answers by SIGKILLing every later `exec` of it with `Code Signature Invalid`; on
2026-08-06 that produced 25 crash reports in a 27-second window from exactly this pattern. Note that
`codesign -v` returns 0 on the affected file — the artifact is fine, the kernel's cached signature is
stale — so nothing you can run afterwards detects it. `install-cli.sh` stages the binary in the
destination's own directory and `rename(2)`s it into place, which is atomic and gives the destination
a new inode, so already-running processes keep their bytes. It also resolves the source from `cargo
metadata` (see above) and checks the binary's embedded `TCR_BUILD_SHA` against `HEAD`, so it will not
quietly install a stale build.

## Git hooks

`git config core.hooksPath .githooks` enables two hooks. They are not optional local nicety; CI
mirrors several of them, and one of them is the only thing standing between a token and a public
repository.

| Hook | What it does |
| --- | --- |
| `pre-commit` | Secret scan (gitleaks, on staged changes only), public-disclosure scan, `cargo fmt --check`, `swift-format lint --strict`, release-version gate, design-token staleness gate. |
| `post-merge` | Rebuilds the release binary so the on-disk artifact tracks the checkout. It never restarts a running proxy. |

Two `pre-commit` gates are **hard failures when their tool or input is missing** — the gitleaks secret
scan (`TCR_ALLOW_MISSING_GITLEAKS=1` to override) and the private-name list at
`.githooks/private-names` (`TCR_ALLOW_MISSING_PRIVATE_NAMES=1`). Both guard against publishing
something unrecoverable. The formatting gates behave the opposite way on purpose: a missing `cargo` or
`swift-format` warns and skips, because a missing formatter costs a red CI job, which is visible and
recoverable.

The disclosure scan reads **added lines only**, so pre-existing content can never block you. It rejects
absolute home paths (`/Users/<someone>/...`), real-looking email addresses, and any name listed in
`.githooks/private-names`. Synthetic users (`alice`, `bob`, `test`, `example`, `runner`, ...) and
`@example.com` / `@users.noreply.github.com` addresses are allowed, so fixtures and docs still work.

## Commits

Conventional commits, **bare type only**:

```
feat: add a thing
fix: stop the thing doing the other thing
docs: ...   refactor: ...   chore: ...   test: ...
```

A parenthesised scope — `fix(quota):` — is **rejected** by the commit gates. Put the scope in the body
of the message instead.

The release-version gate refuses a commit that changes shipped files while `Cargo.toml` still names a
version that has already been tagged and released. It is self-clearing: bump the version, stage
`Cargo.toml`, commit. Paths that install nothing — `*.md`, `docs/`, `assets/`, `.github/`,
`.githooks/` — are exempt, since a change there cannot make an artifact misreport its version.

## Pull requests and CI

`main` is protected. Changes land through a pull request with one approval and passing `ci` and
`audit` checks. An admin bypass exists; using it is a deliberate decision and GitHub records it as
`Bypassed rule violations` in the push output — read that output, because a push can succeed *and*
have bypassed the rules.

`.github/workflows/ci.yml` defines five jobs:

| Job | Runner | What it runs |
| --- | --- | --- |
| `ci` **(required)** | ubuntu | `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all --locked`, `cargo build --release --locked` |
| `audit` **(required)** | ubuntu | `cargo audit` against `Cargo.lock` — fails on real RustSec advisories, warns on unmaintained deps |
| `macos` | macOS | `cargo test --all --locked` again (different code paths: `src/singleton.rs` shells out to BSD `ps`/`lsof`, install placement relies on `rename(2)`, `$TMPDIR` is a sandbox path), plus `swift-format lint --strict`, `swift build` and `swift test` for the TcrBar app |
| `tsan` | ubuntu | ThreadSanitizer over the `concurrent_pacing_stress` harness on nightly |
| `miri` | ubuntu | UB detection over the pure leaf modules (`model::`, `account_uuid::`, `quota::`). `continue-on-error` — informational, not a gate |

`--locked` everywhere is deliberate: CI must fail rather than silently rewrite `Cargo.lock`, because
this is an auth-handling proxy and the reviewed dependency set is the one that should ship.

Before opening a PR, read your own `git diff origin/main..HEAD`. Catch the obvious before a reviewer
or a bot does.

## Testing

Unit tests live beside the code they cover; integration tests are in `tests/` with fixtures under
`tests/fixtures/`. The Swift side has its own tests under `apps/macos/Tests`, run with `swift test`
from `apps/macos`.

**A green suite is a floor, not a finish line.** Before you trust a test you just wrote, watch it
fail:

1. Break the production change it is supposed to guard.
2. Re-run the test.
3. Confirm the failure is the one you expected — not a compile error, not an unrelated assertion.
4. Restore the code.
5. Re-run, green.

A test that has only ever passed proves nothing about what it guards. When you restore the file after
this experiment, verify the *file* — `git status`, re-read the line — rather than just re-running.
`mv` preserves mtime, so a restored file can leave a stale build in place and the suite happily
re-runs the old binary.

Two related habits worth having here. An empty search result is a claim about your probe, not about
the world: run a positive control (grep for something you know is there) before concluding something
is absent. And when you claim something about how the code behaves, cite `file:line` — this codebase
documents *why* things are the way they are in module doc-comments, and several of them name the
specific bug they exist to prevent. Read the doc-comment before "fixing" what it describes.

## Where things are

| Path | What |
| --- | --- |
| `src/` | The `tcr` proxy, TUI and CLI |
| `tests/` | Integration tests and fixtures |
| `apps/macos/` | TcrBar, the SwiftUI menu-bar app, and its build/release scripts |
| `scripts/` | `install-cli.sh`, the palette generator, and assorted analysis tools |
| `.githooks/` | `pre-commit` and `post-merge`, enabled via `core.hooksPath` |
| `DESIGN.md`, `MITM-DESIGN.md` | Design rationale for the proxy and the MITM path |
| `CLAUDE.md` | The same operating rules at more depth, aimed at agents working in-tree |

## Questions

Open an issue. If you are reporting something involving credentials, quotas or account data, please
redact before posting — the issue tracker is as public as the repository.
