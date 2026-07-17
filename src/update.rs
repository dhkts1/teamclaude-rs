//! `tcr update` — self-update by pulling the source checkout and rebuilding.
//!
//! The notify-vs-act split mirrors the JS `updater.js`: we only *act* when we
//! can locate the git checkout that `tcr` was built from (walk up from the
//! running executable to the first ancestor holding BOTH `.git` and
//! `Cargo.toml`). In that case we `git -C <root> pull --ff-only` and, if that
//! moved HEAD, `cargo build --release` in the checkout. If the binary lives
//! outside any checkout (an installed copy under `~/.cargo/bin`, say) we do NOT
//! try to self-replace — we just print the reinstall command and return.
//!
//! Two invariants, both from Principle 9 (don't clobber in-flight work):
//!   * `--ff-only` — NEVER `git reset --hard`. A dirty or diverged tree makes
//!     the pull fail loudly with a message that explains the fix; we never
//!     discard local commits or uncommitted edits to force an update through.
//!   * The running process keeps executing the OLD binary after the rebuild, so
//!     the output says "built — restart tcr", never "updated". Claiming a live
//!     update would be a lie.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context as _};

/// How this `tcr` binary was installed — determines act vs notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Built from a git checkout at this root (`.git` + `Cargo.toml` present).
    GitCheckout(PathBuf),
    /// Installed outside any checkout — we only notify, never self-replace.
    Installed,
}

/// Result of parsing `git pull` stdout — did HEAD actually move?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// Git reported the branch was already current — nothing to rebuild.
    AlreadyUpToDate,
    /// The pull fast-forwarded (or otherwise changed the tree) — rebuild.
    Updated,
}

/// Walk UP from `start` to the first ancestor directory containing BOTH a
/// `.git` entry and a `Cargo.toml`. Robust to the `target/release/tcr` build
/// layout and to a checkout that was moved after build time — unlike
/// `env!("CARGO_MANIFEST_DIR")`, which bakes in the compile-time path.
pub fn find_repo_root_from(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join(".git").exists() && dir.join("Cargo.toml").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// [`find_repo_root_from`] anchored at the running executable's path. The exe
/// path is CANONICALIZED first: `tcr` is normally invoked through a PATH symlink
/// (e.g. `~/.local/bin/tcr` → `…/target/release/tcr`), and `current_exe()` yields
/// that symlink path — walking up from it never reaches the checkout. Resolving
/// symlinks anchors the walk at the real binary under `target/release/`.
pub fn find_repo_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    find_repo_root_from(&exe)
}

/// Classify how the binary at `exe_path` was installed.
pub fn classify_install_from(exe_path: &Path) -> InstallKind {
    match find_repo_root_from(exe_path) {
        Some(root) => InstallKind::GitCheckout(root),
        None => InstallKind::Installed,
    }
}

/// Classify the running `tcr` binary. Falls back to [`InstallKind::Installed`]
/// if the executable path can't be resolved.
pub fn classify_install() -> InstallKind {
    match std::env::current_exe() {
        // Canonicalize so a PATH symlink (e.g. ~/.local/bin/tcr) resolves to the
        // real binary before we look for the checkout — otherwise a symlinked
        // invocation is misclassified as Installed and refuses to self-update.
        Ok(exe) => classify_install_from(&std::fs::canonicalize(&exe).unwrap_or(exe)),
        Err(_) => InstallKind::Installed,
    }
}

/// Classify `git pull` stdout. Git prints "Already up to date." verbatim when
/// there was nothing to fetch; any other body means HEAD moved.
pub fn parse_pull_stdout(stdout: &str) -> PullOutcome {
    if stdout.trim().contains("Already up to date.") {
        PullOutcome::AlreadyUpToDate
    } else {
        PullOutcome::Updated
    }
}

/// Pure argv for the pull — asserted in tests without spawning git.
pub fn git_pull_argv() -> [&'static str; 2] {
    ["pull", "--ff-only"]
}

/// Pure argv for the release build — asserted in tests without spawning cargo.
pub fn cargo_build_argv() -> [&'static str; 2] {
    ["build", "--release"]
}

/// `tcr update [--force]` — self-update entry point.
pub fn run_update(force: bool) -> anyhow::Result<()> {
    run_update_with(classify_install(), force)
}

/// [`run_update`] with the install kind injected, so tests can drive the
/// notify branch without touching the real filesystem or spawning git.
pub fn run_update_with(kind: InstallKind, force: bool) -> anyhow::Result<()> {
    match kind {
        InstallKind::GitCheckout(root) => update_checkout(&root, force),
        InstallKind::Installed => {
            println!(
                "tcr was not run from a git checkout, so it can't self-update.\n\
                 Reinstall from the source tree:\n\
                 \n    git -C <teamclaude-rs> pull --ff-only && cargo install --path <teamclaude-rs>\n\
                 \n(or `cargo build --release` and copy target/release/tcr onto your PATH)."
            );
            Ok(())
        }
    }
}

