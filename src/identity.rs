//! Account identity helpers, ported 1:1 from `teamclaude/src/identity.js`.
//!
//! An OAuth account is identified by its Anthropic account UUID (the *person*)
//! plus the organization it is scoped to. The same email/person can belong to
//! multiple organizations — e.g. a corporate Pro org and a personal Max org —
//! each with its own OAuth token and quota. The org must therefore be part of
//! the identity; otherwise multi-org logins overwrite each other, removals match
//! the wrong entry, and token rotation persists onto the wrong account.
//!
//! The org discriminator prefers the org UUID but falls back to the org name
//! (the profile endpoint has always returned a name), so identity still works on
//! entries created before org UUIDs were stored.
//!
//! Backward compatibility: when the identity fields (`account_uuid`, `org_uuid`,
//! `org_name`) are all absent — the shape of every config written before this
//! change — every comparison falls back to name equality, so single-org
//! behaviour is byte-identical.

use crate::config::Account;

/// Stable org discriminator for an account record: org UUID, else org name, else
/// `None` (an empty string is treated as absent).
pub fn org_key(a: &Account) -> Option<&str> {
    org_key_of(a.org_uuid.as_deref(), a.org_name.as_deref())
}

/// [`org_key`] over the two fields directly, for callers holding a runtime row
/// rather than a config record (`AccountRuntime`, which carries the same pair).
/// One definition, because two copies of "which field wins" is exactly the drift
/// this module exists to prevent.
pub fn org_key_of<'a>(org_uuid: Option<&'a str>, org_name: Option<&'a str>) -> Option<&'a str> {
    org_uuid
        .filter(|s| !s.is_empty())
        .or_else(|| org_name.filter(|s| !s.is_empty()))
}

/// The account UUID of a record, when one is actually stored (an empty string is
/// treated as absent, exactly as [`org_key`] treats an empty org).
fn uuid_key(a: &Account) -> Option<&str> {
    a.account_uuid.as_deref().filter(|s| !s.is_empty())
}

/// Whether two account records refer to the same account+org.
///
/// - Both have an `account_uuid`: it must match. If both org keys are known they
///   must also match; but if either side's org is still unknown we treat them as
///   the same. This lets a freshly-profiled login backfill a legacy entry (which
///   has no stored org) instead of creating a duplicate. Once both sides carry
///   an org key, a *different* org is correctly seen as a distinct account.
/// - Otherwise (API-key accounts, or no UUID yet): fall back to matching by name.
pub fn same_identity(a: &Account, b: &Account) -> bool {
    match (uuid_key(a), uuid_key(b)) {
        (Some(ua), Some(ub)) => {
            if ua != ub {
                return false;
            }
            match (org_key(a), org_key(b)) {
                (Some(ka), Some(kb)) => ka == kb,
                _ => true,
            }
        }
        _ => a.name == b.name,
    }
}

/// Whether two records match with the org tolerance REMOVED: the account UUID is
/// known on both sides and equal, and the org discriminators are equal — both
/// known and the same, or both absent.
///
/// This exists only to break ties, and it is exactly [`same_identity`] minus its
/// one asymmetry. `same_identity` calls an unknown org a match so a
/// freshly-profiled login can backfill a legacy entry written before org UUIDs
/// were stored; the price is that such an entry then matches EVERY org of that
/// person. Under this comparison a record with an org matches only records with
/// the same org, and a record without one matches only records that likewise have
/// none — so in the two-org shape each side has exactly one strict partner, which
/// is what [`resolve`] needs to tell them apart.
///
/// Records with no UUID are never strict-equal. `same_identity` falls back to name
/// equality there, and two entries sharing a name are genuinely indistinguishable
/// — there is no stricter fact to prefer one by.
pub fn same_identity_strict(a: &Account, b: &Account) -> bool {
    matches!((uuid_key(a), uuid_key(b)), (Some(ua), Some(ub)) if ua == ub)
        && org_key(a) == org_key(b)
}

/// Which of a set of candidate records an identity resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one candidate is the answer, at this index.
    One(usize),
    /// No candidate carries that identity.
    None,
    /// Two or more candidates match and the tie cannot be broken on the stored
    /// identity alone.
    Many,
}

