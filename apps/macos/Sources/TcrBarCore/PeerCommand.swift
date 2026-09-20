import Foundation

// Moved out of `PanelV4/PeersTabV4.swift`.
//
// These are pure data (no SwiftUI, no `Tok`, no `V4`), and the test target
// links `TcrBarCore` alone (`Package.swift:39-43`). While they sat in the
// executable target, every assertion about them had to read the SOURCE as
// text; here they are values a test can build and drive.

// MARK: - Argv

/// Every `tcr peer …` the tab runs, as argv.
///
/// The same two rules as ``GroupCommand``: this app never writes the peers file
/// itself, and a switch's argv is **the state it moves to**, never the state it
/// is in (the mockup's rule 4). Both switches write one line of the peers
/// file in the operator's config directory, which the proxy re-reads on
/// mtime, so neither needs a restart.
public enum PeerCommand {
    public static func find(on: Bool) -> [String] { ["peer", "find", on ? "on" : "off"] }
    public static func share(on: Bool) -> [String] { ["peer", "share", on ? "on" : "off"] }
    /// `tcr peer pair <host:port>`, Trust. The six digits come from the
    /// handshake itself, so comparing them binds this connection rather than
    /// two pasted fingerprints.
    public static func pair(address: String) -> [String] { ["peer", "pair", address] }

    /// The same verb, told to print machine lines: `tcr peer pair <addr>
    /// --json`.
    ///
    /// The panel runs THIS one, through ``PeerPairRun``, because the pairing
    /// is one live handshake that asks a question on stdin halfway through and
    /// cannot be split into two invocations. ``pair(address:)`` above stays as
    /// the argv a person types in a terminal, and the two are one array plus a
    /// flag rather than two literals, so the verb cannot be renamed in one
    /// place only.
    public static func pairJSON(address: String) -> [String] { pair(address: address) + ["--json"] }
    // There is NO `confirm` argv here, and that is deliberate. A
    // `tcr peer confirm <peer> <code>` was declared for a pairing sheet and
    // the CLI has never had the verb: the compare happens inside
    // `tcr peer pair`, against a pending pairing that lives in THAT process's
    // memory, so a separate verb has nothing to confirm against
    // (`src/peer/pair.rs`'s `confirm`). `PeerCommandVerbTests` is what found
    // it, and it is the reason that test reads the clap definition instead of
    // a list kept here.
    public static let list = ["peer", "ls", "--json"]

    /// `tcr peer status --json`, the LIVE half of the tab.
    ///
    /// A second verb rather than more keys on ``list``, and the split is the
    /// one ``PeerListDocument/LivePeersRead`` explains: `peer ls` is a
    /// projection of two FILES and answers the same thing with no server
    /// running, while this one asks the RUNNING proxy what it has measured,
    /// the per-path round trip, what each Mac is serving right now, the token
    /// rate off the ledger. Folding the two into one verb would make a proxy
    /// that is down look like a peers file that is empty, which is the one
    /// outcome the tab forbids by name.
    ///
    /// An older `tcr` exits non-zero on it and the tab keeps its file half:
    /// ``PeerListDocument/LivePeersRead/unsupported``.
    public static let liveStatus = ["peer", "status", "--json"]

    /// `tcr peer network-key show`: says whether a key is set, never the key
    /// itself (`PeerNetworkKeyAction::Show`, `src/main.rs`). The one question
    /// ``PeerJoinLink/confirmationBody(for:hasExistingKey:)`` needs answered
    /// before a `tcr://peer/join` link's confirmation sheet can say whether
    /// pressing Join is a no-op on a fresh Mac or a replacement of a key
    /// other Macs still hold.
    public static let networkKeyShow = ["peer", "network-key", "show"]

    // MARK: Settings > Peers
    //
    // Three verbs the PANE runs and the tab does not, kept here with the rest
    // so there is one place that knows what this app asks `tcr` to do. The
    // pane used to spell each of them as a literal at its button, which is how
    // `peer join` shipped with no key on the end of it.

    /// `tcr peer invite --ttl 600 --uses 1`, one use, ten minutes, the
    /// mockup's own flags (`settings-peers.html:318`). Its STDOUT is the join
    /// key, which is why the pane reads it through
    /// ``PeerController/capture(_:into:)`` rather than firing and forgetting.
    ///
    /// **`--ttl` is `u32` SECONDS** (`PeerInviteArgs::ttl`, `src/main.rs`,
    /// `default_value_t = 600`), never a duration string: clap parses it with
    /// `str::parse::<u32>`, so `"10m"` refused every invite with "invalid
    /// digit found in string" and the pane never had a join key to show.
    public static let invite = ["peer", "invite", "--ttl", "600", "--uses", "1"]

    /// `tcr peer invite --sealed`, mode B's first paste: a one-time public
    /// key that names no address and grants nothing. Its stdout is an ask,
    /// which is why the pane reads it through ``PeerController/mintSealedInvite(into:)``
    /// rather than firing and forgetting, ``invite``'s own reason.
    public static let sealedInvite = ["peer", "invite", "--sealed"]

