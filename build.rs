//! Stamps the binary with the commit it was built from.
//!
//! # Why
//!
//! `tcr` is run from a compiled binary while its source sits in a git checkout
//! that keeps moving, and `tcr update` deliberately rebuilds WITHOUT restarting
//! the server (a restart wipes the in-memory session→account pin map, the most
//! expensive event in this system). So three things drift apart routinely: the
//! running process, the on-disk binary, and the checkout. Proving which code a
//! live server is actually executing used to require `lsof -p <pid>` inode
//! comparisons by hand.
//!
//! `env!("CARGO_PKG_VERSION")` cannot do this job: it is the literal `0.1.0`
//! from `Cargo.toml` and is identical across every build ever made. A commit sha
//! captured at compile time is the smallest fact that actually distinguishes two
//! builds.
//!
//! # Contract
//!
//! Emits three `rustc-env` variables, ALWAYS — never conditionally, so
//! [`env!`] in `src/build_info.rs` compiles everywhere:
//!
//! * `TCR_BUILD_SHA`   — short commit sha, or `unknown`
//! * `TCR_BUILD_DIRTY` — `true` / `false` / `unknown` (tracked files only)
//! * `TCR_BUILD_TIME`  — UTC stamp, `unix:<secs>`, or `unknown`
//!
//! Missing git, a `.git`-less source tarball, or an unborn HEAD degrade to
//! `unknown` rather than failing the build: a source release must still compile.
//! No build-dependencies — everything here shells out.
//!
//! # Known limitation: `TCR_BUILD_DIRTY` and `TCR_BUILD_TIME` are as fresh as
//! the last re-run
//!
//! The rerun triggers below are the git ref files, so this script re-runs when
//! HEAD MOVES — not on every compile. That is deliberate: the timestamp changes
//! every run, so an unconditional re-run would force a full recrate recompile on
//! every `cargo check`/`cargo test`, which is a real dev-loop cost.
//!
//! The price is that editing a tracked file and rebuilding does NOT restamp:
//! `TCR_BUILD_DIRTY` can still read `false` for a binary compiled from an
//! edited tree, and `TCR_BUILD_TIME` names the last commit boundary rather than
//! the last compile. `TCR_BUILD_SHA` — the load-bearing field — is exact,
//! because it can only change when a ref file changes. Uncommitted drift is
//! caught at the other end instead: `tcr status` reads the checkout's CURRENT
//! dirtiness at status time (see `build_info::read_checkout_state`), where no
//! caching can stale it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What every field falls back to when git will not answer.
const UNKNOWN: &str = "unknown";

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| UNKNOWN.to_string());

    // `--untracked-files=no`: an untracked file cannot reach the compiler unless
    // some TRACKED file gains a `mod`/`include!` for it — which makes the tree
    // dirty anyway. Counting untracked scratch files as "dirty" would flag
    // nearly every working checkout and train the reader to ignore the flag.
    let dirty = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(out) if out.is_empty() => "false".to_string(),
        Some(_) => "true".to_string(),
        None => UNKNOWN.to_string(),
    };

    println!("cargo:rustc-env=TCR_BUILD_SHA={}", sanitize(&sha));
    println!("cargo:rustc-env=TCR_BUILD_DIRTY={}", sanitize(&dirty));
    println!("cargo:rustc-env=TCR_BUILD_TIME={}", sanitize(&build_time()));

    emit_rerun_triggers();
}

/// Run `git` in the package root — cargo's cwd for a build script.
fn git(args: &[&str]) -> Option<String> {
    run("git", args)
}

/// Run a program and return its trimmed stdout. `None` on a spawn failure or any
/// non-zero exit — the two cases that mean "it would not answer", as distinct
/// from "it answered with nothing", which is how `git status` reports a clean
/// tree.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A `cargo:` directive is newline-delimited, so a stray newline in a value
/// would be parsed as a new directive. Values here are hex/ASCII by
/// construction; this keeps a surprising one from corrupting the stream.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}

/// UTC build stamp. `date -u` is present on every platform this proxy targets;
/// the epoch-seconds form is the honest fallback when it is not, and beats
/// pulling in a date-formatting build-dependency for one string.
fn build_time() -> String {
    if let Some(stamp) = run("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]) {
        if !stamp.is_empty() {
            return stamp;
        }
    }
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("unix:{}", d.as_secs()),
        Err(_) => UNKNOWN.to_string(),
    }
}

/// Tell cargo to re-run this script when HEAD moves.
///
/// Three paths cover the ways a commit becomes visible:
///   * `<git-dir>/HEAD`               — checking out a different branch/commit.
///   * `<common-dir>/refs/heads/<br>` — committing on the current branch.
///   * `<common-dir>/packed-refs`     — the same, once refs have been packed.
///
/// The dirs are asked of git rather than assumed to be `.git/`, because in a
/// LINKED WORKTREE `.git` is a file, `<git-dir>` is `…/.git/worktrees/<name>`
/// (which holds that worktree's own HEAD), and refs live in the shared
/// `<common-dir>`. Hardcoding `.git/HEAD` would silently never fire there.
///
/// Only EXISTING paths are emitted: cargo re-runs a script unconditionally when
/// a declared rerun path is missing, which would reintroduce the
/// recompile-every-time cost the module docs describe. Emitting nothing at all
/// (no git) leaves cargo's default — re-run when any package file changes.
fn emit_rerun_triggers() {
    let Some(git_dir) = git(&["rev-parse", "--git-dir"]).map(resolve) else {
        return;
    };
    let common = git(&["rev-parse", "--git-common-dir"])
        .map(resolve)
        .unwrap_or_else(|| git_dir.clone());

    rerun_if_present(&git_dir.join("HEAD"));
    rerun_if_present(&common.join("packed-refs"));
    // Detached HEAD has no symbolic ref; the HEAD file above already moves.
    if let Some(sym) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        rerun_if_present(&common.join(sym));
    }
}

/// git may answer with a path relative to its cwd, which is this script's cwd.
fn resolve(path: String) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

fn rerun_if_present(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
