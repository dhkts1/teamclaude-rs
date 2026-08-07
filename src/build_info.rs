//! Which commit the running binary carries, and whether it still matches the
//! checkout you are looking at.
//!
//! # Why version skew needs a mechanism
//!
//! A `tcr` server runs for days from a binary on disk while its source checkout
//! moves underneath it. `tcr update` (see [`crate::update`]) pulls and rebuilds
//! but deliberately does NOT restart — a restart wipes the in-memory
//! session→account pin map and cold-starts every live session's prompt prefix,
//! the single most expensive event in this system, so it stays a human's
//! explicit choice. The consequence is that "the fix is in the source" and "the
//! fix is in the process serving traffic" are routinely different facts, and
//! nothing in the output said so: the boot marker logged
//! `env!("CARGO_PKG_VERSION")`, the literal `0.1.0`, identical across every
//! build ever made. Proving which code a live server executed meant comparing
//! inodes with `lsof -p <pid>` by hand.
//!
//! This module makes the binary self-reporting. `build.rs` stamps [`SHA`],
//! [`DIRTY`] and [`BUILT_AT`] at compile time; the boot marker logs them, the
//! status endpoint ships them, and [`compare`] turns "server sha vs checkout
//! HEAD" into a single line `tcr status` can print.
//!
//! # What is proven, and what is not
//!
//! A matching sha is necessary but not sufficient. Two ways it can hold while
//! the running binary is NOT the checkout's code, both of which [`compare`]
//! reports rather than glossing:
//!
//! * The checkout has uncommitted changes — the ordinary way work happens, and
//!   exactly the case where "the fix is in the source" is most tempting to
//!   believe. Read live in [`read_checkout_state`], never from a build stamp.
//! * The binary was itself built from a dirty tree, so its sha describes the
//!   last commit, not the bytes that were compiled.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// What a build or checkout field reads when git could not be consulted: a
/// source tarball with no `.git`, no git on `PATH`, an unborn HEAD.
pub const UNKNOWN: &str = "unknown";

/// Shortest sha we will treat as comparable. Git's own `--short` floor is 7, and
/// below that a prefix match stops meaning anything.
const MIN_SHA_LEN: usize = 7;

/// Short commit sha this binary was compiled from, or [`UNKNOWN`].
pub const SHA: &str = env!("TCR_BUILD_SHA");

/// Whether TRACKED files carried uncommitted changes when `build.rs` last ran:
/// `"true"`, `"false"` or [`UNKNOWN`]. Stale by design in one direction — see
/// the limitation section in `build.rs`, and prefer [`read_checkout_state`] for
/// the live answer.
pub const DIRTY: &str = env!("TCR_BUILD_DIRTY");

/// When `build.rs` last stamped this binary (UTC), or [`UNKNOWN`].
pub const BUILT_AT: &str = env!("TCR_BUILD_TIME");

/// A binary's build provenance, as it crosses the wire in
/// [`crate::status::StatusPayload`].
///
/// `#[serde(default)]` on the container is the back-compat hinge: a NEW client
/// reading an OLD server (whose payload has no `build` object, or a partial one)
/// gets [`BuildInfo::default`] — every field [`UNKNOWN`] — and renders "cannot
/// tell", which is true. It must never silently render as "in sync".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BuildInfo {
    pub sha: String,
    /// Dirty at BUILD time. `Option`, not `bool`, because "the build could not
    /// tell" and "the tree was clean" must not render the same.
    pub dirty: Option<bool>,
    pub built_at: String,
}

impl Default for BuildInfo {
    /// Deliberately not the derived all-empty default: an absent build stamp is
    /// `unknown`, and an empty string would read as a sha.
    fn default() -> Self {
        Self {
            sha: UNKNOWN.to_string(),
            dirty: None,
            built_at: UNKNOWN.to_string(),
        }
    }
}

impl BuildInfo {
    /// This binary's own stamp.
    pub fn current() -> Self {
        Self {
            sha: SHA.to_string(),
            dirty: parse_tri_bool(DIRTY),
            built_at: BUILT_AT.to_string(),
        }
    }

    /// `"true"` / `"false"` / [`UNKNOWN`] — never collapses unknown into clean.
    pub fn dirty_str(&self) -> &'static str {
        tri_bool_str(self.dirty)
    }
}

/// `"true"` / `"false"` / [`UNKNOWN`] for a tri-state flag.
pub fn tri_bool_str(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => UNKNOWN,
    }
}

