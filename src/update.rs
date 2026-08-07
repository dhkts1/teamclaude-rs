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
    /// Running from the CLI copy bundled inside a macOS `.app` (TcrBar bundles
    /// it at `Contents/MacOS/tcr`). The payload is the bundle root. We only
    /// notify: the bundled binary is a COPY, so rebuilding a checkout would not
    /// replace it, and `cargo install` would write a third competing artifact.
    AppBundle(PathBuf),
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
/// path is CANONICALIZED first because `tcr` on PATH is not always the real
/// binary: an install may place a symlink there (`~/.local/bin/tcr` →
/// `<target-dir>/release/tcr`, or → `…/TcrBar.app/Contents/MacOS/tcr`), and
/// `current_exe()` yields that symlink path — walking up from it never reaches
/// the checkout. Resolving symlinks anchors the walk at the real binary.
///
/// The current install on this machine copies the binary instead (a plain file
/// at `~/.local/bin/tcr`), for which canonicalization is a no-op and the walk
/// correctly finds no checkout. Both shapes ship, so the defence stays.
pub fn find_repo_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    find_repo_root_from(&exe)
}

/// Walk UP from `start` to the first ancestor directory that is a macOS
/// application bundle. `TcrBar.app` ships the CLI at `Contents/MacOS/tcr`, so a
/// binary running from inside a bundle must be recognised as such rather than
/// as a generic installed copy.
///
/// The name suffix ALONE is not the test, and that is the point. A checkout
/// living under a directory a human happened to name `myapp.app` matched the
/// old suffix-only check, and because [`classify_install_from`] tries the
/// bundle first, that false positive beat the `GitCheckout` classification that
/// would have worked — `tcr update` refused to self-update a perfectly ordinary
/// checkout. So the candidate must also actually CONTAIN `Contents/MacOS`,
/// which is what makes a directory a bundle rather than a name that ends in
/// `.app`.
///
/// Two smaller corrections in the same check: the suffix is compared
/// case-insensitively, because HFS+/APFS are case-insensitive by default and
/// `TcrBar.APP` is the same directory as `TcrBar.app` there; and the name is
/// read with `to_string_lossy` rather than `to_str`, so a path component that
/// is not valid UTF-8 anywhere above the binary no longer silently drops the
/// whole ancestor out of consideration.
pub fn find_app_bundle_from(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let named_app = dir.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.len() > ".app".len() && n.to_ascii_lowercase().ends_with(".app")
        });
        if named_app && dir.join("Contents").join("MacOS").is_dir() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Classify how the binary at `exe_path` was installed.
///
/// The bundle check runs FIRST: a `.app` nested inside a checkout (a build
/// artifact under the target dir, say) still holds a *copy* of the binary, so
/// pulling and rebuilding the checkout would not replace what is running.
pub fn classify_install_from(exe_path: &Path) -> InstallKind {
    if let Some(bundle) = find_app_bundle_from(exe_path) {
        return InstallKind::AppBundle(bundle);
    }
    match find_repo_root_from(exe_path) {
        Some(root) => InstallKind::GitCheckout(root),
        None => InstallKind::Installed,
    }
}

