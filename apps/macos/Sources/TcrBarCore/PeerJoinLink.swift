import CryptoKit
import Foundation

/// The `tcr://` share link, decision row 11's "one link a person can paste in
/// Slack or iMessage".
///
/// `tcr://peer/join?v=1&nk=<network key>[&jk=<join key>]`. TcrBar registers the
/// scheme in `CFBundleURLTypes` (`apps/macos/scripts/build-tcrbar.sh`) and
/// hands the WHOLE URL to `tcr peer join --stdin`; the CLI accepts the same
/// string (`src/main.rs:494-514`).
///
/// # Two rules, and both are the whole point of this type
///
/// **The link is a secret and never appears in argv.** `nk` is the office
/// network key and `jk` is a one-use join key: either one turns an unknown Mac
/// into a member of the mesh. argv is readable by every process on this Mac
/// through `ps` and lands in this process's crash reports, which is the leak
/// `src/main.rs:501-505` refuses in as many words. So the only thing this type
/// can build is a ``PeerSecretInvocation``, whose argv half is fixed.
///
/// **The WHOLE URL goes across, not its parts.** This is a correctness rule
/// rather than a convenience: the CLI owns
/// what a link means, which key sets what, that a spent `jk` still sets `nk`,
/// what `v=1` admits, and a panel that pulled the two query items out and
/// re-spelled them would be a second parser of the same string, free to
/// disagree about a link the CLI accepts.
public enum PeerJoinLink {
    /// The scheme, and it is NOT the app's own `tcrbar://`.
    ///
    /// Two schemes, two jobs: `tcrbar://check-for-updates` is the CLI asking
    /// this app to do something (`TcrBarApp.swift`), and `tcr://peer/join?…` is
    /// a person pasting a link that happens to open this app. Sharing one
    /// scheme would make an update check and a credential the same namespace.
    public static let scheme = "tcr"
    /// `tcr://peer/join`, `URL` parses `peer` as the host and `/join` as the
    /// path.
    public static let host = "peer"
    public static let path = "/join"

    /// Why a URL was not a join link. Named cases rather than `nil`, because
    /// the handler LOGS the refusal and "ignored a URL" with no reason is the
    /// line that makes a mistyped link look like a broken app.
    public enum Refusal: Error, Equatable, Sendable {
        case notOurScheme(String?)
        case notTheJoinPath(host: String?, path: String)
        /// No `nk` and no `jk`: a link that carries neither key sets nothing,
        /// so handing it to `tcr` would be a press that cannot work.
        case carriesNoKey
    }

    /// Turn a URL into the one invocation that may run it.
    ///
    /// Checks the shape and NOTHING about the keys' contents: whether a
    /// network key is 32 valid Crockford bytes is the CLI's question, and a
    /// panel that pre-judged it would refuse links a newer `tcr` accepts. The
    /// one thing checked is that at least one key is present, because a link
    /// with neither is a no-op with a spinner on it.
    public static func invocation(for url: URL) -> Result<PeerSecretInvocation, Refusal> {
        guard url.scheme?.lowercased() == scheme else {
            return .failure(.notOurScheme(url.scheme))
        }
        // Path compared with a trailing slash trimmed: `tcr://peer/join/` and
        // `tcr://peer/join` are the same link to anyone pasting one, and
        // refusing the first would be a refusal nobody could see the reason
        // for.
        let trimmedPath = url.path.hasSuffix("/") ? String(url.path.dropLast()) : url.path
        guard url.host?.lowercased() == host, trimmedPath == path else {
            return .failure(.notTheJoinPath(host: url.host, path: url.path))
        }
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let keyed = items.filter {
            ($0.name == "nk" || $0.name == "jk") && !($0.value ?? "").isEmpty
        }
        guard !keyed.isEmpty else { return .failure(.carriesNoKey) }
        return .success(
            PeerSecretInvocation(
                arguments: ["peer", "join", "--stdin"], stdin: url.absoluteString))
    }

    /// What may be written to a log about a link: its shape, and which keys it
    /// carried, never a key, and never the URL itself.
    ///
    /// `tcr://peer/join with a network key and a join key`. The handler has to
    /// say something (a URL that silently does nothing is indistinguishable
    /// from a broken handler) and it must not say the secret, so this is the
    /// only sentence it is given.
    public static func redacted(_ url: URL) -> String {
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let names = Set(items.filter { !($0.value ?? "").isEmpty }.map(\.name))
        var carried: [String] = []
        if names.contains("nk") { carried.append("a network key") }
        if names.contains("jk") { carried.append("a join key") }
        let what = carried.isEmpty ? "no key" : carried.joined(separator: " and ")
        return "tcr://\(url.host ?? "?")\(url.path) with \(what)"
    }