fn parse_tri_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// What a git checkout says about itself RIGHT NOW, read at status time rather
/// than baked in at build time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckoutState {
    /// `git rev-parse --short HEAD`, or [`UNKNOWN`].
    pub head: String,
    /// Uncommitted changes to tracked files. `None` = git would not say.
    pub dirty: Option<bool>,
    /// How many commits HEAD has moved past the running build. `None` when this
    /// checkout does not know that commit (never fetched, or a different repo),
    /// or when the shas already match.
    pub commits_ahead: Option<u64>,
}

/// The verdict of [`compare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skew {
    /// Same commit, checkout clean, binary built clean. Nothing to report.
    InSync,
    /// Something says the running binary is not this checkout's code.
    Drifted(Drift),
    /// Not comparable — a sha is [`UNKNOWN`] or malformed. Distinct from
    /// [`Skew::InSync`] on purpose: "I cannot tell" is not "it matches".
    Indeterminate { running: String, checkout: String },
}

/// The specific ways a running binary can fail to be the checkout's code. More
/// than one can hold at once, so they are flags rather than variants — the
/// report line prints all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// Sha the running server was built from.
    pub running: String,
    /// Sha this checkout's HEAD is at.
    pub checkout: String,
    /// Commits HEAD is ahead of `running`, when known.
    pub commits_ahead: Option<u64>,
    /// The checkout has uncommitted edits the running binary cannot contain.
    pub checkout_dirty: bool,
    /// The running binary was compiled from an uncommitted tree, so its sha
    /// describes the last commit rather than the code that was compiled.
    pub built_dirty: bool,
}

impl Drift {
    /// HEAD is at a different commit than the one the server is running.
    pub fn behind_head(&self) -> bool {
        !same_commit(&self.running, &self.checkout)
    }
}

/// Compare a running server's build stamp against a checkout's live state.
///
/// Pure — every git call happens in [`read_checkout_state`] — so the whole
/// decision table is unit-testable without a repository.
pub fn compare(running: &BuildInfo, checkout: &CheckoutState) -> Skew {
    if !is_comparable_sha(&running.sha) || !is_comparable_sha(&checkout.head) {
        return Skew::Indeterminate {
            running: running.sha.clone(),
            checkout: checkout.head.clone(),
        };
    }

    let behind = !same_commit(&running.sha, &checkout.head);
    // A tri-state that will not answer is treated as "no evidence of drift":
    // the sha comparison above is the load-bearing check, and an `unknown`
    // dirty flag must not manufacture a warning on every status call.
    let checkout_dirty = checkout.dirty.unwrap_or(false);
    let built_dirty = running.dirty.unwrap_or(false);

    if !behind && !checkout_dirty && !built_dirty {
        return Skew::InSync;
    }
    Skew::Drifted(Drift {
        running: running.sha.clone(),
        checkout: checkout.head.clone(),
        // Only meaningful when HEAD actually moved.
        commits_ahead: if behind { checkout.commits_ahead } else { None },
        checkout_dirty,
        built_dirty,
    })
}

impl Skew {
    /// The single line `tcr status` prints, or `None` when there is nothing to
    /// say. Every line leads with a stable lowercase token (`stale-server`,
    /// `uncommitted-in-checkout`, `dirty-build`, `build-skew-unknown`) followed
    /// by `key=value` pairs, so it is greppable, and closes with the sentence
    /// that explains what to do about it.
    ///
    /// It says "restart" and never offers to do it: a restart wipes ~450 session
    /// pins and cold-starts every live session's prompt prefix, so it belongs to
    /// a human at a quiet moment.
    pub fn report_line(&self) -> Option<String> {
        match self {
            Skew::InSync => None,
            Skew::Indeterminate { running, checkout } => Some(format!(
                "[tcr] note build-skew-unknown: running={running} checkout={checkout} \
                 — cannot tell whether the running server is this checkout's code."
            )),
            Skew::Drifted(drift) => Some(drift.report_line()),
        }
    }
}

impl Drift {
    fn report_line(&self) -> String {
        let keys = format!(
            "running={} checkout={} commits_ahead={} checkout_dirty={} built_dirty={}",
            self.running,
            self.checkout,
            match self.commits_ahead {
                Some(n) => n.to_string(),
                None => UNKNOWN.to_string(),
            },
            self.checkout_dirty,
            self.built_dirty,
        );
        if self.behind_head() {
            format!(
                "[tcr] WARNING stale-server: {keys} — the server is executing OLD code; \
                 restart tcr to adopt this checkout."
            )
        } else if self.checkout_dirty {
            format!(
                "[tcr] WARNING uncommitted-in-checkout: {keys} — the checkout has uncommitted \
                 changes the running server cannot contain; rebuild and restart tcr to adopt them."
            )
        } else {
            format!(
                "[tcr] note dirty-build: {keys} — this binary was compiled from an uncommitted \
                 tree, so a matching sha does not prove it matches the checkout."
            )
        }
    }
}