/// Pull + rebuild inside the located checkout. Stdio is inherited so the user
/// sees git/cargo progress live; git's stdout is additionally captured so we
/// can classify the up-to-date case and skip a needless rebuild.
fn update_checkout(root: &Path, force: bool) -> anyhow::Result<()> {
    println!("tcr: updating checkout at {}", root.display());

    let argv = git_pull_argv();
    // Capture stdout (to classify) but let stderr through so the user sees git's
    // progress / any error text live.
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(argv)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("failed to spawn `git` — is git on PATH?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Echo git's stdout ourselves since we captured it.
    print!("{stdout}");

    if !output.status.success() {
        bail!(
            "`git pull --ff-only` failed in {}.\n\
             This is intentional: tcr never runs `git reset --hard`, so a dirty or \
             diverged tree stops the update instead of discarding your work.\n\
             Fix it by hand — commit/stash local changes, or reconcile the divergence \
             (`git -C {} status`) — then re-run `tcr update`.",
            root.display(),
            root.display()
        );
    }

    match parse_pull_stdout(&stdout) {
        PullOutcome::AlreadyUpToDate if !force => {
            println!("tcr: already up to date — nothing to rebuild.");
            return Ok(());
        }
        PullOutcome::AlreadyUpToDate => {
            println!("tcr: already up to date, but --force given — rebuilding anyway.");
        }
        PullOutcome::Updated => {
            println!("tcr: pulled new commits — rebuilding.");
        }
    }

    let build_argv = cargo_build_argv();
    let status = Command::new("cargo")
        .args(build_argv)
        .current_dir(root)
        .status()
        .context("failed to spawn `cargo` — is cargo on PATH?")?;

    if !status.success() {
        bail!(
            "`cargo build --release` failed in {} — the new binary was NOT built.",
            root.display()
        );
    }

    let built = root.join("target").join("release").join("tcr");
    println!("tcr: built {}", built.display());
    println!(
        "tcr: this process is still running the OLD binary — restart tcr to run the new build."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique scratch dir under the system temp root, following the repo's
    /// pid-suffixed convention (no `tempfile` dev-dependency).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-update-test-{}-{}-{tag}",
            std::process::id(),
            // nanos disambiguate multiple scratch dirs in one process.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn find_repo_root_walks_up_to_git_and_cargo() {
        let root = scratch("root");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let nested = root.join("target").join("release");
        fs::create_dir_all(&nested).unwrap();
        let exe = nested.join("tcr");
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(find_repo_root_from(&exe).as_deref(), Some(root.as_path()));

        fs::remove_dir_all(&root).ok();
    }

    /// Regression (found by a live `tcr update`): `tcr` is invoked through a PATH
    /// symlink (~/.local/bin/tcr → …/target/release/tcr). Walking up from the
    /// SYMLINK path never reaches the checkout; canonicalizing first — what
    /// `find_repo_root`/`classify_install` now do — resolves to the real binary.
    /// The injected-path unit tests missed this because they never symlinked.
    #[test]
    fn find_repo_root_resolves_through_a_symlinked_exe() {
        use std::os::unix::fs::symlink;
        let base = scratch("symlink");
        let root = base.join("checkout");
        let nested = root.join("target").join("release");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let real_exe = nested.join("tcr");
        fs::write(&real_exe, b"fake").unwrap();

        // A PATH symlink in a sibling dir (no .git above it) → the real binary.
        let bindir = base.join("bin");
        fs::create_dir_all(&bindir).unwrap();
        let link = bindir.join("tcr");
        symlink(&real_exe, &link).unwrap();

        // The bug: walking up from the symlink path finds nothing.
        assert_eq!(find_repo_root_from(&link), None);
        // The fix: canonicalize first, then the walk reaches the checkout root.
        let resolved = fs::canonicalize(&link).unwrap();
        assert_eq!(
            find_repo_root_from(&resolved).map(|r| fs::canonicalize(r).unwrap()),
            Some(fs::canonicalize(&root).unwrap())
        );

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn find_repo_root_none_without_markers() {
        let root = scratch("bare");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let exe = nested.join("tcr");
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(find_repo_root_from(&exe), None);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn classify_install_git_checkout() {
        let root = scratch("classify-git");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let exe = root.join("target").join("release").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(
            classify_install_from(&exe),
            InstallKind::GitCheckout(root.clone())
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn classify_install_installed_without_repo() {
        let root = scratch("classify-installed");
        let exe = root.join("bin").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(classify_install_from(&exe), InstallKind::Installed);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_pull_stdout_already_up_to_date() {
        assert_eq!(
            parse_pull_stdout("Already up to date.\n"),
            PullOutcome::AlreadyUpToDate
        );
    }

    #[test]
    fn parse_pull_stdout_diffstat_is_updated() {
        let body =
            "Updating a1b2c3d..e4f5g6h\nFast-forward\n src/main.rs | 4 ++--\n 1 file changed\n";
        assert_eq!(parse_pull_stdout(body), PullOutcome::Updated);
    }

    #[test]
    fn argv_shapes_are_stable() {
        assert_eq!(git_pull_argv(), ["pull", "--ff-only"]);
        assert_eq!(cargo_build_argv(), ["build", "--release"]);
    }

    #[test]
    fn run_update_installed_notifies_without_spawning() {
        // The Installed kind must reach the notify branch and return Ok without
        // ever touching git/cargo or the real filesystem.
        assert!(run_update_with(InstallKind::Installed, false).is_ok());
        assert!(run_update_with(InstallKind::Installed, true).is_ok());
    }
}