/// Classify the running `tcr` binary. Falls back to [`InstallKind::Installed`]
/// if the executable path can't be resolved.
pub fn classify_install() -> InstallKind {
    match std::env::current_exe() {
        // Canonicalize first: when the PATH entry is a symlink (~/.local/bin/tcr
        // pointing into a target dir or into TcrBar.app) the link path itself has
        // neither a checkout nor a `.app` above it, so an unresolved path is
        // misclassified as Installed and refuses to self-update. When the PATH
        // entry is a copied regular file — the current shape here —
        // canonicalization changes nothing and Installed is the right answer.
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

/// Pure argv for the target-directory query — asserted in tests without
/// spawning cargo.
pub fn cargo_metadata_argv() -> [&'static str; 4] {
    ["metadata", "--format-version", "1", "--no-deps"]
}

/// Extract `target_directory` from `cargo metadata` JSON.
///
/// Cargo does NOT always write to `<root>/target`: `CARGO_TARGET_DIR` (and
/// `build.target-dir` in a config file) relocate it, and on this machine it is
/// exported globally. Hardcoding `<root>/target/release/tcr` printed the path
/// of a stale orphan from before that export — a different file, with a
/// different hash, from the one the build had just produced. `cargo metadata`
/// is the only source that answers where cargo actually writes.
pub fn parse_target_directory(stdout: &str) -> anyhow::Result<PathBuf> {
    let json: serde_json::Value =
        serde_json::from_str(stdout).context("`cargo metadata` did not emit valid JSON")?;
    match json.get("target_directory").and_then(|v| v.as_str()) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => bail!("`cargo metadata` JSON has no usable `target_directory` field"),
    }
}

/// Absolute path of the release binary cargo just built in `root`.
///
/// What this DOES guarantee: the target directory is read from `cargo metadata`
/// rather than assumed to be `<root>/target`, and a failure to obtain it is
/// returned as an error instead of degrading into a hardcoded path.
///
/// What it does NOT guarantee: that the returned path exists. The final
/// `release/tcr` is still joined by hand, and `cargo metadata` reports no
/// target triple — so with `CARGO_BUILD_TARGET` set (or `[build] target` in a
/// config file) cargo writes `<target-dir>/<triple>/release/tcr` and the path
/// composed here is a file that is not there. Callers must therefore check for
/// existence before presenting it as fact; [`update_checkout`] does.
///
/// Resolving that properly means reading the artifact path out of
/// `cargo build --message-format=json`, which is a deliberate follow-up rather
/// than part of this change.
fn built_binary_path(root: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new("cargo")
        .args(cargo_metadata_argv())
        .current_dir(root)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("failed to spawn `cargo` — is cargo on PATH?")?;

    if !output.status.success() {
        bail!(
            "`cargo metadata` failed in {} — cannot determine the target directory.",
            root.display()
        );
    }

    let target_dir = parse_target_directory(&String::from_utf8_lossy(&output.stdout))?;
    Ok(target_dir.join("release").join("tcr"))
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
        InstallKind::AppBundle(bundle) => {
            println!(
                "tcr is running from the copy bundled inside {}, so it can't self-update.\n\
                 Rebuild and reinstall the app from the source tree:\n\
                 \n    git -C <teamclaude-rs> pull --ff-only && <teamclaude-rs>/apps/macos/scripts/install.sh\n\
                 \nDo NOT `cargo install --path` here: that writes ~/.cargo/bin/tcr, a third copy \
                 competing with the bundled one and with anything already on your PATH.",
                bundle.display()
            );
            Ok(())
        }
        // Reinstall via `scripts/install-cli.sh`, NOT `cargo install --path`.
        // The AppBundle arm above already refuses `cargo install` because it
        // writes a third competing copy at ~/.cargo/bin/tcr; this arm used to
        // recommend exactly that. README.md now installs with
        // `scripts/install-cli.sh`, which writes a regular file, so every new
        // user lands HERE — and was being routed straight into the artifact
        // drift the installer exists to prevent. The installer resolves the
        // build output from `cargo metadata`, refuses a stale binary, and
        // renames it into place instead of rewriting a live inode.
        InstallKind::Installed => {
            println!(
                "tcr was not run from a git checkout, so it can't self-update.\n\
                 Reinstall from the source tree:\n\
                 \n    git -C <teamclaude-rs> pull --ff-only \\\n      \
                 && cargo build --release --manifest-path <teamclaude-rs>/Cargo.toml \\\n      \
                 && <teamclaude-rs>/scripts/install-cli.sh\n\
                 \nDo NOT `cargo install --path` here: that writes ~/.cargo/bin/tcr, a copy \
                 competing with whatever is already on your PATH. install-cli.sh reads the build \
                 output location from `cargo metadata` (CARGO_TARGET_DIR may move it well outside \
                 the checkout) and refuses to install a binary that does not match HEAD."
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

    match built_binary_path(root) {
        // The path is composed, not observed (see `built_binary_path`), so it
        // is checked before it is announced. With a target triple configured
        // cargo writes one directory deeper and this file does not exist;
        // printing it anyway would name an artifact that is not there, which is
        // the same lie as reporting a build that did not land.
        Ok(built) if built.exists() => println!("tcr: built {}", built.display()),
        Ok(built) => println!(
            "tcr: the build succeeded, but the binary is not at {} — nothing is there.\n\
             tcr: that is expected when a target triple is configured \
             (CARGO_BUILD_TARGET, or `[build] target` in a cargo config): cargo then writes \
             <target-dir>/<triple>/release/tcr. Look one level down from {}.",
            built.display(),
            built
                .parent()
                .and_then(Path::parent)
                .unwrap_or(root)
                .display()
        ),
        // The build itself succeeded, so this is not fatal — but we refuse to
        // print a guessed path. Say plainly that we don't know where it landed.
        Err(err) => eprintln!(
            "tcr: the build succeeded, but locating the binary failed: {err:#}\n\
             tcr: run `cargo metadata --format-version 1 --no-deps` in {} to find the \
             target directory; the binary is at <target-dir>/release/tcr.",
            root.display()
        ),
    }
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
                .map_or(0, |d| d.as_nanos())
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

    /// Regression (found by a live `tcr update`): when the PATH entry is a
    /// SYMLINK into the build output (`~/.local/bin/tcr` → `…/release/tcr`),
    /// walking up from the symlink path never reaches the checkout;
    /// canonicalizing first — what `find_repo_root`/`classify_install` now do —
    /// resolves to the real binary. The injected-path unit tests missed this
    /// because they never symlinked. Installs also ship the binary as a copied
    /// regular file (the current shape on this machine), where canonicalization
    /// is a no-op; this test covers the symlink shape specifically.
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
    fn classify_install_app_bundle() {
        // TcrBar.app/Contents/MacOS/tcr — no checkout above it.
        let root = scratch("classify-bundle");
        let bundle = root.join("TcrBar.app");
        let exe = bundle.join("Contents").join("MacOS").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(
            classify_install_from(&exe),
            InstallKind::AppBundle(bundle.clone())
        );
        assert_eq!(
            find_app_bundle_from(&exe).as_deref(),
            Some(bundle.as_path())
        );

        fs::remove_dir_all(&root).ok();
    }

    /// A bundle nested INSIDE a checkout still holds a copy of the binary, so
    /// the bundle classification must win over `GitCheckout` — rebuilding the
    /// checkout would not replace what is running.
    #[test]
    fn classify_install_app_bundle_beats_enclosing_checkout() {
        let root = scratch("bundle-in-checkout");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let bundle = root.join("dist").join("TcrBar.app");
        let exe = bundle.join("Contents").join("MacOS").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(classify_install_from(&exe), InstallKind::AppBundle(bundle));

        fs::remove_dir_all(&root).ok();
    }

    /// A directory literally named `.app` is a dotfile, not a bundle.
    #[test]
    fn find_app_bundle_ignores_bare_dot_app_and_plain_dirs() {
        let root = scratch("bundle-negative");
        let exe = root.join(".app").join("bin").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(find_app_bundle_from(&exe), None);
        assert_eq!(classify_install_from(&exe), InstallKind::Installed);

        fs::remove_dir_all(&root).ok();
    }

    /// False positive the suffix-only check produced: an ordinary checkout that
    /// happens to live under a directory someone named `…app` is NOT a bundle,
    /// and misclassifying it beat the `GitCheckout` answer that would have
    /// worked (the bundle test runs first), so `tcr update` refused to update a
    /// checkout it could perfectly well have pulled and rebuilt. What makes a
    /// bundle is `Contents/MacOS`, not the name.
    #[test]
    fn checkout_under_an_app_named_directory_is_not_a_bundle() {
        let base = scratch("app-named-parent");
        let root = base.join("myapp.app").join("checkout");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let exe = root.join("target").join("release").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(find_app_bundle_from(&exe), None);
        assert_eq!(classify_install_from(&exe), InstallKind::GitCheckout(root));

        fs::remove_dir_all(&base).ok();
    }

    /// False negative in the other direction: APFS and HFS+ are case-insensitive
    /// by default, so `TcrBar.APP` names the same bundle as `TcrBar.app` and must
    /// classify the same way.
    #[test]
    fn find_app_bundle_matches_the_suffix_case_insensitively() {
        let root = scratch("bundle-uppercase");
        let bundle = root.join("TcrBar.APP");
        let exe = bundle.join("Contents").join("MacOS").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(
            find_app_bundle_from(&exe).as_deref(),
            Some(bundle.as_path())
        );
        assert_eq!(classify_install_from(&exe), InstallKind::AppBundle(bundle));

        fs::remove_dir_all(&root).ok();
    }

    /// A `.app` directory with no `Contents/MacOS` inside it is a name, not a
    /// bundle — and with no checkout above it the honest answer is `Installed`.
    #[test]
    fn app_named_directory_without_contents_macos_is_not_a_bundle() {
        let root = scratch("bundle-shell-only");
        let exe = root.join("Empty.app").join("tcr");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fake").unwrap();

        assert_eq!(find_app_bundle_from(&exe), None);
        assert_eq!(classify_install_from(&exe), InstallKind::Installed);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_update_app_bundle_notifies_without_spawning() {
        let kind = InstallKind::AppBundle(PathBuf::from("/Applications/TcrBar.app"));
        assert!(run_update_with(kind.clone(), false).is_ok());
        assert!(run_update_with(kind, true).is_ok());
    }

    /// The whole point of defect #1: with `CARGO_TARGET_DIR` set, cargo writes
    /// far outside the checkout, so the built path must come from metadata.
    #[test]
    fn parse_target_directory_reads_relocated_target_dir() {
        let json = r#"{"packages":[],"workspace_root":"/repo","target_directory":"/elsewhere/cargo-target","version":1}"#;
        assert_eq!(
            parse_target_directory(json).unwrap(),
            PathBuf::from("/elsewhere/cargo-target")
        );
        // …and the printed binary path is target_dir/release/tcr, NOT
        // <root>/target/release/tcr.
        let built = parse_target_directory(json)
            .unwrap()
            .join("release")
            .join("tcr");
        assert_eq!(built, PathBuf::from("/elsewhere/cargo-target/release/tcr"));
        assert!(!built.starts_with("/repo"));
    }

    #[test]
    fn parse_target_directory_errors_are_not_swallowed() {
        // Missing field, wrong type, empty string and non-JSON must all error
        // rather than degrade into a guessed path.
        assert!(parse_target_directory(r#"{"workspace_root":"/repo"}"#).is_err());
        assert!(parse_target_directory(r#"{"target_directory":null}"#).is_err());
        assert!(parse_target_directory(r#"{"target_directory":""}"#).is_err());
        assert!(parse_target_directory("error: no manifest found").is_err());
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
        assert_eq!(
            cargo_metadata_argv(),
            ["metadata", "--format-version", "1", "--no-deps"]
        );
    }

    /// The `Installed` arm tells the user to run `scripts/install-cli.sh`.
    /// Naming a path that is not there is the same class of defect as printing
    /// a built-binary path that does not exist, so the recommendation is
    /// checked against the tree rather than trusted to stay true.
    #[test]
    fn the_recommended_installer_script_exists() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("install-cli.sh");
        assert!(
            script.is_file(),
            "run_update_with(Installed) recommends {}, which does not exist",
            script.display()
        );
    }

    #[test]
    fn run_update_installed_notifies_without_spawning() {
        // The Installed kind must reach the notify branch and return Ok without
        // ever touching git/cargo or the real filesystem.
        assert!(run_update_with(InstallKind::Installed, false).is_ok());
        assert!(run_update_with(InstallKind::Installed, true).is_ok());
    }
}