/// What the stand-down established about the incumbent's build, as a value
/// rather than as words in a line.
///
/// `main` turns this into the process's exit code, and an exit code must key on
/// a STRUCTURAL fact — grepping our own rendered sentence for "WARNING" would be
/// a gate that any rewording silently disarms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandDownBuild {
    /// Same commit, and neither side has any evidence of uncommitted code.
    InSync,
    /// Shas match, but at least one side is not provably that commit's code.
    DirtyBuild,
    /// The incumbent is executing a DIFFERENT commit than this binary.
    Stale,
    /// Not comparable: the incumbent would not report a build, or a sha is
    /// unknown. Never collapses into agreement.
    Unknown,
}

/// [`stand_down_build_report`]'s output: the verdict a caller can branch on and
/// the line a human reads, produced together so they can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandDownReport {
    pub verdict: StandDownBuild,
    pub line: String,
}

/// The build half of the message printed when startup finds a healthy proxy
/// already on the port and stands down instead of replacing it.
///
/// # Why this line has to exist
///
/// Standing down is the right default (a takeover costs every live session its
/// prompt cache) but it creates a trap the old kill-the-incumbent behaviour did
/// not have: `cargo build && tcr` used to GUARANTEE the new binary was serving.
/// Now it exits 0 with the OLD build still serving, and an exit 0 with no
/// complaint reads exactly like success — the "the fix is in the source" vs "the
/// fix is in the process serving traffic" confusion this whole module exists to
/// end, reintroduced by the default path. So the stand-down always says which
/// build keeps serving, and warns when that is not the one just run.
///
/// `running` is the incumbent's own stamp, read best-effort from its status
/// endpoint; `None` means it would not answer, which renders as
/// `build-skew-unknown` and never as agreement. Pure, so the whole decision table
/// is unit-testable without a server.
///
/// # Why `checkout` is an input and not a build stamp
///
/// `ours.dirty` comes from `build.rs`, which re-runs only when a git REF moves
/// (its own doc-comment states the limitation verbatim: "editing a tracked file
/// and rebuilding does NOT restamp"). So the ordinary loop — edit a file,
/// `cargo build --release`, run `tcr` — produces a binary whose `dirty` stamp
/// still reads `false`. Comparing that stamp against the incumbent's equally
/// stale one prints "build in sync" for a proxy that predates the edit, which is
/// the exact false all-clear this function exists to prevent.
///
/// [`compare`] does not have the bug because it pairs the running stamp with a
/// LIVE [`read_checkout_state`]. This takes the same input for the same reason:
/// uncommitted changes in the checkout we are standing in, at the commit this
/// binary was built from, mean neither side is provably this commit's code. When
/// no checkout is available (run from outside the source tree, no git) the
/// verdict degrades to the build stamps — which is what it was before, so the
/// degradation is never worse than the old behaviour.
pub fn stand_down_build_report(
    port: u16,
    ours: &BuildInfo,
    running: Option<&BuildInfo>,
    checkout: Option<&CheckoutState>,
) -> StandDownReport {
    let Some(running) = running else {
        return StandDownReport {
            verdict: StandDownBuild::Unknown,
            line: format!(
                "[tcr] note build-skew-unknown: this_binary={} but the proxy on :{port} did not \
                 report its build — it may be serving older code. `tcr status` reports the \
                 running build; `tcr --replace` restarts the port onto this binary.",
                ours.sha
            ),
        };
    };
    // The LIVE half. `Some(true)` only when git actually said the checkout we are
    // standing in has uncommitted tracked changes AND its HEAD is the commit this
    // binary claims — anywhere else the answer is about a different commit and
    // says nothing about whether this binary matches that one.
    let checkout_dirty_now = checkout
        .filter(|state| same_commit(&state.head, &ours.sha))
        .and_then(|state| state.dirty);
    // A dirty checkout at our own commit is evidence about THIS binary that its
    // build stamp structurally cannot carry, so it counts the same as the stamp.
    let ours_dirty = ours.dirty.unwrap_or(false) || checkout_dirty_now.unwrap_or(false);

    let keys = format!(
        "running={} this_binary={} built_dirty={} this_binary_dirty={} checkout_dirty={}",
        running.sha,
        ours.sha,
        running.dirty_str(),
        ours.dirty_str(),
        tri_bool_str(checkout_dirty_now),
    );
    if !is_comparable_sha(&running.sha) || !is_comparable_sha(&ours.sha) {
        return StandDownReport {
            verdict: StandDownBuild::Unknown,
            line: format!(
                "[tcr] note build-skew-unknown: {keys} — cannot tell whether the proxy on :{port} \
                 is running this binary's code. `tcr --replace` restarts the port onto this binary."
            ),
        };
    }
    if !same_commit(&running.sha, &ours.sha) {
        return StandDownReport {
            verdict: StandDownBuild::Stale,
            line: format!(
                "[tcr] WARNING stale-server: {keys} — the proxy on :{port} is NOT running this \
                 binary's code and will keep serving its own until it is restarted. \
                 `tcr --replace` takes the port over with this build."
            ),
        };
    }
    if running.dirty.unwrap_or(false) || ours_dirty {
        return StandDownReport {
            verdict: StandDownBuild::DirtyBuild,
            line: format!(
                "[tcr] note dirty-build: {keys} — the shas match but at least one side is not \
                 provably this commit's code (a build was compiled from an uncommitted tree, or \
                 this checkout has uncommitted changes now — a build stamp cannot see edits made \
                 since the last commit). Not proof the proxy on :{port} is running this binary."
            ),
        };
    }
    StandDownReport {
        verdict: StandDownBuild::InSync,
        line: format!(
            "[tcr] build in sync: {keys} — the proxy on :{port} is running this binary's commit."
        ),
    }
}

