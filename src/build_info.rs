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
