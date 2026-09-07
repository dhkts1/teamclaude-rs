//! Account identity helpers.
//!
//! Two different questions live here, and keeping them apart is the whole point
//! of the module.
//!
//! **Which row did the user mean?** [`match_one`] — an EXACT match on
//! `Account.name`, and nothing else. `Account.name` is unique across a config by
//! construction: `config::load` renames duplicates on sight ([`mint_name`] is
//! the rule), and a login mints a free name rather than colliding. So a name is
//! a complete answer to that question, and every earlier way of narrowing one —
//! matching the email portion of a name, then narrowing that by org — is
//! gone. Those existed only because a name could name two rows, and the bugs
//! they produced (a group label on the row nobody asked for, a copied token from
//! the wrong org) were bugs of a lookup that had to guess.
//!
//! **Is this the same account re-logging-in?** [`same_identity`] / [`resolve`] —
//! the Anthropic account UUID (the *person*) plus the organization it is scoped
//! to. The same person routinely belongs to several organizations, each with its
//! own OAuth token and quota, so the org has to be part of that comparison or
//! multi-org logins overwrite each other. This pair is still what `upsert_account`
//! and `save_account` decide a write's destination with. It is NOT a query key:
//! nothing takes a user's argument and resolves it this way.
//!
//! The org discriminator prefers the org UUID but falls back to the org name
//! (the profile endpoint has always returned a name), so identity still works on
//! entries created before org UUIDs were stored. When the identity fields are all
//! absent — the shape of every config written before they existed — the
//! comparison falls back to name equality, which unique names make exact.

use crate::config::Account;
use std::collections::HashSet;

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
    uuid_key_of(a.account_uuid.as_deref())
}

/// [`uuid_key`] over the field directly, for callers holding a runtime row
/// rather than a config record — the same split, and for the same reason, as
/// [`org_key`] and [`org_key_of`].
pub fn uuid_key_of(account_uuid: Option<&str>) -> Option<&str> {
    account_uuid.filter(|s| !s.is_empty())
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

/// The separator between the email and the org slug in a minted account name.
///
/// `/` is legal in every argv, can never occur inside an email address, and
/// reads as "this account, in that org". A name therefore splits back into its
/// two parts unambiguously — which is what [`email_of`] relies on.
pub const ORG_SEPARATOR: char = '/';

/// The email portion of an account name: everything before the first
/// [`ORG_SEPARATOR`], or the whole name when it carries no org suffix.
///
/// Only the OAuth `login_hint` uses this. Nothing resolves an account by it —
/// that was the old email-portion matching, and it is exactly what made
/// `henry@example.com` name two rows.
pub fn email_of(name: &str) -> &str {
    name.split_once(ORG_SEPARATOR)
        .map_or(name, |(email, _)| email)
}

/// An org name reduced to the suffix half of an account name: lower-cased, every
/// run of non-`[a-z0-9]` collapsed to a single `-`, and leading/trailing `-`
/// trimmed. `Henry Token` → `henry-token`.
///
/// Empty when the org name contains nothing usable (`"---"`, `"…"`); callers
/// treat that exactly as an absent org name and fall back to the org UUID.
pub fn org_slug(org_name: &str) -> String {
    let mut slug = String::with_capacity(org_name.len());
    for ch in org_name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// The suffix half of a name for an account in this org: the slugged org name,
/// or — when there is no usable org name — the first 8 characters of the org
/// UUID. `None` when neither is available, which is the only case a minted name
/// cannot carry an org suffix at all.
pub fn org_suffix(org_name: Option<&str>, org_uuid: Option<&str>) -> Option<String> {
    let from_name = org_name.map(org_slug).filter(|s| !s.is_empty());
    from_name.or_else(|| {
        org_uuid
            .map(|uuid| uuid.chars().take(8).collect::<String>())
            .filter(|s| !s.is_empty())
    })
}

/// Mint a UNIQUE account name for an account with this email, in this org.
///
/// The bare `email` when no name in `taken` is that email; otherwise
/// `email/<org-suffix>` ([`org_suffix`]); and if that is somehow taken too —
/// the same org twice cannot happen, since identity is `(account_uuid,
/// org_uuid)`, but a hand-edited config is not bound by that — `-2`, `-3`, and
/// so on until one is free.
///
/// This is ONE rule with two callers by design: a login mints through it, and
/// `config::load`'s duplicate-name migration renames through it. Two copies is
/// how a fleet ends up with a row the migration called `a/acme` and a re-login
/// calls something else, which puts the duplicate straight back.
pub fn mint_name(
    email: &str,
    org_name: Option<&str>,
    org_uuid: Option<&str>,
    taken: &HashSet<String>,
) -> String {
    if !taken.contains(email) {
        return email.to_string();
    }
    let base = match org_suffix(org_name, org_uuid) {
        Some(suffix) => format!("{email}{ORG_SEPARATOR}{suffix}"),
        // No org to name it by. Fall straight through to the numeric suffixes,
        // which are the only thing left that can make it unique.
        None => email.to_string(),
    };
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unbounded range always yields a free name")
}

/// The one field a user-supplied account query is matched against.
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
}

impl Queryable for Account {
    fn query_name(&self) -> &str {
        &self.name
    }
}

/// Indices of accounts whose name is EXACTLY `query`. Caller decides on 0/1/many.
///
/// Unique names make "many" unreachable through any path that went through
/// `config::load`; it survives as a return shape because a hand-edited file is
/// still a file, and refusing beats writing to whichever row came first.
pub fn match_accounts<T: Queryable>(accounts: &[T], query: &str) -> Vec<usize> {
    accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.query_name() == query)
        .map(|(i, _)| i)
        .collect()
}