/// Two shas name the same commit when the shorter is a prefix of the longer.
///
/// Not string equality: `--short` abbreviates to the shortest UNAMBIGUOUS
/// length, which grows as a repo does, so the same commit legitimately renders
/// as 7 characters in one build and 8 in a later one. Equality would report
/// those as a stale server.
fn same_commit(a: &str, b: &str) -> bool {
    if !is_comparable_sha(a) || !is_comparable_sha(b) {
        return false;
    }
    let (a, b) = (a.trim(), b.trim());
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.starts_with(short)
}

/// A value we can compare as a sha: hex, and long enough that a prefix match
/// means something. Rejects [`UNKNOWN`] and the empty string, either of which
/// would otherwise prefix-match everything.
fn is_comparable_sha(value: &str) -> bool {
    let value = value.trim();
    value.len() >= MIN_SHA_LEN && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Locate the teamclaude-rs checkout containing `start`, if any.
///
/// Two steps, and the second is the important one. [`crate::update::find_repo_root_from`]
/// finds the nearest ancestor holding both `.git` and `Cargo.toml` — but
/// `tcr status` is routinely run from inside some OTHER repository, and
/// comparing this server's sha against an unrelated repo's HEAD would
/// manufacture a confident `stale-server` warning out of nothing. Confirming the
/// manifest names this package keeps the whole comparison silent when we are not
/// standing in tcr's own source.
pub fn find_tcr_checkout(start: &Path) -> Option<PathBuf> {
    let root = crate::update::find_repo_root_from(start)?;
    if manifest_names_this_package(&root.join("Cargo.toml")) {
        Some(root)
    } else {
        None
    }
}

/// Does this `Cargo.toml` declare `name = "<this package>"`? A line scan rather
/// than a TOML parse — the crate takes no toml dependency, and the question is
/// narrow enough that "some line assigns this exact name" answers it.
fn manifest_names_this_package(manifest: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return false;
    };
    text.lines().any(|line| {
        let Some(rest) = line.trim().strip_prefix("name") else {
            return false;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            return false;
        };
        value.trim().trim_matches('"') == env!("CARGO_PKG_NAME")
    })
}

/// Read `root`'s live state: HEAD, current dirtiness, and how far HEAD has moved
/// past `running_sha`.
///
/// Every field degrades to unknown independently — a git that will not answer
/// costs one field, never the whole report.
pub fn read_checkout_state(root: &Path, running_sha: &str) -> CheckoutState {
    let head = git_in(root, &["rev-parse", "--short", "HEAD"])
        .filter(|sha| is_comparable_sha(sha))
        .unwrap_or_else(|| UNKNOWN.to_string());

    // Same `--untracked-files=no` reasoning as build.rs: an untracked file only
    // reaches the compiler once a tracked file references it.
    let dirty =
        git_in(root, &["status", "--porcelain", "--untracked-files=no"]).map(|out| !out.is_empty());

    let commits_ahead = commits_ahead(root, running_sha);

    CheckoutState {
        head,
        dirty,
        commits_ahead,
    }
}

