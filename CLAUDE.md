# Working in this repo

Notes for anyone — human or agent — making changes here. Everything below is a thing that was learned
the expensive way, not a style preference.

## This repository is public

Assume every line you commit is world-readable, because it is.

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

- **Never restart, kill or signal it** without being asked. A restart wipes the in-memory
  session→account pin map, and Anthropic's prompt cache is per-account, so every live session pays a
  full cold prefix. It is the most expensive event in this system.
- **`cargo build --release` is safe while it is running.** Cargo writes a new file and renames, so the
  live process keeps its own inode and its own bytes (measured 2026-08-07, N=5 — a fresh `exec` during
  the build exits 0). The real hazard is overwriting a binary *in place*: `cp` onto a path something is
  executing rewrites the **same inode**, and macOS then SIGKILLs every later `exec` of it with
  `Code Signature Invalid`. That killed 25 processes on 2026-08-06. `codesign -v` returns 0 on the
  corrupted file — the artifact is fine, the kernel's cached signature is stale — so it cannot detect
  this class. Place a binary with `scripts/install-cli.sh` (stage in the destination dir, then
  `rename(2)`), never `cp`.
- **The build probably does not land in `target/`.** `CARGO_TARGET_DIR` redirects it and every agent
  session here sets it, so `<repo>/target/release/tcr` is usually a stale orphan from before that
  variable existed. Resolve the real path from `cargo metadata --format-version 1 --no-deps`
  (`.target_directory`) rather than writing it down.
- The running process can be several commits behind the source. `tcr status --json` reports the running
  build's SHA — check it before concluding a fix is live. "The fix is in `main`" and "the fix is in the
  process serving traffic" are routinely different facts.

## Branching

- **The primary checkout stays on `main`.** This is a convention, not something the repo checks — the
  pre-commit hook in `.githooks/` runs a `gitleaks` secret scan and a `cargo fmt --check` gate, and
  looks at no branch. Keeping to it is on you. Do feature work in a worktree:
  `git worktree add ~/worktrees/<name> -b <branch> main`.
- Branch **before** you start editing. Making changes in the primary checkout and then wanting a branch
  is a trap — a fresh worktree branches from `HEAD` without your uncommitted work, and any shared file
  (`Cargo.toml`) blocks a clean move.
- `main` requires a pull request, one approval, and the `ci` and `audit` checks. An admin exemption
  exists; using it is a deliberate decision, not a shortcut, and GitHub records it as
  `Bypassed rule violations` in the push output. Read that output — a push can succeed *and* have
  bypassed the rules.

## Commits

Conventional commits, **bare type only** — `fix:`, `feat:`, `docs:`, `refactor:`, `chore:`, `test:`.
A parenthesised scope (`fix(quota):`) is rejected by the commit gates. Put the scope in the body.

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
