import Foundation

/// The short forms of an account's row name, one rule for every place the panel
/// shortens it.
///
/// A row name is an email, sometimes with a `/suffix` after the domain: a second
/// row for the same login in another organization reads
/// `henry@example.com/research`. The account card draws the part before the `@`
/// bold and the rest dim; the Sessions tab has room for one short word per
/// session. Both cut at the same `@` through here, so the two tabs cannot name
/// one account two ways.
public enum AccountName {
    /// `"henry10"`: everything before the first `@`, or the whole name when it
    /// has none.
    public static func localPart(_ name: String) -> String {
        guard let at = name.firstIndex(of: "@") else { return name }
        return String(name[name.startIndex..<at])
    }

    /// `"henry10"`, or `"henry/research"` for a row whose name carries a
    /// `/suffix` after the domain. The suffix is kept because it is the only
    /// thing that tells two rows of one login apart: cut at the `@` alone, both
    /// would read `henry`.
    public static func short(_ name: String) -> String {
        let local = localPart(name)
        guard name.contains("@"), let slash = name.lastIndex(of: "/") else { return local }
        let suffix = name[name.index(after: slash)...]
        return suffix.isEmpty ? local : "\(local)/\(suffix)"
    }
}