/// `git rev-list --count <running>..HEAD`, gated on this checkout actually
/// knowing that commit.
///
/// The `cat-file -e` probe first is what keeps the count honest: against a
/// commit this repo has never seen, `rev-list` fails and an ungated
/// `unwrap_or(0)` would print `commits_ahead=0` — a measured-looking zero for a
/// question that was never answered.
fn commits_ahead(root: &Path, running_sha: &str) -> Option<u64> {
    if !is_comparable_sha(running_sha) {
        return None;
    }
    git_in(
        root,
        &["cat-file", "-e", &format!("{running_sha}^{{commit}}")],
    )?;
    git_in(
        root,
        &["rev-list", "--count", &format!("{running_sha}..HEAD")],
    )?
    .parse::<u64>()
    .ok()
}

/// Run git inside `root`. `None` on a spawn failure or non-zero exit, so
/// "git would not answer" stays distinct from "git answered with nothing".
fn git_in(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(sha: &str, dirty: Option<bool>) -> BuildInfo {
        BuildInfo {
            sha: sha.to_string(),
            dirty,
            built_at: "2026-07-26T00:00:00Z".to_string(),
        }
    }

    fn checkout(head: &str, dirty: Option<bool>, ahead: Option<u64>) -> CheckoutState {
        CheckoutState {
            head: head.to_string(),
            dirty,
            commits_ahead: ahead,
        }
    }

    /// `build.rs` must emit all three variables unconditionally — `env!` would
    /// have failed the compile otherwise, so this asserts the values are also
    /// non-empty and, for the tri-state, one of the three legal tokens.
    #[test]
    fn the_build_stamped_every_field() {
        assert!(!SHA.is_empty(), "TCR_BUILD_SHA is stamped");
        assert!(!BUILT_AT.is_empty(), "TCR_BUILD_TIME is stamped");
        assert!(
            matches!(DIRTY, "true" | "false" | "unknown"),
            "TCR_BUILD_DIRTY is a tri-state token, got {DIRTY:?}"
        );
        // Whatever git said, the current stamp round-trips into a BuildInfo.
        let current = BuildInfo::current();
        assert_eq!(current.sha, SHA);
        assert_eq!(current.dirty_str(), DIRTY);
    }

    /// Same sha, clean both ends → the status output says NOTHING. A line on
    /// every invocation is a line nobody reads.
    #[test]
    fn matching_sha_is_quiet() {
        let skew = compare(
            &build("cd146ce", Some(false)),
            &checkout("cd146ce", Some(false), None),
        );
        assert_eq!(skew, Skew::InSync);
        assert_eq!(skew.report_line(), None, "an in-sync server prints nothing");
    }

    /// Git abbreviates to the shortest unambiguous length, which GROWS with the
    /// repo — so the same commit can be stamped 7 chars and read back 8. String
    /// equality would report that as a stale server on every status call.
    #[test]
    fn matching_sha_of_different_abbreviation_lengths_is_quiet() {
        assert_eq!(
            compare(
                &build("cd146ce", Some(false)),
                &checkout("cd146ce9", Some(false), None)
            ),
            Skew::InSync
        );
        // …and symmetrically, a longer stamp against a shorter HEAD.
        assert_eq!(
            compare(
                &build("cd146ce9", Some(false)),
                &checkout("cd146ce", Some(false), None)
            ),
            Skew::InSync
        );
    }

    /// Different sha → the loud line, naming both shas, the distance, and the
    /// action. This is the case that cost a full session to diagnose by hand.
    #[test]
    fn different_sha_warns_with_both_shas_and_the_action() {
        let skew = compare(
            &build("cd146ce", Some(false)),
            &checkout("9f3a1b2", Some(false), Some(3)),
        );
        let Skew::Drifted(ref drift) = skew else {
            panic!("a moved HEAD is drift, got {skew:?}");
        };
        assert!(drift.behind_head());

        let line = skew.report_line().expect("a stale server warns");
        assert!(line.contains("stale-server"), "greppable token: {line}");
        assert!(line.contains("running=cd146ce"), "names the server: {line}");
        assert!(
            line.contains("checkout=9f3a1b2"),
            "names the checkout: {line}"
        );
        assert!(
            line.contains("commits_ahead=3"),
            "names the distance: {line}"
        );
        assert!(line.contains("restart"), "names the action: {line}");
        assert_eq!(line.lines().count(), 1, "one line: {line}");
    }

    /// An unmeasurable distance stays `unknown`. A `0` here would read as
    /// "HEAD has not moved" for a question git never answered — the same class
    /// of false zero that hid the prompt-cache failure.
    #[test]
    fn unknown_distance_never_renders_as_zero() {
        let line = compare(
            &build("cd146ce", Some(false)),
            &checkout("9f3a1b2", Some(false), None),
        )
        .report_line()
        .expect("still stale");
        assert!(line.contains("commits_ahead=unknown"), "{line}");
        assert!(!line.contains("commits_ahead=0"), "{line}");
    }

    /// THE CASE THAT MATTERS MOST. Uncommitted edits are how work happens, and
    /// the sha matches all the way through them — so a sha-only check would
    /// print "in sync" for a server that provably cannot contain the fix you are
    /// looking at. That is the exact failure this module exists to end.
    #[test]
    fn uncommitted_checkout_warns_even_when_the_sha_matches() {
        let skew = compare(
            &build("cd146ce", Some(false)),
            &checkout("cd146ce", Some(true), None),
        );
        let Skew::Drifted(ref drift) = skew else {
            panic!("uncommitted edits are drift, got {skew:?}");
        };
        assert!(!drift.behind_head(), "HEAD itself did not move");
        assert!(drift.checkout_dirty);

        let line = skew.report_line().expect("warns");
        assert!(line.contains("uncommitted-in-checkout"), "{line}");
        assert!(line.contains("rebuild and restart"), "{line}");
    }

    /// A binary built from a dirty tree: the sha names the last commit, not the
    /// bytes compiled. Reported, but as a note — the checkout is clean, so there
    /// is nothing to adopt.
    #[test]
    fn a_dirty_build_is_noted_not_claimed_in_sync() {
        let skew = compare(
            &build("cd146ce", Some(true)),
            &checkout("cd146ce", Some(false), None),
        );
        assert_ne!(skew, Skew::InSync, "a dirty build is not proof of a match");
        let line = skew.report_line().expect("notes the dirty build");
        assert!(line.contains("dirty-build"), "{line}");
        assert!(line.contains("built_dirty=true"), "{line}");
    }

    /// An unknown sha degrades to "cannot tell" — never to InSync, and never to
    /// a stale warning. Covers the tarball build, the git-less container, and a
    /// NEW client reading an OLD server whose payload has no build stamp.
    #[test]
    fn unknown_sha_degrades_honestly() {
        for (running, head) in [
            (UNKNOWN, "cd146ce"),
            ("cd146ce", UNKNOWN),
            (UNKNOWN, UNKNOWN),
            ("", "cd146ce"),
            ("abc", "cd146ce"),     // too short to mean anything
            ("zzzzzzz", "cd146ce"), // not hex
        ] {
            let skew = compare(&build(running, None), &checkout(head, None, None));
            assert!(
                matches!(skew, Skew::Indeterminate { .. }),
                "{running:?} vs {head:?} is not comparable, got {skew:?}"
            );
            let line = skew.report_line().expect("says so out loud");
            assert!(line.contains("build-skew-unknown"), "{line}");
            assert!(
                !line.contains("stale-server"),
                "an unknown sha is not evidence of staleness: {line}"
            );
        }
    }

    /// The default a missing `build` object deserializes into. Every field
    /// unknown, and comparing it is Indeterminate rather than InSync.
    #[test]
    fn default_build_info_is_unknown_not_empty() {
        let default = BuildInfo::default();
        assert_eq!(default.sha, UNKNOWN);
        assert_eq!(default.dirty_str(), UNKNOWN);
        assert_eq!(default.built_at, UNKNOWN);
        assert!(matches!(
            compare(&default, &checkout("cd146ce", Some(false), None)),
            Skew::Indeterminate { .. }
        ));
    }

    /// THE TRAP THE STAND-DOWN DEFAULT CREATES. `cargo build && tcr` no longer
    /// guarantees the new binary serves; if the line said nothing, exit 0 would
    /// read as "my fix is live" while the old build kept serving. A different sha
    /// must be LOUD, name both builds, and name the escape hatch.
    #[test]
    fn standing_down_on_a_different_build_warns_loudly() {
        let report = stand_down_build_report(
            3456,
            &build("9f3a1b2", Some(false)),
            Some(&build("cd146ce", Some(false))),
            None,
        );
        assert_eq!(
            report.verdict,
            StandDownBuild::Stale,
            "the verdict, not just the wording, is what main exits on"
        );
        let line = report.line;
        assert!(line.contains("stale-server"), "greppable token: {line}");
        assert!(line.contains("running=cd146ce"), "names the server: {line}");
        assert!(
            line.contains("this_binary=9f3a1b2"),
            "names this build: {line}"
        );
        assert!(line.contains("--replace"), "names the escape hatch: {line}");
    }

    /// The incumbent would not report its build. That is "cannot tell", never
    /// agreement — and it still has to point at the two commands that resolve it.
    #[test]
    fn standing_down_with_no_answer_from_the_incumbent_says_so() {
        let report = stand_down_build_report(3456, &build("9f3a1b2", Some(false)), None, None);
        assert_eq!(report.verdict, StandDownBuild::Unknown);
        let line = report.line;
        assert!(line.contains("build-skew-unknown"), "{line}");
        assert!(
            !line.contains("in sync"),
            "an unanswered probe is not agreement: {line}"
        );
        assert!(line.contains("tcr status"), "{line}");
        assert!(line.contains("--replace"), "{line}");
        // The KEY SPELLING, not just the value. Every other branch emits
        // `this_binary=`; this branch used to write `this binary=` with a space,
        // so `grep 'this_binary='` over tcr's stderr silently dropped the one
        // case that most needs surfacing — the incumbent would not say what it
        // is running. The module's contract (see `Skew::report_line`) is that
        // every line is `token key=value …`, so the key is the assertion.
        assert!(
            line.contains("this_binary=9f3a1b2"),
            "the sha must carry the same greppable key as every other branch: {line}"
        );
        assert!(
            !line.contains("this binary="),
            "a space here breaks `grep this_binary=`: {line}"
        );
    }

    /// Matching shas, both clean: say so plainly. This is the ordinary case and it
    /// must not carry a warning, or the warning stops being read.
    #[test]
    fn standing_down_on_the_same_build_is_quiet() {
        let report = stand_down_build_report(
            3456,
            &build("cd146ce", Some(false)),
            Some(&build("cd146ce", Some(false))),
            // A checkout that is clean AT our commit corroborates the stamps.
            Some(&checkout("cd146ce", Some(false), None)),
        );
        assert_eq!(report.verdict, StandDownBuild::InSync);
        let line = report.line;
        assert!(line.contains("in sync"), "{line}");
        assert!(!line.contains("WARNING"), "{line}");
        assert!(!line.contains("stale-server"), "{line}");
        assert!(line.contains("checkout_dirty=false"), "{line}");
    }

    /// THE STALE-STAMP CASE, and the reason this function takes a live checkout.
    ///
    /// `build.rs` re-runs only when a git ref MOVES — its own doc says editing a
    /// tracked file and rebuilding does not restamp — so the ordinary
    /// edit → `cargo build --release` → `tcr` loop leaves BOTH `dirty` stamps
    /// reading `false` at the SAME sha. Comparing the two stamps against each
    /// other therefore prints "build in sync" for a proxy that predates the edit:
    /// the developer reads exit 0 plus "in sync" as "my fix is live" and debugs
    /// against a binary that cannot contain it. The live read is the only input
    /// that can see the edit, which is exactly why `compare` takes one too.
    #[test]
    fn a_live_dirty_checkout_beats_a_stale_clean_build_stamp() {
        let stamps_say_clean = || build("cd146ce", Some(false));
        // Control: with no live checkout to consult, the stamps are all we have
        // and the old (wrong-in-this-scenario) verdict is the honest best effort.
        assert_eq!(
            stand_down_build_report(3456, &stamps_say_clean(), Some(&stamps_say_clean()), None)
                .verdict,
            StandDownBuild::InSync,
            "without a checkout there is no evidence of the edit"
        );

        // The real loop: same commit, both stamps stale-clean, and the checkout
        // has uncommitted tracked changes RIGHT NOW.
        let report = stand_down_build_report(
            3456,
            &stamps_say_clean(),
            Some(&stamps_say_clean()),
            Some(&checkout("cd146ce", Some(true), None)),
        );
        assert_eq!(
            report.verdict,
            StandDownBuild::DirtyBuild,
            "an edited checkout at our own commit is not proof of a match"
        );
        assert!(
            !report.line.contains("in sync"),
            "the false all-clear this function exists to prevent: {}",
            report.line
        );
        assert!(report.line.contains("dirty-build"), "{}", report.line);
        assert!(
            report.line.contains("checkout_dirty=true"),
            "the live evidence has to be IN the line, or the verdict is unexplainable: {}",
            report.line
        );
    }

    /// The live read is scoped to OUR commit. A checkout sitting on a different
    /// commit says nothing about whether this binary matches the one it claims,
    /// and must not manufacture a `dirty-build` note out of an unrelated tree —
    /// `tcr status`/[`compare`] is what reports a moved HEAD.
    #[test]
    fn a_dirty_checkout_at_another_commit_does_not_taint_the_verdict() {
        let report = stand_down_build_report(
            3456,
            &build("cd146ce", Some(false)),
            Some(&build("cd146ce", Some(false))),
            Some(&checkout("9f3a1b2", Some(true), Some(3))),
        );
        assert_eq!(report.verdict, StandDownBuild::InSync);
        assert!(
            report.line.contains("checkout_dirty=unknown"),
            "unmeasured for THIS commit is `unknown`, never a bare `false`: {}",
            report.line
        );
    }

    /// A dirty build on either side: the shas match and the code still may not.
    /// Noted, not claimed in sync — the same honesty [`compare`] applies.
    #[test]
    fn standing_down_never_claims_a_dirty_build_is_in_sync() {
        for (ours, theirs) in [(Some(true), Some(false)), (Some(false), Some(true))] {
            let report = stand_down_build_report(
                3456,
                &build("cd146ce", ours),
                Some(&build("cd146ce", theirs)),
                Some(&checkout("cd146ce", Some(false), None)),
            );
            assert_eq!(report.verdict, StandDownBuild::DirtyBuild);
            let line = report.line;
            assert!(line.contains("dirty-build"), "{line}");
            assert!(!line.contains("in sync"), "{line}");
        }
    }

    /// An unknown sha from an old server degrades to "cannot tell" rather than
    /// manufacturing a stale-server warning out of a missing field.
    #[test]
    fn standing_down_on_an_unknown_sha_is_indeterminate_not_stale() {
        let report = stand_down_build_report(
            3456,
            &build("cd146ce", Some(false)),
            Some(&BuildInfo::default()),
            None,
        );
        assert_eq!(report.verdict, StandDownBuild::Unknown);
        let line = report.line;
        assert!(line.contains("build-skew-unknown"), "{line}");
        assert!(!line.contains("stale-server"), "{line}");
    }

    /// The git plumbing against a REAL repository — the half no amount of pure
    /// unit testing covers. Everything above drives [`compare`] with hand-built
    /// structs; this drives the actual `git -C` shell-outs.
    ///
    /// Not skippable-by-accident: a known [`SHA`] means git answered at build
    /// time in this very manifest directory, so it must answer now too. The
    /// early return fires only for a genuine no-git build (a source tarball),
    /// where there is nothing to test rather than something being hidden.
    #[test]
    fn read_checkout_state_reads_this_very_repository() {
        if SHA == UNKNOWN {
            return; // built without git; the fallbacks are covered above
        }
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = find_tcr_checkout(manifest)
            .expect("git stamped a sha from this directory, so it is our checkout");

        let state = read_checkout_state(&root, SHA);
        assert!(
            is_comparable_sha(&state.head),
            "a real checkout has a real HEAD, got {:?}",
            state.head
        );
        assert!(
            state.dirty.is_some(),
            "git answered about the working tree, so dirtiness is known"
        );
        // `commits_ahead` is deliberately NOT asserted: it is `None` whenever the
        // shas match, which is the ordinary state here.

        // And the end-to-end verdict is a real one — never "cannot tell".
        assert!(
            !matches!(
                compare(&BuildInfo::current(), &state),
                Skew::Indeterminate { .. }
            ),
            "both shas are real, so the comparison is decidable"
        );
    }

    /// The wrong-repo guard: `tcr status` run from another repository must not
    /// compare this server's sha against that repo's HEAD.
    #[test]
    fn find_tcr_checkout_rejects_a_foreign_repo() {
        let root = std::env::temp_dir().join(format!(
            "tcr-buildinfo-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"some-other-crate\"\n",
        )
        .unwrap();
        assert_eq!(
            find_tcr_checkout(&root),
            None,
            "another crate's checkout is not ours to compare against"
        );

        // The same directory, renamed to this package, IS ours.
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"{}\"\n", env!("CARGO_PKG_NAME")),
        )
        .unwrap();
        assert_eq!(find_tcr_checkout(&root).as_deref(), Some(root.as_path()));

        std::fs::remove_dir_all(&root).ok();
    }
}
