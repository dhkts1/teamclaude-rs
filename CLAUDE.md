# Working in this repo

Notes for anyone — human or agent — making changes here. Everything below is a thing that was learned
the expensive way, not a style preference.

## This repository is public

Assume every line you commit is world-readable, because it is. This rule is kept in full here, and
not behind a link, because an agent must obey it without reading anything else; the same rule with
its full rationale is [`CONTRIBUTING.md`](CONTRIBUTING.md) § "The two things to read before your
first commit".

- **No real account data.** Account emails, organization UUIDs, account UUIDs and workspace names never
  go into fixtures, tests, commit messages, PR titles or PR bodies. Use obviously-fake values
  (`alice@example.com`, `11111111-1111-...`). The existing tests already do this — follow them.
- **No credentials, ever.** OAuth access and refresh tokens, and the proxy API key, are secrets. They do
  not belong in code, tests, logs you paste, or a PR description. A pre-commit `gitleaks` hook is the
  backstop, not the plan.
- The live config at `~/.config/teamclaude.json` holds **working credentials for real accounts**. Never
  copy it into the repo, never commit it, never paste its contents anywhere. Tests write their own
  temp-file configs; do the same.

## There is probably a live proxy running

A `tcr` server may be serving real traffic on `127.0.0.1:3456`, with client sessions pointing at it.

- **Never restart, kill or signal it** without being asked. Anthropic's prompt cache is per-account,
  so a session that comes back on a different account pays a full cold prefix. It is still the most
  expensive event in this system.
  A restart is no longer *automatic* total loss, though, and this line used to say it was. Session
  affinity persists its pins to `~/.cache/teamclaude/session-affinity.json` and restores them at
  boot: measured 2026-08-09, three restarts restored 7, 5 and 7 pins. The real limit is the TTL
  (`affinity::PIN_TTL_MS`, 15 minutes) — a restart inside it keeps most sessions warm, and one after
  it restores nothing at all (`restored=0 expired=27`, same log). None of that applies with
  `sessionAffinity` off, which requires an explicit `"sessionAffinity": false` in a fresh config — the
  default is now ON. Read the log line the server prints at boot rather than assuming either outcome.
- **You cannot replace `/Applications/TcrBar.app` while the proxy is running.** TcrBar resolves the
  *bundled* `tcr` ahead of `PATH` (`TcrTool.swift:69-82`) and supervises it as a child, so
  `Contents/MacOS/tcr` is an executing image inside the very bundle being swapped. Finder and Sparkle
  both refuse with "the item TcrBar is in use". Quitting TcrBar clears it — `applicationWillTerminate`
  stops the child — which is why every TcrBar update also restarts the proxy and spends the
  cold-prefix cost above. Plan the update for a quiet moment; it is not a free background operation.
- **`cargo build --release` is safe while it is running.** Cargo writes a new file and renames, so the
  live process keeps its own inode and its own bytes (measured 2026-08-07, N=5 — a fresh `exec` during
  the build exits 0). **Placing** a binary is the unsafe part: never `cp`, always
  `scripts/install-cli.sh` — the reason, and the 25 processes it killed, are in
  [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Installing it onto your PATH".
- **The build probably does not land in `target/`.** `CARGO_TARGET_DIR` redirects it and every agent
  session here sets it. Resolve the real path from `cargo metadata` rather than writing it down —
  command and rationale in [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Finding the binary you just built".
- The running process can be several commits behind the source. `tcr status --json` reports the running
  build's SHA — check it before concluding a fix is live. "The fix is in `main`" and "the fix is in the
  process serving traffic" are routinely different facts. (Kept in full here rather than as a link,
  because it is the check that stops a false "it's live" claim; also in
  [`docs/cli.md`](docs/cli.md) § `tcr status`.)

## Branching

- **Feature work goes on a branch — `git switch -c <branch>` in the checkout you are already in.** A
  worktree is not required for a pull request and should not be the default: each one is a GB-scale
  allocation, and a fleet of them accumulates far faster than anyone prunes it. Reach for
  `git worktree add ~/worktrees/<name> -b <branch> main` only when you genuinely need two trees at the
  same instant — a long build you must not disturb, or two branches side by side — and remove it when
  the branch lands.
- Nothing here keys on which branch you are on: the pre-commit hook in `.githooks/` runs six gates
  (secret scan, public-repo disclosure scan, Rust format, Swift format, release-version, design-token
  staleness) and looks at no branch. What each gate blocks on, and which two are hard failures when
  their tool or input is missing, is in [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Git hooks".
- Branch **before** you start editing. `git switch -c` carries uncommitted work onto the new branch,
  which is what you want; a *worktree* created after the fact does not — it branches from `HEAD`
  without those changes, and any shared file (`Cargo.toml`) blocks a clean move afterwards.
- `main` requires a pull request, one approval, and the `ci`, `audit` and `macos` checks; an admin
  bypass exists and is a deliberate decision, never a shortcut. The CI job table and how a bypass
  shows up in the push output are in [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Pull requests and CI".
- Without push access here, work from a fork; the pull request's base is this repository and its head
  is your copy. [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Opening one from a fork" has the mechanics and
  explains why a fork's checks sit unstarted until a maintainer approves the run.

## Commits

Conventional commits, **bare type only** — a parenthesised scope (`fix(quota):`) is rejected by the
commit gates; put the scope in the body. Allowed types and examples:
[`CONTRIBUTING.md`](CONTRIBUTING.md) § "Commits".

## Verifying a change

- A green test suite is a floor, not a finish line. Before trusting a new test, **watch it fail**:
  break the production change, re-run, confirm the failure is the one you expect, restore, re-run green.
  A test that has only ever passed proves nothing about what it guards.
- When you restore a file after such an experiment, verify the *file* (`git status`, re-read the line) —
  not just a re-run. `mv` preserves mtime, so a restored file can leave a stale build in place and the
  suite will re-run the broken binary.
- An empty search result is a claim about your probe, not about the world. Run a positive control — grep
  for something you know is there — before concluding something is absent.
- Cite `file:line` for claims about how the code behaves. This codebase documents *why* things are the
  way they are in module doc-comments; several of them name the specific bug they exist to prevent.
  Read the doc-comment before "fixing" what it describes.