/// Resolve `target` to at most ONE of `candidates` — the single record a rotated
/// credential or a `disabled` flag may be written to.
///
/// One loose match is the answer. Several loose matches are the legacy two-org
/// shape far more often than a real ambiguity: an entry stored before org UUIDs
/// existed carries a UUID and no org, so `same_identity` matches it against every
/// org of that person. When exactly one of the tied candidates also matches
/// strictly ([`same_identity_strict`]) the rest matched only on the org tolerance,
/// so the strict one is the answer.
///
/// An unbreakable tie is [`Resolved::Many`] and every caller REFUSES rather than
/// guesses. Guessing is not a cosmetic error here: stamping account A's rotated
/// credential onto account B's record overwrites B's own single-use refresh token,
/// which then 400s (`invalid_grant`) on its next use and leaves B dead until it is
/// re-authed by hand.
pub fn resolve<'a, I>(candidates: I, target: &Account) -> Resolved
where
    I: IntoIterator<Item = (usize, &'a Account)>,
{
    let mut loose: Vec<usize> = Vec::new();
    let mut exact: Vec<usize> = Vec::new();
    for (index, candidate) in candidates {
        if same_identity(target, candidate) {
            loose.push(index);
            if same_identity_strict(target, candidate) {
                exact.push(index);
            }
        }
    }
    match (loose.as_slice(), exact.as_slice()) {
        ([], _) => Resolved::None,
        ([only], _) => Resolved::One(*only),
        (_, [only]) => Resolved::One(*only),
        _ => Resolved::Many,
    }
}

/// The email portion of a display name, stripping a trailing " (org)" suffix.
pub fn email_of(name: &str) -> &str {
    if name.ends_with(')') {
        if let Some(i) = name.rfind(" (") {
            return &name[..i];
        }
    }
    name
}

/// The three fields a user-supplied account query is matched against.
///
/// It exists so ONE resolution rule runs over both representations of the fleet:
/// the config file's [`Account`] records (what the CLI edits) and the running
/// proxy's in-memory rotation slots (`manager::AccountRuntime`, what the live
/// control endpoint mutates). Duplicating the rule instead is how a CLI and an
/// endpoint come to disagree about which account `disable alice` names — and the
/// endpoint's index is a rotation slot, so disagreeing there benches the wrong
/// account.
pub trait Queryable {
    fn query_name(&self) -> &str;
    fn query_org_name(&self) -> Option<&str>;
    fn query_org_uuid(&self) -> Option<&str>;
}

impl Queryable for Account {
    fn query_name(&self) -> &str {
        &self.name
    }
    fn query_org_name(&self) -> Option<&str> {
        self.org_name.as_deref()
    }
    fn query_org_uuid(&self) -> Option<&str> {
        self.org_uuid.as_deref()
    }
}

/// Indices of accounts matching `query` (exact name, else email), narrowed by
/// `org_filter` (org name exact, or org uuid exact/prefix). Caller decides on
/// 0/1/many.
pub fn match_accounts<T: Queryable>(
    accounts: &[T],
    query: &str,
    org_filter: Option<&str>,
) -> Vec<usize> {
    let mut matches: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.query_name() == query)
        .map(|(i, _)| i)
        .collect();
    if matches.is_empty() {
        matches = accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| email_of(a.query_name()) == query)
            .map(|(i, _)| i)
            .collect();
    }
    if let Some(f) = org_filter {
        matches.retain(|&i| {
            let a = &accounts[i];
            a.query_org_name().is_some_and(|n| n == f)
                || a.query_org_uuid()
                    .is_some_and(|u| u == f || u.starts_with(f))
        });
    }
    matches
}

/// What a user-supplied `(query, org)` resolved to.
///
/// Separate from [`Resolved`], which resolves a stored IDENTITY against records.
/// This one resolves a human's argument, so its ambiguous arm carries the
/// candidate NAMES: every caller has to put them in front of the person who
/// typed the query, or `--org` is unusable advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Exactly one account matched, at this index.
    One(usize),
    /// Nothing matched.
    None,
    /// Two or more matched; these are their names, in fleet order.
    Ambiguous(Vec<String>),
}

/// [`match_accounts`] collapsed to the 0 / 1 / many decision every caller makes.
pub fn match_one<T: Queryable>(accounts: &[T], query: &str, org_filter: Option<&str>) -> Match {
    let candidates = match_accounts(accounts, query, org_filter);
    match candidates.as_slice() {
        [] => Match::None,
        [only] => Match::One(*only),
        many => Match::Ambiguous(
            many.iter()
                .map(|&i| accounts[i].query_name().to_string())
                .collect(),
        ),
    }
}

