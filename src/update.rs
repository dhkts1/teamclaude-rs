//! `tcr update` — self-update, by rebuilding a checkout or by re-running the
//! published release installer.
//!
//! The act-how split mirrors the JS `updater.js`: when we can locate the git
//! checkout that `tcr` was built from (walk up from the running executable to
//! the first ancestor holding BOTH `.git` and `Cargo.toml`) we
//! `git -C <root> pull --ff-only` and, if that moved HEAD,
//! `cargo build --release` in the checkout. When the binary is an *installed*
//! copy outside any checkout, we fetch the release installer published with the
//! newest GitHub release and run it against the directory the running binary
//! actually lives in. Only the `.app`-bundled copy is notify-only: it is a copy
//! that its own installer owns.
//!
//! Two invariants, both from Principle 9 (don't clobber in-flight work):
//!   * `--ff-only` — NEVER `git reset --hard`. A dirty or diverged tree makes
//!     the pull fail loudly with a message that explains the fix; we never
//!     discard local commits or uncommitted edits to force an update through.
//!   * The running process keeps executing the OLD binary after the rebuild, so
//!     the output says "built — restart tcr", never "updated". Claiming a live
//!     update would be a lie.
//!
//! And one for the installed path: we do NOT hand-roll download-and-replace.
//! Overwriting a live binary in place is how you get a half-written executable
//! under a running process. The published installer stages into a temp dir
//! *inside* the destination and finishes with a same-filesystem `mv`, i.e. an
//! atomic `rename(2)`; reusing it means the atomic swap is the vendor's tested
//! code path, not ours.

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
    /// Installed outside any checkout — updated by re-running the release
    /// installer against the running binary's own directory.
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

// ---------------------------------------------------------------------------
// Installed copies: update via the published release installer.
// ---------------------------------------------------------------------------

/// `owner/repo` the releases are published under. Kept as one constant so the
/// API URL and the asset URL can never drift apart.
const RELEASE_REPO: &str = "dhkts1/teamclaude-rs";

/// The installer asset name `dist` publishes with every release.
const INSTALLER_ASSET: &str = "teamclaude-rs-installer.sh";

/// The environment variable that makes `dist`'s installer write the binary into
/// exactly one directory. Without an override the installer picks its own
/// default (`CARGO_HOME/bin`, `~/.local/bin`), so for a copy installed anywhere
/// else the update lands as a SECOND binary while the one on your PATH stays
/// stale — the exact drift `tcr update` exists to end.
///
/// It is deliberately NOT `CARGO_DIST_FORCE_INSTALL_DIR`. Read the published
/// v0.1.0 installer: that variable selects the **cargo-home** layout, which
/// appends `/bin` (`_install_dir="$_force_install_dir/bin"`), so pointing it at
/// the running binary's directory installs to `<dir>/bin/tcr` — a second copy,
/// the very failure being fixed. `<APP>_UNMANAGED_INSTALL` selects the **flat**
/// layout, `_install_dir="$_force_install_dir"` verbatim, and additionally sets
/// `NO_MODIFY_PATH=1` and skips the receipt/self-updater — correct for
/// replacing a binary that is already installed and already on PATH.
const UNMANAGED_INSTALL_ENV: &str = "TEAMCLAUDE_RS_UNMANAGED_INSTALL";

/// What the version comparison decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Running version equals the latest release — nothing to download.
    AlreadyCurrent,
    /// Running version is strictly ahead of the latest release (a local build
    /// from an unreleased commit). Installing would be a DOWNGRADE, so we stop.
    RunningNewer,
    /// A different — and not newer — version is published: install it.
    Install,
}

/// GitHub release tags are cut as `v<Cargo.toml version>`; strip the `v` (and
/// surrounding whitespace) so the tag and `CARGO_PKG_VERSION` are comparable.
pub fn normalize_version(v: &str) -> &str {
    v.trim().trim_start_matches('v')
}

/// The dotted numeric core of a version (`1.2.3-rc1` → `[1, 2, 3]`), or `None`
/// when any component is not a plain number. `None` means "cannot order these",
/// NOT "equal" — callers must treat it as unknown rather than as a match.
fn version_parts(v: &str) -> Option<Vec<u64>> {
    let core = v.split(['-', '+']).next()?;
    if core.is_empty() {
        return None;
    }
    core.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

/// Order two versions by their numeric cores, zero-padding the shorter one so
/// `0.2` and `0.2.0` compare equal. `None` when either side is unparseable.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (a, b) = (version_parts(a)?, version_parts(b)?);
    let width = a.len().max(b.len());
    let at = (0..width).map(|i| a.get(i).copied().unwrap_or(0));
    let bt = (0..width).map(|i| b.get(i).copied().unwrap_or(0));
    Some(at.cmp(bt))
}