    /// A short, non-secret fingerprint of the credential a link carries, for a
    /// confirmation prompt an operator reads BEFORE it joins anything.
    ///
    /// `nk` when the link carries one, `jk` otherwise (``carriesNoKey`` is
    /// already refused by ``invocation(for:)`` before this is ever called on
    /// a link this app would act on). A hash, not an excerpt: printing eight
    /// characters of the key itself would still be printing key material,
    /// which is exactly the leak this whole type exists to avoid (see the
    /// type's own header, "the link is a secret and never appears in argv").
    /// SHA-256 is one-way, so this identifies the credential for a
    /// side-by-side compare (matching what the sender shows on their pairing
    /// screen) without handing over anything that could reconstruct it.
    ///
    /// **Not a parse of what the key MEANS.** Only the raw query VALUE is
    /// hashed; this still never asks whether 32 Crockford bytes are inside
    /// it, which is `tcr`'s own question and stays there.
    public static func fingerprint(for url: URL) -> String? {
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let value =
            items.first(where: { $0.name == "nk" && !($0.value ?? "").isEmpty })?.value
            ?? items.first(where: { $0.name == "jk" && !($0.value ?? "").isEmpty })?.value
        guard let value else { return nil }
        let digest = SHA256.hash(data: Data(value.utf8))
        let hex = digest.map { String(format: "%02x", $0) }.joined()
        // Eight bytes, grouped in fours, the same shape a six-digit pairing
        // code already trains an operator to read and compare at a glance:
        // `a1b2 c3d4 e5f6 a7b8`.
        let short = String(hex.prefix(16))
        return stride(from: 0, to: short.count, by: 4).map { offset -> String in
            let start = short.index(short.startIndex, offsetBy: offset)
            let end = short.index(start, offsetBy: 4, limitedBy: short.endIndex) ?? short.endIndex
            return String(short[start..<end])
        }.joined(separator: " ")
    }

    /// The confirmation sheet's body: what this link sets, and its
    /// fingerprint to compare against the sender's own screen.
    ///
    /// Decision row 11 ships `handleJoinLink` piping a clicked link straight
    /// to `tcr peer join --stdin` with NO confirmation at all: one link,
    /// clicked once, re-keys this Mac onto a stranger's mesh, and unlike
    /// Trust (which compares six digits on both ends before anything moves)
    /// a link has never asked the operator to look at anything. This is that
    /// look: named here so the confirmation sheet
    /// (``PeerJoinConfirmation`` in the app target, which owns the actual
    /// `NSAlert`) and any future caller say the same sentence.
    ///
    /// `hasExistingKey` names the ONE fact this app can state honestly
    /// without re-deriving the CLI's own answer: whether this Mac's peers
    /// file already carries a network key. It never claims the join will
    /// succeed or fail: `tcr peer join` (once it refuses an overwrite
    /// without `--replace`, the sibling fix to this one) is still the one
    /// place that decides that; it only tells the operator, before they
    /// press Join, that pressing it is not a no-op on a fresh Mac.
    public static func confirmationBody(for url: URL, hasExistingKey: Bool) -> String {
        let carries = redacted(url)
        let fingerprint = fingerprint(for: url) ?? "no readable key"
        var lines = [
            "\(carries).",
            "Fingerprint: \(fingerprint). Compare it against what the other Mac shows before "
                + "joining.",
        ]
        if hasExistingKey {
            lines.append(
                "This Mac already has a network key. Joining with this link may replace it.")
        }
        return lines.joined(separator: "\n\n")
    }

    /// Parses ``PeerCommand/networkKeyShow``'s stdout, `"peer network-key: \
    /// set"` or `"peer network-key: not-set"` (`src/main.rs`'s
    /// `run_peer_network_key`, the `Show` arm, verbatim). This is the CLI's
    /// own word, read and not re-derived: whether a key is set is a fact
    /// about the peers file this app never opens itself.
    ///
    /// `false` on anything else, a refused verb, an older `tcr`, output this
    /// build does not recognize, which is the same direction
    /// ``LendScope/parse(_:)`` and every other lenient decode in this app
    /// takes: the confirmation sheet then reads as "nothing to replace"
    /// rather than raising an alarm it cannot back up. `tcr peer join` itself
    /// is still the one place that actually refuses an overwrite.
    public static func networkKeyIsSet(output: String) -> Bool {
        output.trimmingCharacters(in: .whitespacesAndNewlines).hasSuffix(": set")
    }

    /// The sentence a refusal puts on screen or in the log.
    public static func sentence(for refusal: Refusal) -> String {
        switch refusal {
        case .notOurScheme(let scheme):
            return "not a tcr:// link (scheme \(scheme ?? "none")), so nothing was joined"
        case .notTheJoinPath(let host, let path):
            return "a tcr:// link this build does not handle (\(host ?? "?")\(path)); "
                + "the only one is tcr://peer/join"
        case .carriesNoKey:
            return "that link carries neither a network key nor a join key, so there is "
                + "nothing to join"
        }
    }
}

extension PeerCommand {
    /// `tcr peer join --stdin` with a whole `tcr://` link on stdin.
    ///
    /// The other half of ``join(key:)``, and deliberately a different factory:
    /// a bare join key and a link are two strings with two meanings, and the
    /// CLI tells them apart itself. Both land on stdin, because both are
    /// secrets.
    ///
    /// Returns the refusal rather than an invocation when the URL is not a
    /// join link, so a caller cannot pipe an arbitrary URL into `tcr`.
    public static func join(link: URL) -> Result<PeerSecretInvocation, PeerJoinLink.Refusal> {
        PeerJoinLink.invocation(for: link)
    }
}