/// Build a lightweight probe [`Account`] carrying only the identity fields, for
/// [`same_identity`] comparison against stored records (upsert / persist). The
/// non-identity fields are placeholders and never read by the identity helpers.
pub fn probe(
    name: &str,
    account_uuid: Option<String>,
    org_uuid: Option<String>,
    org_name: Option<String>,
) -> Account {
    Account {
        name: name.to_string(),
        account_type: "oauth".to_string(),
        account_uuid,
        org_uuid,
        org_name,
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        switch_threshold: None,
        disabled: None,
        groups: None,
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(
        name: &str,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        org_name: Option<&str>,
    ) -> Account {
        probe(
            name,
            account_uuid.map(str::to_string),
            org_uuid.map(str::to_string),
            org_name.map(str::to_string),
        )
    }

    #[test]
    fn org_key_prefers_uuid_then_name_then_none() {
        assert_eq!(
            org_key(&acct("a", None, Some("uuid-1"), Some("Acme"))),
            Some("uuid-1")
        );
        assert_eq!(org_key(&acct("a", None, None, Some("Acme"))), Some("Acme"));
        assert_eq!(org_key(&acct("a", None, None, None)), None);
        // Empty strings are treated as absent.
        assert_eq!(org_key(&acct("a", None, Some(""), Some(""))), None);
    }

    #[test]
    fn all_none_falls_back_to_name_equality() {
        // The current real-config shape: no identity fields. Match must reduce to
        // name equality so single-org behaviour is byte-identical.
        let a = acct("me@example.com", None, None, None);
        let b = acct("me@example.com", None, None, None);
        let c = acct("other@example.com", None, None, None);
        assert!(same_identity(&a, &b), "same name → same identity");
        assert!(!same_identity(&a, &c), "different name → distinct");
    }

    #[test]
    fn same_person_different_org_is_distinct_once_both_orgs_known() {
        let corp = acct(
            "me@example.com",
            Some("uuid-person"),
            Some("org-corp"),
            Some("Corp"),
        );
        let personal = acct(
            "me@example.com",
            Some("uuid-person"),
            Some("org-personal"),
            Some("Personal"),
        );
        assert!(
            !same_identity(&corp, &personal),
            "same email, same person, different org → two accounts"
        );
    }

    #[test]
    fn same_person_same_org_is_same() {
        let a = acct(
            "me@example.com",
            Some("uuid-person"),
            Some("org-corp"),
            Some("Corp"),
        );
        let b = acct(
            "me@example.com",
            Some("uuid-person"),
            Some("org-corp"),
            Some("Corp"),
        );
        assert!(same_identity(&a, &b));
    }

    #[test]
    fn different_person_never_same_even_with_matching_name() {
        let a = acct(
            "shared@example.com",
            Some("uuid-a"),
            Some("org"),
            Some("Org"),
        );
        let b = acct(
            "shared@example.com",
            Some("uuid-b"),
            Some("org"),
            Some("Org"),
        );
        assert!(!same_identity(&a, &b), "different account_uuid → distinct");
    }

    #[test]
    fn legacy_entry_backfills_instead_of_duplicating() {
        // A freshly-profiled login (full identity) meeting a legacy entry that
        // has the uuid but no stored org: unknown-org side → treat as same, so
        // the login backfills the org onto the legacy entry rather than adding a
        // duplicate.
        let legacy = acct("me@example.com", Some("uuid-person"), None, None);
        let fresh = acct(
            "me@example.com",
            Some("uuid-person"),
            Some("org-corp"),
            Some("Corp"),
        );
        assert!(
            same_identity(&legacy, &fresh),
            "unknown org on one side → same"
        );
        assert!(same_identity(&fresh, &legacy), "symmetric");
    }

    #[test]
    fn a_strict_match_drops_the_unknown_org_tolerance() {
        let full = acct("me@example.com", Some("u1"), Some("org-a"), Some("Corp"));
        let legacy = acct("me@example.com", Some("u1"), None, None);
        let other_org = acct(
            "me@example.com",
            Some("u1"),
            Some("org-b"),
            Some("Personal"),
        );
        let no_uuid = acct("me@example.com", None, None, None);

        assert!(same_identity_strict(&full, &full.clone()));
        assert!(
            !same_identity_strict(&full, &legacy),
            "an unknown org on ONE side is a loose match only — that is the tolerance"
        );
        assert!(
            same_identity(&full, &legacy),
            "…and loosely they are still the same, which is what backfill needs"
        );
        assert!(
            same_identity_strict(&legacy, &legacy.clone()),
            "unknown on BOTH sides is agreement, not tolerance: the org keys are equal"
        );
        assert!(!same_identity_strict(&full, &other_org));
        assert!(
            !same_identity_strict(&no_uuid, &no_uuid.clone()),
            "no UUID is never strict — two same-named entries are indistinguishable"
        );
    }

    /// The legacy two-org shape, which is the whole reason `resolve` prefers the
    /// strict match: entry `{uuid, org-a}` and entry `{uuid}` (written before org
    /// UUIDs were stored) are TWO REAL ACCOUNTS, and `same_identity` matches
    /// EITHER target against both of them. Each target has exactly one strict
    /// partner, so both resolve — refusing them was what left neither benchable.
    #[test]
    fn resolve_breaks_the_legacy_tie_in_both_directions() {
        let candidates = [
            acct("me@example.com", Some("u1"), Some("org-a"), Some("Corp")),
            acct("me@example.com", Some("u1"), None, None),
        ];
        let indexed = || candidates.iter().enumerate();

        let corp = acct("me@example.com", Some("u1"), Some("org-a"), Some("Corp"));
        assert_eq!(
            resolve(indexed(), &corp),
            Resolved::One(0),
            "both candidates match loosely; only one carries the same org"
        );

        let legacy = acct("me@example.com", Some("u1"), None, None);
        assert_eq!(
            resolve(indexed(), &legacy),
            Resolved::One(1),
            "a target with no org resolves to the candidate that likewise has none"
        );
    }

    /// The tolerance is only dropped where dropping it decides something. One
    /// pre-org entry against a person who really is in two orgs stays a refusal:
    /// neither candidate shares the target's (absent) org key, so nothing is
    /// stricter and the tie is real.
    #[test]
    fn resolve_still_refuses_when_the_strict_pass_decides_nothing() {
        let candidates = [
            acct("me@example.com", Some("u1"), Some("org-a"), Some("Corp")),
            acct(
                "me@example.com",
                Some("u1"),
                Some("org-b"),
                Some("Personal"),
            ),
        ];
        let legacy = acct("me@example.com", Some("u1"), None, None);
        assert_eq!(
            resolve(candidates.iter().enumerate(), &legacy),
            Resolved::Many
        );
    }

    #[test]
    fn resolve_reports_none_one_and_an_unbreakable_tie() {
        let one = [acct("me@example.com", None, None, None)];
        assert_eq!(
            resolve(
                one.iter().enumerate(),
                &acct("me@example.com", None, None, None)
            ),
            Resolved::One(0)
        );
        assert_eq!(
            resolve(
                one.iter().enumerate(),
                &acct("nobody@example.com", None, None, None)
            ),
            Resolved::None
        );

        // Two same-named entries with no UUID: nothing stored distinguishes them.
        let twins = [
            acct("me@example.com", None, None, None),
            acct("me@example.com", None, None, None),
        ];
        assert_eq!(
            resolve(
                twins.iter().enumerate(),
                &acct("me@example.com", None, None, None)
            ),
            Resolved::Many
        );

        // Two candidates that BOTH match exactly are equally unbreakable.
        let duplicates = [
            acct("me@example.com", Some("u1"), Some("org-a"), None),
            acct("me@example.com", Some("u1"), Some("org-a"), None),
        ];
        assert_eq!(
            resolve(
                duplicates.iter().enumerate(),
                &acct("me@example.com", Some("u1"), Some("org-a"), None)
            ),
            Resolved::Many
        );
    }

    #[test]
    fn email_of_strips_org_suffix() {
        assert_eq!(email_of("me@example.com (Acme)"), "me@example.com");
        assert_eq!(email_of("me@example.com"), "me@example.com");
        // Only a trailing " (…)" is stripped, not a mid-string parenthesis.
        assert_eq!(email_of("weird (x) name"), "weird (x) name");
    }

    #[test]
    fn match_accounts_exact_name_then_email_then_org() {
        let accounts = vec![
            acct(
                "me@example.com (Corp)",
                Some("u1"),
                Some("org-corp"),
                Some("Corp"),
            ),
            acct(
                "me@example.com (Personal)",
                Some("u1"),
                Some("org-pers"),
                Some("Personal"),
            ),
            acct("other@example.com", None, None, None),
        ];

        // Exact display-name wins outright.
        assert_eq!(
            match_accounts(&accounts, "me@example.com (Corp)", None),
            vec![0]
        );

        // No exact name → email match finds both org variants.
        assert_eq!(
            match_accounts(&accounts, "me@example.com", None),
            vec![0, 1]
        );

        // Email + org filter narrows to one (by org name).
        assert_eq!(
            match_accounts(&accounts, "me@example.com", Some("Personal")),
            vec![1]
        );
        // Org filter by uuid prefix.
        assert_eq!(
            match_accounts(&accounts, "me@example.com", Some("org-corp")),
            vec![0]
        );
        assert_eq!(
            match_accounts(&accounts, "me@example.com", Some("org-")),
            vec![0, 1],
            "shared uuid prefix keeps both"
        );

        // No match at all.
        assert!(match_accounts(&accounts, "nobody@example.com", None).is_empty());
    }
}