/// Decide whether to install `latest_tag` over `current`.
///
/// `force` short-circuits every check — that is what `--force` is for, and it is
/// the escape hatch when a version string lies (a binary rebuilt from a dirty
/// tree still reports the released version).
///
/// An UNPARSEABLE version is deliberately not treated as a match: we install,
/// rather than silently skipping an update because we could not read a string.
pub fn decide_update(current: &str, latest_tag: &str, force: bool) -> UpdateDecision {
    if force {
        return UpdateDecision::Install;
    }
    let (current, latest) = (normalize_version(current), normalize_version(latest_tag));
    if current == latest {
        return UpdateDecision::AlreadyCurrent;
    }
    match compare_versions(current, latest) {
        Some(std::cmp::Ordering::Greater) => UpdateDecision::RunningNewer,
        _ => UpdateDecision::Install,
    }
}

/// The GitHub API URL for the newest release.
///
/// We ask the API rather than just fetching `/releases/latest/download/<asset>`
/// because the API is the only cheap way to learn the *version* before
/// downloading anything: the asset URL would make "already current" cost a full
/// installer download plus an install, on every run.
pub fn latest_release_api_url() -> String {
    format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest")
}

/// The installer asset URL for one specific tag.
///
/// Tag-pinned, not `/releases/latest/download/…`: we already resolved the tag
/// when we decided to update, and a release cut between those two requests
/// would otherwise install a version we never compared against.
pub fn installer_url_for_tag(tag: &str) -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/{tag}/{INSTALLER_ASSET}")
}

/// Extract `tag_name` from the GitHub "latest release" JSON.
pub fn parse_latest_tag(body: &str) -> anyhow::Result<String> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("the GitHub releases API did not return valid JSON")?;
    match json.get("tag_name").and_then(|v| v.as_str()) {
        Some(tag) if !tag.trim().is_empty() => Ok(tag.trim().to_string()),
        _ => bail!("the GitHub releases API response has no usable `tag_name` field"),
    }
}

/// The directory an update must write into: the one holding the RUNNING binary,
/// with symlinks resolved. Resolving matters — when the PATH entry is a symlink,
/// its own directory is not where the real binary lives, and installing there
/// would leave the link pointing at the old file.
pub fn install_dir_for_exe(exe: &Path) -> anyhow::Result<PathBuf> {
    let resolved = std::fs::canonicalize(exe)
        .with_context(|| format!("cannot resolve the running executable at {}", exe.display()))?;
    resolved
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no parent directory", resolved.display()))
}

/// Blocking HTTP GET returning the body bytes.
///
/// Runs on a DEDICATED thread with its own current-thread runtime: `run_update`
/// is called from inside `#[tokio::main]`, and `block_on` from a thread already
/// driving a runtime panics. A fresh thread has no runtime of its own, so this
/// is legal from either context.
fn fetch_bytes(url: &str, accept: &str) -> anyhow::Result<Vec<u8>> {
    let (url, accept) = (url.to_string(), accept.to_string());
    let worker = std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime for the download")?;
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                // GitHub's API rejects requests without a User-Agent.
                .user_agent(concat!("tcr/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .context("could not build the HTTP client")?;
            let response = client
                .get(&url)
                .header(reqwest::header::ACCEPT, accept)
                .send()
                .await
                .with_context(|| format!("GET {url} failed"))?;
            let status = response.status();
            if !status.is_success() {
                bail!("GET {url} returned HTTP {status}");
            }
            let body = response
                .bytes()
                .await
                .with_context(|| format!("could not read the response body of {url}"))?;
            Ok(body.to_vec())
        })
    });
    match worker.join() {
        Ok(result) => result,
        Err(_) => bail!("the download thread panicked"),
    }
}

/// Write the installer into a fresh private directory and mark it executable.
///
/// The directory is created 0700 and freshly named. `/tmp` is world-writable, so
/// a predictable path would let any local user pre-create it and swap the script
/// we are about to run as ourselves.
fn stage_installer(script: &[u8]) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("tcr-update-{}-{nanos}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("could not create the staging directory {}", dir.display()))?;

    let path = dir.join(INSTALLER_ASSET);
    std::fs::write(&path, script)
        .with_context(|| format!("could not write the installer to {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not make {} executable", path.display()))?;
    Ok(path)
}