/// What a user-supplied query resolved to.
///
/// Separate from [`Resolved`], which resolves a stored IDENTITY against records.
/// This one resolves a human's argument, so its ambiguous arm carries the
/// candidate NAMES — the operator has to be told which file to go fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Exactly one account matched, at this index.
    One(usize),
    /// Nothing matched.
    None,
    /// Two or more matched; these are their names, in fleet order. Only
    /// reachable on a config edited by hand behind the loader's back.
    Ambiguous(Vec<String>),
}

/// [`match_accounts`] collapsed to the 0 / 1 / many decision every caller makes.
pub fn match_one<T: Queryable>(accounts: &[T], query: &str) -> Match {
    let candidates = match_accounts(accounts, query);
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
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
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
    fn email_of_splits_at_the_org_separator() {
        assert_eq!(email_of("me@example.com/acme"), "me@example.com");
        assert_eq!(email_of("me@example.com"), "me@example.com");
        // A suffix carrying its own separator still yields the email half.
        assert_eq!(email_of("me@example.com/acme/eu"), "me@example.com");
        // A name that is not an email at all comes back whole.
        assert_eq!(email_of("work"), "work");
    }

    #[test]
    fn org_slug_lowercases_and_collapses_punctuation() {
        assert_eq!(org_slug("Henry Token"), "henry-token");
        assert_eq!(org_slug("ACME, Inc."), "acme-inc");
        assert_eq!(org_slug("  spaced  out  "), "spaced-out");
        assert_eq!(org_slug("already-slugged"), "already-slugged");
        // Nothing usable survives — the caller must fall back to the org uuid.
        assert_eq!(org_slug("---"), "");
        assert_eq!(org_slug(""), "");
    }

    #[test]
    fn org_suffix_prefers_the_name_then_eight_uuid_chars() {
        assert_eq!(
            org_suffix(Some("Henry Token"), Some("abcdefgh-1111")),
            Some("henry-token".to_string())
        );
        assert_eq!(
            org_suffix(None, Some("abcdefgh-1111")),
            Some("abcdefgh".to_string())
        );
        // An org name that slugs to nothing is treated as absent.
        assert_eq!(
            org_suffix(Some("---"), Some("abcdefgh-1111")),
            Some("abcdefgh".to_string())
        );
        assert_eq!(org_suffix(None, None), None);
    }

    fn taken(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn mint_name_takes_the_bare_email_when_it_is_free() {
        assert_eq!(
            mint_name(
                "me@example.com",
                Some("Acme"),
                Some("org-1"),
                &taken(&["other@example.com"])
            ),
            "me@example.com"
        );
    }

    #[test]
    fn mint_name_qualifies_with_the_org_when_the_email_is_taken() {
        assert_eq!(
            mint_name(
                "me@example.com",
                Some("Henry Token"),
                Some("org-1"),
                &taken(&["me@example.com"])
            ),
            "me@example.com/henry-token"
        );
    }

    #[test]
    fn mint_name_appends_a_counter_when_the_qualified_name_is_taken_too() {
        assert_eq!(
            mint_name(
                "me@example.com",
                Some("Acme"),
                Some("org-1"),
                &taken(&["me@example.com", "me@example.com/acme"])
            ),
            "me@example.com/acme-2"
        );
        assert_eq!(
            mint_name(
                "me@example.com",
                Some("Acme"),
                Some("org-1"),
                &taken(&[
                    "me@example.com",
                    "me@example.com/acme",
                    "me@example.com/acme-2",
                ])
            ),
            "me@example.com/acme-3"
        );
    }

    #[test]
    fn mint_name_with_no_org_at_all_falls_through_to_the_counter() {
        assert_eq!(
            mint_name("me@example.com", None, None, &taken(&["me@example.com"])),
            "me@example.com-2"
        );
    }

    #[test]
    fn match_accounts_is_exact_on_the_name_and_nothing_else() {
        let accounts = vec![
            acct("me@example.com", Some("u1"), Some("org-corp"), Some("Corp")),
            acct(
                "me@example.com/personal",
                Some("u1"),
                Some("org-pers"),
                Some("Personal"),
            ),
            acct("other@example.com", None, None, None),
        ];

        assert_eq!(match_accounts(&accounts, "me@example.com"), vec![0]);
        assert_eq!(
            match_accounts(&accounts, "me@example.com/personal"),
            vec![1]
        );
        // The email PORTION of a qualified name resolves nothing on its own —
        // the bare email is a different, and here a real, account.
        assert_eq!(match_accounts(&accounts, "personal"), Vec::<usize>::new());
        assert!(match_accounts(&accounts, "nobody@example.com").is_empty());
    }

    #[test]
    fn match_one_still_refuses_a_hand_edited_duplicate() {
        let accounts = vec![
            acct("me@example.com", Some("u1"), Some("org-a"), Some("A")),
            acct("me@example.com", Some("u2"), Some("org-b"), Some("B")),
        ];
        assert_eq!(
            match_one(&accounts, "me@example.com"),
            Match::Ambiguous(vec!["me@example.com".to_string(); 2])
        );
    }
}