    /// `tcr peer invite --reply --stdin`, with the reply ON STDIN, opening a
    /// reply this Mac was handed back and joining immediately.
    ///
    /// A reply is a live answer to a live ask this Mac minted: it carries the
    /// friend's freshly-sealed join key, so it is a secret in the same shape
    /// a join key is, and it rides stdin for the same reason ``join(key:)``
    /// does.
    public static func openReply(_ reply: String) -> PeerSecretInvocation {
        PeerSecretInvocation(arguments: ["peer", "invite", "--reply", "--stdin"], stdin: reply)
    }

    /// `tcr peer join --stdin`, with the key ON STDIN, the other half, on the
    /// Mac that was given the key.
    ///
    /// The key is a secret: it is the one credential that turns an unknown Mac
    /// into a trusted one, and it is single-use for exactly that reason. In
    /// argv it is readable by every process on this Mac through `ps`, ends up
    /// in this process's own crash reports, and is the leak `src/main.rs:726`
    /// already refuses for `tcr login`'s token in as many words: "an argv
    /// value is visible in `ps` and lands in shell history, both worse leaks
    /// than a stdin prompt". So the pane pipes it instead.
    ///
    /// The argv and the bytes are ONE value (``PeerSecretInvocation``) rather
    /// than two arguments a caller pairs up: a `--stdin` flag with nothing fed
    /// to it hangs, and a key handed to the argv half is the leak this whole
    /// function exists to close.
    ///
    /// The parameter is named `key` because it started as one, but it now
    /// carries whatever a person pasted: a v1 or v2 key, a `tcr://` link, or
    /// an ask (`tcr-invite:v1:…`). `JoinInput` on the other end decides which
    /// one arrived; this factory does not need to know.
    public static func join(key: String) -> PeerSecretInvocation {
        PeerSecretInvocation(arguments: ["peer", "join", "--stdin"], stdin: key)
    }

    /// `tcr peer moved mint <peer>`, one link for one Mac this one already
    /// trusts, to be handed over in whatever chat the two people already use.
    ///
    /// The peer id is NOT a secret: it is what `tcr peer ls` prints in the
    /// first column and what the panel already draws, so it rides argv like
    /// every other verb's subject. ``moved(open:apply:)`` is the other half,
    /// and it is a different kind of value, which is why the two are separate
    /// factories rather than one with a flag.
    public static func moved(mint peer: String) -> [String] { ["peer", "moved", "mint", peer] }

    /// `tcr peer moved open --stdin [--yes]`, with the whole link on stdin.
    ///
    /// Returns the refusal rather than an invocation when the URL is not a
    /// moved link, so no caller can pipe an arbitrary URL into `tcr`.
    ///
    /// `apply` is the operator's answer, carried straight through: `false`
    /// reads the link and writes nothing, `true` keeps what the first run
    /// showed them.
    public static func moved(
        open link: URL, apply: Bool = false
    ) -> Result<PeerSecretInvocation, PeerMovedLink.Refusal> {
        PeerMovedLink.invocation(for: link, apply: apply)
    }

    /// `tcr peer id --regenerate --yes`, the pane asked first, so the CLI
    /// must not ask again. `--yes` is what makes the `confirmationDialog` the
    /// one and only confirmation; without it this button opens a prompt on a
    /// terminal nobody is looking at and appears to have done nothing.
    public static let regenerate = ["peer", "id", "--regenerate", "--yes"]

    /// `tcr peer graph --serve`, the mini mesh card's "Open full graph".
    ///
    /// The one verb this app runs that does NOT end: it binds loopback and
    /// keeps serving. ``PeerGraphLauncher`` launches it and lets it go rather
    /// than waiting on it, for the reason that type's own doc gives.
    public static let graphServe = ["peer", "graph", "--serve"]

    /// `tcr peer forget <peer>`, behind the pane's own confirm.
    public static func forget(peer: String) -> [String] { ["peer", "forget", peer] }
}

/// A `tcr` verb that must be fed a SECRET, carried together with the bytes it
/// is fed on stdin.
///
/// One value, not two parameters, because the pairing is the whole point. A
/// caller that can pass the argv and the secret separately can pass the secret
/// as an argument (which is the leak) or pass `--stdin` with nothing behind
/// it, which hangs on a pipe nobody writes to. Here the only way to build the
/// invocation is through a factory on ``PeerCommand`` that writes both halves.
///
/// ``secretIsInArgv`` is the property a test asserts against; it is not a
/// runtime guard, because there is no argv path left to guard.
public struct PeerSecretInvocation: Equatable {
    /// What the process is exec'd with. Never the secret.
    public let arguments: [String]
    /// What is written to the process's stdin, which is then closed.
    public let stdin: String

    public init(arguments: [String], stdin: String) {
        self.arguments = arguments
        self.stdin = stdin
    }

    /// Whether the secret leaked into argv, as a whole argument or inside
    /// one, since `--key=<secret>` is the same leak spelled differently.
    ///
    /// An empty secret answers `false` rather than matching every argument:
    /// there is nothing to leak, and `contains("")` is true of every string.
    public var secretIsInArgv: Bool {
        guard !stdin.isEmpty else { return false }
        return arguments.contains { $0.contains(stdin) }
    }

    /// The argv as one string, for the in-flight key and for an error
    /// message. Safe to show and to log: the secret is not in it.
    public var displayCommand: String { arguments.joined(separator: " ") }
}