/// Update an installed copy: resolve the latest release, compare, then download
/// and run the published installer against the running binary's own directory.
fn update_installed(force: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot determine the running executable's path")?;
    let install_dir = install_dir_for_exe(&exe)?;

    let api_url = latest_release_api_url();
    println!("tcr: checking {api_url}");
    let body = fetch_bytes(&api_url, "application/vnd.github+json")
        .context("could not reach the GitHub releases API — no update was attempted")?;
    let tag = parse_latest_tag(&String::from_utf8_lossy(&body))?;

    let current = env!("CARGO_PKG_VERSION");
    match decide_update(current, &tag, force) {
        UpdateDecision::AlreadyCurrent => {
            println!("tcr: already current — running {current}, latest release is {tag}.");
            return Ok(());
        }
        UpdateDecision::RunningNewer => {
            println!(
                "tcr: this build is {current}, ahead of the latest release {tag} — \
                 installing it would be a downgrade, so nothing was changed.\n\
                 tcr: re-run with `tcr update --force` if you really want {tag}."
            );
            return Ok(());
        }
        UpdateDecision::Install => {
            println!(
                "tcr: updating {current} → {tag} in {}",
                install_dir.display()
            );
        }
    }

    let url = installer_url_for_tag(&tag);
    let script = fetch_bytes(&url, "application/octet-stream")
        .with_context(|| format!("could not download the installer from {url}"))?;
    let staged = stage_installer(&script)?;

    // Stdio inherited so the installer's own progress and errors are the user's
    // progress and errors — we add nothing by capturing and re-printing them.
    let status = Command::new(&staged)
        .env(UNMANAGED_INSTALL_ENV, &install_dir)
        .status()
        .with_context(|| format!("failed to run the installer at {}", staged.display()))?;

    // Best effort, and loud when it fails: a leftover staging dir is harmless,
    // but silently swallowing the error would hide a filesystem problem.
    if let Some(dir) = staged.parent() {
        if let Err(err) = std::fs::remove_dir_all(dir) {
            eprintln!("tcr: could not clean up {}: {err}", dir.display());
        }
    }

    if !status.success() {
        bail!(
            "the {tag} installer exited with {status} — {} was NOT updated.",
            install_dir.display()
        );
    }

    println!(
        "tcr: installed {tag} into {}.\n\
         tcr: this process is still running {current} — restart tcr to run the new build.",
        install_dir.display()
    );
    Ok(())
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
        // An installed copy updates from the PUBLISHED RELEASE, not from a
        // source tree it has no reason to have. This arm used to print reinstall
        // instructions that assumed a checkout on the machine — for anyone who
        // installed from the release installer, that is advice they cannot
        // follow. Now it fetches the installer for the newest release and runs
        // it against the directory the running binary is in, so the update
        // replaces the copy actually on your PATH instead of adding a second.
        InstallKind::Installed => update_installed(force),
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
    use std::cmp::Ordering;
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

    /// The `AppBundle` arm tells the user to run `apps/macos/scripts/install.sh`.
    /// Naming a path that is not there is the same class of defect as printing
    /// a built-binary path that does not exist, so the recommendation is
    /// checked against the tree rather than trusted to stay true.
    #[test]
    fn the_recommended_app_installer_script_exists() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("apps")
            .join("macos")
            .join("scripts")
            .join("install.sh");
        assert!(
            script.is_file(),
            "run_update_with(AppBundle) recommends {}, which does not exist",
            script.display()
        );
    }

    // -- installed-copy update: version comparison ---------------------------

    /// The whole point of the check: the common case must cost one API call and
    /// no download at all.
    #[test]
    fn equal_versions_skip_the_download() {
        assert_eq!(
            decide_update("0.1.0", "v0.1.0", false),
            UpdateDecision::AlreadyCurrent
        );
        // The tag is normally `v`-prefixed, but a bare tag must behave the same.
        assert_eq!(
            decide_update("0.1.0", "0.1.0", false),
            UpdateDecision::AlreadyCurrent
        );
        // …and trailing whitespace from a sloppy tag must not read as a change.
        assert_eq!(
            decide_update("0.1.0", " v0.1.0\n", false),
            UpdateDecision::AlreadyCurrent
        );
    }

    #[test]
    fn a_newer_release_is_installed() {
        assert_eq!(
            decide_update("0.1.0", "v0.1.1", false),
            UpdateDecision::Install
        );
        assert_eq!(
            decide_update("0.1.0", "v0.2.0", false),
            UpdateDecision::Install
        );
        assert_eq!(
            decide_update("0.9.9", "v1.0.0", false),
            UpdateDecision::Install
        );
        // Numeric ordering, not lexicographic: "0.10.0" > "0.9.0".
        assert_eq!(
            decide_update("0.9.0", "v0.10.0", false),
            UpdateDecision::Install
        );
    }

    /// A local build from an unreleased commit must not be silently downgraded
    /// to the newest published tag.
    #[test]
    fn a_build_ahead_of_the_release_is_not_downgraded() {
        assert_eq!(
            decide_update("0.2.0", "v0.1.0", false),
            UpdateDecision::RunningNewer
        );
        assert_eq!(
            decide_update("0.10.0", "v0.9.0", false),
            UpdateDecision::RunningNewer
        );
        // Zero-padded compare: 0.2 and 0.2.0 are the same version.
        assert_eq!(
            decide_update("0.2", "v0.2.0", false),
            UpdateDecision::Install
        );
        assert_eq!(compare_versions("0.2", "0.2.0"), Some(Ordering::Equal));
    }

    /// `--force` is the escape hatch for a version string that lies (a binary
    /// rebuilt from a dirty tree still reports the released version), so it must
    /// bypass EVERY skip branch, not just the equal one.
    #[test]
    fn force_always_installs() {
        assert_eq!(
            decide_update("0.1.0", "v0.1.0", true),
            UpdateDecision::Install
        );
        assert_eq!(
            decide_update("0.2.0", "v0.1.0", true),
            UpdateDecision::Install
        );
        assert_eq!(
            decide_update("not-a-version", "v0.1.0", true),
            UpdateDecision::Install
        );
    }

    /// An unreadable version must NOT be mistaken for a match: failing to parse
    /// a string is not evidence that we are up to date.
    #[test]
    fn unparseable_versions_install_rather_than_skip() {
        assert_eq!(
            decide_update("0.1.0-dev", "v0.1.0", false),
            UpdateDecision::Install
        );
        assert_eq!(
            decide_update("main", "v0.1.0", false),
            UpdateDecision::Install
        );
        assert_eq!(
            decide_update("0.1.0", "nightly", false),
            UpdateDecision::Install
        );
        assert_eq!(compare_versions("main", "0.1.0"), None);
        assert_eq!(version_parts("0.1.x"), None);
        assert_eq!(version_parts("0.1.0-rc1"), Some(vec![0, 1, 0]));
    }

    /// The version this binary reports is what gets compared, so it has to be a
    /// version at all — the release tags are cut FROM it.
    #[test]
    fn the_crate_version_is_comparable() {
        assert!(
            version_parts(env!("CARGO_PKG_VERSION")).is_some(),
            "CARGO_PKG_VERSION {} has no numeric core, so decide_update can never \
             report AlreadyCurrent",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            decide_update(
                env!("CARGO_PKG_VERSION"),
                &format!("v{}", env!("CARGO_PKG_VERSION")),
                false
            ),
            UpdateDecision::AlreadyCurrent
        );
    }

    // -- installed-copy update: URLs and JSON --------------------------------

    #[test]
    fn asset_and_api_urls_are_built_from_one_repo_constant() {
        assert_eq!(
            latest_release_api_url(),
            "https://api.github.com/repos/dhkts1/teamclaude-rs/releases/latest"
        );
        assert_eq!(
            installer_url_for_tag("v0.1.0"),
            "https://github.com/dhkts1/teamclaude-rs/releases/download/v0.1.0/teamclaude-rs-installer.sh"
        );
        // Tag-pinned, NOT /releases/latest/download/…: a release cut between the
        // API query and the download would otherwise install a version we never
        // compared against.
        assert!(!installer_url_for_tag("v0.1.0").contains("/latest/"));
    }

    /// Measured against the published v0.1.0 installer, both directions:
    /// `TEAMCLAUDE_RS_UNMANAGED_INSTALL=<dir>` installed `<dir>/tcr`, while
    /// `CARGO_DIST_FORCE_INSTALL_DIR=<dir>` installed `<dir>/bin/tcr`. Pointing
    /// the latter at the running binary's directory would therefore create the
    /// second copy this whole path exists to prevent, so the variable name is
    /// pinned rather than left to look interchangeable.
    #[test]
    fn the_install_dir_override_is_the_flat_layout_variable() {
        assert_eq!(UNMANAGED_INSTALL_ENV, "TEAMCLAUDE_RS_UNMANAGED_INSTALL");
        assert_ne!(UNMANAGED_INSTALL_ENV, "CARGO_DIST_FORCE_INSTALL_DIR");
    }

    #[test]
    fn parse_latest_tag_reads_the_tag_name() {
        let body = r#"{"tag_name":"v0.1.0","name":"v0.1.0","assets":[{"name":"teamclaude-rs-installer.sh"}]}"#;
        assert_eq!(parse_latest_tag(body).unwrap(), "v0.1.0");
    }

    /// The release-query error path: every failure shape must surface as an
    /// error, never as a version we then compare against.
    #[test]
    fn parse_latest_tag_errors_are_not_swallowed() {
        assert!(parse_latest_tag(r#"{"message":"Not Found"}"#).is_err());
        assert!(parse_latest_tag(r#"{"tag_name":null}"#).is_err());
        assert!(parse_latest_tag(r#"{"tag_name":""}"#).is_err());
        assert!(parse_latest_tag(r#"{"tag_name":"   "}"#).is_err());
        assert!(parse_latest_tag(r#"{"tag_name":123}"#).is_err());
        assert!(parse_latest_tag("<html>502 Bad Gateway</html>").is_err());
        assert!(parse_latest_tag("").is_err());
    }

    // -- installed-copy update: destination ----------------------------------

    /// The update must land where the RUNNING binary is. With a symlink on PATH,
    /// that is the link's target directory — installing next to the link would
    /// leave the link pointing at the old file.
    #[test]
    fn install_dir_follows_a_symlinked_exe_to_the_real_binary() {
        use std::os::unix::fs::symlink;
        let base = scratch("install-dir");
        let real_dir = base.join("opt").join("tcr-0.1.0");
        fs::create_dir_all(&real_dir).unwrap();
        let real_exe = real_dir.join("tcr");
        fs::write(&real_exe, b"fake").unwrap();

        let bindir = base.join("bin");
        fs::create_dir_all(&bindir).unwrap();
        let link = bindir.join("tcr");
        symlink(&real_exe, &link).unwrap();

        assert_eq!(
            install_dir_for_exe(&link).unwrap(),
            fs::canonicalize(&real_dir).unwrap()
        );
        // A plain copied file (the common install shape) resolves to its own dir.
        assert_eq!(
            install_dir_for_exe(&real_exe).unwrap(),
            fs::canonicalize(&real_dir).unwrap()
        );

        fs::remove_dir_all(&base).ok();
    }

    /// An exe path that does not exist must error, not degrade into some default
    /// install directory that would write a second binary somewhere else.
    #[test]
    fn install_dir_errors_when_the_exe_cannot_be_resolved() {
        let base = scratch("install-dir-missing");
        assert!(install_dir_for_exe(&base.join("nope").join("tcr")).is_err());
        fs::remove_dir_all(&base).ok();
    }

    /// The staged installer must be executable and live in a private directory —
    /// the system temp root is world-writable.
    #[test]
    fn stage_installer_writes_a_private_executable_script() {
        use std::os::unix::fs::PermissionsExt as _;
        let staged = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();

        assert_eq!(fs::read(&staged).unwrap(), b"#!/bin/sh\nexit 0\n");
        assert_eq!(staged.file_name().unwrap(), INSTALLER_ASSET);
        let mode = fs::metadata(&staged).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o100, 0o100, "installer is not executable: {mode:o}");
        assert_eq!(mode & 0o077, 0, "installer is group/world accessible");

        let dir = staged.parent().unwrap();
        let dir_mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "staging dir is not private: {dir_mode:o}");

        fs::remove_dir_all(dir).ok();
    }

    /// Two stagings in the same process must not collide — a fixed path in a
    /// world-writable temp root is a script-swap window.
    #[test]
    fn stage_installer_uses_a_fresh_directory_each_time() {
        let a = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();
        let b = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();
        assert_ne!(a.parent(), b.parent());
        fs::remove_dir_all(a.parent().unwrap()).ok();
        fs::remove_dir_all(b.parent().unwrap()).ok();
    }
}
