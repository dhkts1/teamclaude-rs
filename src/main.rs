//! `tcr` — the teamclaude-rs binary.
//!
//! Boot sequence (DESIGN §main): load the drop-in config → hand it to
//! [`teamclaude_rs::server::serve`], which builds the [`Manager`], spawns the
//! axum proxy task and the background loops, and binds → run the TUI (or block
//! in `--headless`) → shut the handle down, which flushes on exit.
//!
//! What is left in THIS file is what only a binary may do: parse clap, install a
//! logging subscriber, print operator diagnostics, and turn a stand-down into a
//! process exit code. Everything reusable lives in the library.
//!
//! [`Manager`]: teamclaude_rs::manager::Manager

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use teamclaude_rs::cli::{self, validate_group_label_chars, PriorityArg};
use teamclaude_rs::config::{self, Config, ConfigError};
use teamclaude_rs::peer;
use teamclaude_rs::proxy::GROUP_HEADER_NAME;
use teamclaude_rs::{
    affinity, build_info, demo, mint, mitm, oauth, server, singleton, status, tui, update,
};

#[derive(Parser)]
#[command(
    name = "tcr",
    version,
    about = "Lean single-user rotating Anthropic proxy with a live TUI",
    // Let `tcr [flags]` (no subcommand) behave as the default server run, while
    // `tcr server [flags]` is the explicit form. The two cannot be mixed.
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    server: ServerArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server (the default when no subcommand is given).
    Server(ServerArgs),
    /// Run Claude Code through the proxy (launches it directly if we're not up).
    Run(RunArgs),
    /// Authenticate a Claude account via the browser and add it to the config.
    Login(LoginArgs),
    /// Mint a long-lived (365-day) token for an existing account, or every
    /// account in a group, and put the result on the clipboard. Nothing is
    /// stored — export only.
    Mint(MintArgs),
    /// List the configured accounts (offline; `--probe` refreshes live quota).
    Accounts(AccountsArgs),
    /// Remove an account from the config.
    Remove(RemoveArgs),
    /// Set an account's rotation priority (lower value = preferred).
    Priority(PriorityArgs),
    /// Print an account's current access token to stdout (a secret — pipe it, do
    /// not paste it into a chat or a shell history).
    Token(TokenArgs),
    /// Enable an account (clears the `disabled` flag).
    Enable(EnableArgs),
    /// Disable an account (holds it out of rotation).
    Disable(DisableArgs),
    /// Set, clear, or show the identity-bound control account.
    Control(ControlArgs),
    /// Manage account group membership (`ls` / `add` / `rm`).
    Group(GroupArgs),
    /// The LAN peer mesh: find your other Macs, trust one, share accounts with
    /// it, or reach the internet through it.
    Peer(peer_cli::PeerArgs),
    /// Probe every account's live quota and print the fleet status.
    Status(StatusArgs),
    /// Print the live sessions the running proxy has seen in the last hour.
    ///
    /// Separate from `status` on purpose: `tcr status --json` emits a bare
    /// array of accounts and that contract has clients (the panel, `jq`
    /// one-liners in the docs). Sessions get their own object-shaped verb
    /// rather than a flag that changes `status`'s top-level type.
    Sessions(SessionsArgs),
    /// Print a usage report for the last N days (default 7), read from the
    /// usage ledger — cost, tokens, cache-hit ratio, by model/account/day, and
    /// the busiest sessions.
    Wrap(WrapArgs),
    /// Self-update: `git pull --ff-only` + `cargo build --release` in the checkout.
    Update(UpdateArgs),
    /// Render the TUI against fake accounts (for a sanitized README screenshot).
    Demo,
    /// Open TcrBar, the macOS menu-bar app (macOS only).
    Ui,
    /// Say whether Claude Code is actually reaching this proxy, and which file
    /// decides that. Exits 0 when it is, 2 when another base URL owns the
    /// route, 3 when the route points here and no proxy answers.
    Doctor(DoctorArgs),
}

/// `tcr peer …`, the argument shape for the LAN peer mesh.
///
/// The two verbs on the common path are `find` and `share`, one switch each and
/// both off by default: finding opens a port, announces presence (and a name,
/// if allowed) and carries bytes it cannot read, while sharing lets another
/// machine read the requests it serves. A switch that silently included the
/// other would hide the one disclosure that matters. Everything else here is a
/// setting, present for the operator who wants it and out of the way of the one
/// who does not.
///
/// Like `tcr group`, this shape is a CONTRACT with the TcrBar panel, which
/// shells out to it. The panel never speaks HTTP to the proxy and never reads
/// the config file, so every write it makes is an argv built from this surface
/// and every read is a `--json` sibling document.
///
/// `#[allow(dead_code)]`: the fields below are the surface, and the phase that
/// fills `run_peer`'s bodies is what starts reading them. It deletes this line
/// on the way past, the suppression is deliberately on the module rather than
/// on each struct inside it, so there is one thing to delete and one place to
/// look.
#[allow(dead_code)]
mod peer_cli {
    use std::path::PathBuf;

    use clap::Subcommand;

    #[derive(clap::Args)]
    pub struct PeerArgs {
        #[command(subcommand)]
        pub action: PeerAction,
    }

    #[derive(Subcommand)]
    pub enum PeerAction {
        /// Print this node's own peer id, minting the keypair on first use.
        ///
        /// Never re-mints: a new key evicts every peer that pinned this one.
        Id(PeerIdArgs),
        /// List pinned peers, what each may do, and what is in flight.
        Ls(PeerLsArgs),
        /// Find other Macs on this network, and let them find this one.
        Find(PeerFindArgs),
        /// Set this Mac's display name, what another Mac shows for it.
        Name(PeerNameArgs),
        /// Let trusted Macs serve requests with this Mac's accounts, which
        /// means they read those requests, prompts included.
        Share(PeerSwitchArgs),
        /// Trust a Mac interactively: both screens show six digits, you
        /// compare them, you confirm on both.
        Pair(PeerPairArgs),
        /// Mint a one-line join key for a Mac with no screen.
        Invite(PeerInviteArgs),
        /// Join another Mac using a key it printed.
        Join(PeerJoinArgs),
        /// Stop trusting a Mac. One deleted line; the next handshake from it
        /// fails.
        ///
        /// It does NOT revoke egress still reachable through a peer that holds
        /// `forward`, and the output says so whenever one does.
        Forget(PeerForgetArgs),
        /// Say hello to one pinned peer over the address already on its row:
        /// tell it where this node listens now, and record where it says it
        /// listens. The one thing that lets a moved Mac be found again
        /// without re-pairing.
        Hello(PeerHelloArgs),
        /// Grant or revoke one thing for one peer.
        Allow(PeerAllowArgs),
        /// Set what one peer may borrow on one window, or take it away.
        Lend(PeerLendArgs),
        /// Choose how this Mac reaches the internet.
        Via(PeerViaArgs),
        /// Show the Macs asking to pair with this one, and nothing else about
        /// them: a request is a row, never a trust decision.
        Pending(PeerPendingArgs),
        /// Approve one pairing request, opening a two-minute window for that
        /// one Mac.
        Accept(PeerDecideArgs),
        /// Turn one pairing request down and stay quiet to that address for an
        /// hour.
        Ignore(PeerDecideArgs),
        /// Never hear from that Mac again: its address, and its key too when
        /// this Mac has learned one.
        Block(PeerDecideArgs),
        /// Lift a block.
        Unblock(PeerUnblockArgs),
        /// The office network key: mint one, paste one in, or clear it.
        NetworkKey(PeerNetworkKeyArgs),
        /// Print one link to share, which brings another Mac onto this mesh.
        Link(PeerLinkArgs),
        /// Mint, or open, the sealed link one Mac sends one friend after it
        /// changed networks. It carries an address and nothing else: it joins
        /// nothing, grants nothing and pairs nothing.
        Moved(PeerMovedArgs),
        /// Print what this Mac can be reached on from off its own LAN.
        Reach(PeerReachArgs),
        /// Let pinned Macs reach this one from off its own LAN. Off by
        /// default: with it on, this Mac asks its router for a port mapping
        /// and answers a return visit against a pinned key from anywhere,
        /// and still answers nothing else from off the LAN.
        Internet(PeerInternetArgs),
        /// Set where one account's requests leave from, and what an exit
        /// this Mac cannot reach costs.
        Account(PeerAccountArgs),
        /// Ask the RUNNING proxy what it holds for each pinned Mac.
        Status(PeerStatusArgs),
        /// Print, or serve, the whole mesh as this Mac sees it: one node per
        /// Mac, one edge per way to reach one, one edge per live lease.
        Graph(PeerGraphArgs),
    }

    #[derive(clap::Args)]
    pub struct PeerGraphArgs {
        /// Path to the peers file (default: `tcr-peers.json` in the
        /// operator's own config directory). Also selects the runtime-state
        /// file beside it and the node-key directory, the same one-flag
        /// contract every other peer verb gives a test.
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Emit the graph as JSON instead of greppable text.
        #[arg(long)]
        pub json: bool,
        /// Serve an inline HTML page drawing the graph, refreshed every 5 s.
        /// Binds LOOPBACK ONLY and refuses any other address; the mesh-served
        /// variant (any trusted Mac reading any other's graph) is deferred.
        #[arg(long)]
        pub serve: bool,
        /// Loopback address to serve on with `--serve`.
        #[arg(long, default_value = "127.0.0.1:7756")]
        pub addr: String,
    }

    #[derive(clap::Args)]
    pub struct PeerAccountArgs {
        /// Path to the peers file (default: `tcr-peers.json` in the config
        /// directory). Read to turn a Mac's NAME into the peer id the pin
        /// stores, and to say so when no pinned Mac answers to it.
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Path to the main config (default: `teamclaude.json` in the config
        /// directory). This is the file the pin is written to.
        #[arg(long)]
        pub config: Option<PathBuf>,
        /// Which account to change, by the same query every other account verb
        /// takes: a name, or enough of one to be unambiguous.
        pub account: String,
        /// Where this account's requests leave from: `local` for this Mac's
        /// own socket, or a pinned Mac, named by its peer id or by the name it
        /// shows under in `tcr peer ls`.
        ///
        /// Omitted leaves the pin alone, which is what lets `--must` change
        /// only the strictness of a pin that is already there.
        #[arg(long = "exits-from", value_name = "local|PEER")]
        pub exits_from: Option<String>,
        /// Refuse a request this exit cannot carry, rather than sending it out
        /// of this Mac instead. For an account pinned BECAUSE the address is
        /// load-bearing, where leaving by another route is worse than not
        /// leaving at all.
        #[arg(long)]
        pub must: bool,
        /// Let a request fall back to this Mac when the exit is unavailable,
        /// which is the default.
        #[arg(long = "no-must", conflicts_with = "must")]
        pub no_must: bool,
    }

    #[derive(clap::Args)]
    pub struct PeerInternetArgs {
        /// Path to the peers file (default: `tcr-peers.json` in the config directory).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// `on` or `off`.
        #[arg(value_enum)]
        pub state: Switch,
    }

    #[derive(clap::Args)]
    pub struct PeerStatusArgs {
        /// Path to the main config (default: `teamclaude.json` in the config
        /// directory), read for the port and api-key this asks on.
        #[arg(long)]
        pub config: Option<PathBuf>,
        /// Emit the peers block as JSON instead of greppable text.
        #[arg(long)]
        pub json: bool,
    }

    #[derive(clap::Args)]
    pub struct PeerReachArgs {
        /// Path to the peers file (default: `tcr-peers.json` in the config directory).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Emit a machine-readable JSON object instead of greppable text.
        #[arg(long)]
        pub json: bool,
        /// Also ask the router for a port mapping, held for two minutes.
        ///
        /// Off by default because it WRITES to the router. A repeated NAT-PMP
        /// request replaces the lifetime of an existing mapping rather than
        /// adding one (RFC 6886 s3.3), so a probe run while something holds a
        /// long mapping for the same port would cut that mapping down to this
        /// probe's two minutes. Without the flag this verb only asks questions.
        #[arg(long)]
        pub map: bool,
    }

    #[derive(clap::Args)]
    pub struct PeerPendingArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json). Also
        /// selects the runtime-state file beside it, so a test points the
        /// whole peer surface at a temp dir with this one argument.
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Emit a machine-readable JSON object instead of greppable text.
        #[arg(long)]
        pub json: bool,
    }

    /// The selector every approval verb takes: an instance id or an address.
    ///
    /// One shape for `accept`, `ignore` and `block` rather than three, because
    /// the three are one decision an operator makes about one row and a
    /// selector that meant different things per verb is the kind of difference
    /// nobody discovers until they block the wrong Mac.
    #[derive(clap::Args)]
    pub struct PeerDecideArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The instance id or the address, as `tcr peer pending` prints them.
        pub target: String,
    }

    #[derive(clap::Args)]
    pub struct PeerUnblockArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The address to unblock, as `tcr peer ls --json` lists it under
        /// `blocked`.
        pub addr: String,
    }

    /// `tcr peer network-key set|join|clear|show [<key>]`.
    ///
    /// A flat args struct with the action as a positional value, not a nested
    /// `#[command(subcommand)]`. Measured, not a preference: with a nested
    /// subcommand, clap requires the parent's own flags BEFORE it, so
    /// `tcr peer network-key set --peers <path>` is refused with "unexpected
    /// argument '--peers' found" while every other peer verb accepts `--peers`
    /// anywhere. One surface where a flag goes in a different place is a
    /// surface an operator gets wrong once and a test harness gets wrong
    /// silently.
    #[derive(clap::Args)]
    pub struct PeerNetworkKeyArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// What to do: mint one, paste one in, clear it, or say whether one is
        /// set.
        #[arg(value_enum)]
        pub action: PeerNetworkKeyAction,
        /// For `join`: the key string the other Mac printed. Omit with
        /// `--stdin`.
        pub key: Option<String>,
        /// For `join`: read the key from standard input instead, so it never
        /// enters this process's argv and so never appears in `ps` or in shell
        /// history.
        #[arg(long)]
        pub stdin: bool,
        /// Required by `set` and `join` when a key is ALREADY set, because
        /// replacing one cuts this Mac off from every Mac still holding the old
        /// one. Without it the verb refuses and prints what it would have cut
        /// off.
        #[arg(long)]
        pub replace: bool,
    }

    #[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
    pub enum PeerNetworkKeyAction {
        /// Mint a key and print it once. Every Mac in the office pastes it in
        /// with `network-key join`.
        Set,
        /// Paste in the key another Mac printed.
        Join,
        /// Forget the key. This Mac then sees, and is seen by, every `tcr` on
        /// the network again.
        Clear,
        /// Say whether a key is set, without printing it.
        Show,
    }

    #[derive(clap::Args)]
    pub struct PeerLinkArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Also mint a one-use join key and carry it in the link, so the other
        /// Mac enrols with this one on opening it. **The link is then a live
        /// bearer secret with ten minutes on it**, without this flag it
        /// carries the network key alone.
        #[arg(long)]
        pub invite: bool,
        /// A name for the joining Mac, when `--invite` is given.
        #[arg(long)]
        pub label: Option<String>,
    }

    /// Which half of `tcr peer moved` is being run.
    ///
    /// A typed pair rather than two verbs, because the two are one feature and
    /// a reader looking for either finds both, and rather than a bare string,
    /// so a third spelling cannot appear at one call site and not another.
    #[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
    pub enum PeerMovedAction {
        /// Print one link for one already-trusted Mac, saying where this Mac
        /// is now.
        Mint,
        /// Read a link somebody sent: say which of this Mac's peers it is from
        /// and where that Mac now is.
        Open,
    }

    #[derive(clap::Args)]
    pub struct PeerMovedArgs {
        /// `mint` or `open`.
        #[arg(value_enum)]
        pub action: PeerMovedAction,
        /// For `mint`: the peer id to seal for, in its full wire form, the
        /// `node` field of `tcr peer ls --json`. For `open`: the
        /// `tcr://peer/moved?…` link.
        ///
        /// **A link typed here is visible in `ps` to every process on this Mac
        /// and in the shell history**, and `open` says so on stderr when it
        /// finds one here. `--stdin` is the path that does not leak, and it is
        /// the only one the panel and the `tcr://` handler use.
        pub target: Option<String>,
        /// For `open`: read the link from standard input, one line, so it never
        /// enters this process's argument vector.
        #[arg(long)]
        pub stdin: bool,
        /// For `open`: write the addresses. Without it `open` reads the link,
        /// says what it would add, and changes nothing. No effect on `mint`.
        #[arg(long)]
        pub yes: bool,
        /// Path to the peers file (default: ~/.config/tcr-peers.json). Also
        /// selects the runtime-state file beside it and the node-key directory,
        /// the same one-flag contract every other peer verb gives a test.
        #[arg(long)]
        pub peers: Option<PathBuf>,
    }

    /// On or off. A typed pair rather than a bare string, so a third spelling
    /// cannot appear at one call site and not another.
    #[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
    pub enum Switch {
        On,
        Off,
    }

    /// The argv spelling of [`teamclaude_rs::peer::config::LendMode`].
    ///
    /// A separate enum because clap's derive needs one it owns, and the
    /// conversion is the one place the two spellings meet.
    #[derive(Clone, Copy, Debug, clap::ValueEnum)]
    pub enum PeerLendMode {
        /// The borrower's requests go over the owner's Mac.
        Serve,
        /// The owner hands over a short-lived bearer and the borrower sends
        /// on its own IP.
        Hand,
    }

    impl From<PeerLendMode> for teamclaude_rs::peer::config::LendMode {
        fn from(mode: PeerLendMode) -> Self {
            match mode {
                PeerLendMode::Serve => Self::Serve,
                PeerLendMode::Hand => Self::Hand,
            }
        }
    }

    /// Which grant `tcr peer allow` is setting.
    ///
    /// The two inspect grants are one direction each, and the asymmetry is the
    /// point: `inspect` is "I will read their requests" and `disclose` is
    /// "they may read mine". A lease needs an explicit act on both machines.
    #[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
    pub enum PeerGrant {
        /// Carry this peer's bytes out to an allow-listed origin, blind.
        Gateway,
        /// We may ask this peer to carry us out. The inverse of `Gateway`,
        /// separate because one act must not grant both directions.
        Carry,
        /// Forward to peers this Mac has pinned. **Transitive**: the node two
        /// hops out then reaches them with this peer's authority.
        Forward,
        /// Accept requests from this peer and serve them here, reading them in
        /// full.
        Inspect,
        /// Send requests to this peer to serve, letting it read them in full.
        Disclose,
        /// Accept an account moved from this peer. The only grant under which
        /// a credential crosses a host boundary.
        AcceptMove,
        /// Tell this peer about this Mac's other peers, one level out.
        ControlBriefs,
        /// Tell this peer how much this Mac could lend, per window.
        ControlLendable,
        /// Tell this peer this Mac's build and boot id.
        ControlDiag,
    }

    /// `--peers <path>`, on every verb, for the same reason `--config` is on
    /// every account verb: a test points the whole peer surface at a temp file
    /// with one argument and touches nothing real.
    #[derive(clap::Args)]
    pub struct PeerIdArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json). Also
        /// selects the node-key directory, so a test points the whole peer
        /// surface at a temp dir with this one argument.
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Mint a NEW keypair, evicting every peer that pinned this one, the
        /// next handshake from any of them fails the pin check, correctly.
        /// Requires `--yes`.
        #[arg(long)]
        pub regenerate: bool,
        /// Confirm `--regenerate`. Has no effect alone.
        #[arg(long)]
        pub yes: bool,
    }

    #[derive(clap::Args)]
    pub struct PeerLsArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// Emit a machine-readable JSON object instead of greppable text.
        ///
        /// A SIBLING document, never an addition to `tcr status --json`: that
        /// is a bare array of accounts and clients depend on exactly that
        /// shape.
        ///
        /// Every row under `peers` carries two derived keys beside the file's
        /// own: `until`, the unix SECONDS this Mac's lending to that row stops
        /// (`null` when nothing it holds has an end), and `ended`, true when
        /// every lease it holds is past its `until`, the row stays in the
        /// list either way. Names as written, camelCase like their
        /// neighbours; the panel decodes both.
        #[arg(long)]
        pub json: bool,
        /// Path to the main config (default: ~/.config/teamclaude.json), read
        /// for the ACCOUNT LABELS the `lentTo` block is keyed by. Nothing else
        /// is taken from it, and a config that is missing or unreadable leaves
        /// `lentTo` empty rather than failing the listing: the peers half of
        /// this output does not depend on it.
        #[arg(long)]
        pub config: Option<PathBuf>,
    }

    #[derive(clap::Args)]
    pub struct PeerSwitchArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// `on` or `off`.
        #[arg(value_enum)]
        pub state: Switch,
        /// Which rate-limit window is shared. `5h` and `7d` are UNTIERED, a
        /// lease on either is not per-model, because `7d_oi` is the only
        /// window upstream reports per model at all.
        #[arg(long, value_enum, default_value = "7d")]
        pub window: PeerWindow,
        /// The ceiling on any one lease, as a fraction of that window.
        /// Clamped to 0.0..=0.5, the same clamp the main config applies to its
        /// own reserve.
        #[arg(long, default_value_t = 0.10)]
        pub fraction: f64,
        /// How long a granted lease lives, in seconds.
        #[arg(long, default_value_t = 600)]
        pub ttl: u32,
        /// How many borrowed requests may be in flight against one lease.
        /// `0` mints leases every request of which is refused, which is what
        /// writing `0` asks for; it is not silently raised to `1`.
        #[arg(long, default_value_t = 2)]
        pub max_inflight: u8,
        /// What the DEFAULT lease draws from: `all`, `group:<name>`, or
        /// `account:<label>[,<label>]`. Written onto every pinned Mac and
        /// recorded as `defaultLend`, which is what the Sharing defaults sheet
        /// shows.
        #[arg(long, default_value = "all")]
        pub scope: String,
    }

    /// The wire's three windows, as an operator types them.
    ///
    /// A typed pair with [`tcr_peer_wire::Window`] rather than a string parsed
    /// at each call site: `Window::Unknown` is a FORWARD-COMPAT arm for a
    /// window a future build names, and an operator must not be able to ask for
    /// it, a lease against a window this build cannot measure is a lease
    /// against nothing.
    #[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
    pub enum PeerWindow {
        /// The rolling five-hour window. Untiered.
        #[value(name = "5h")]
        FiveHour,
        /// The weekly window. Untiered.
        #[value(name = "7d")]
        SevenDay,
        /// The weekly OUTPUT window, which is the only model-scoped one.
        #[value(name = "7d_oi")]
        SevenDayOi,
    }

    impl From<PeerWindow> for tcr_peer_wire::Window {
        fn from(window: PeerWindow) -> Self {
            match window {
                PeerWindow::FiveHour => Self::FiveHour,
                PeerWindow::SevenDay => Self::SevenDay,
                PeerWindow::SevenDayOi => Self::SevenDayOi,
            }
        }
    }

    #[derive(clap::Args)]
    pub struct PeerLendArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The peer id to change, in its full wire form, the `node` field of
        /// `tcr peer ls --json`, never the `tcr-…` short form the text output
        /// prints. See [`PeerAllowArgs::peer`].
        pub peer: String,
        /// Which window this grant is against.
        #[arg(long, value_enum, default_value = "7d")]
        pub window: PeerWindow,
        /// The ceiling on any one lease, as a fraction of that window.
        /// Clamped to 0.0..=0.5. `0` removes the grant.
        #[arg(long, default_value_t = 0.10)]
        pub fraction: f64,
        /// How long a granted lease lives, in seconds.
        #[arg(long, default_value_t = 600)]
        pub ttl: u32,
        /// How many borrowed requests may be in flight against one lease.
        #[arg(long, default_value_t = 2)]
        pub max_inflight: u8,
        /// What this lease draws from: `all`, `group:<name>`, or
        /// `account:<label>[,<label>]`. A Mac may hold one lease
        /// per scope, so lending a second scope adds a lease rather than
        /// replacing the first.
        ///
        /// Labels are the sanitized ones `tcr status` prints, never an email
        /// and never a uuid, because this is written to a file and printed by a
        /// CLI in a public repository.
        #[arg(long, default_value = "all")]
        pub scope: String,
        /// How the borrower spends this grant: `serve` (its requests go over
        /// this Mac and out on this Mac's IP, and this Mac reads them) or
        /// `hand` (this Mac hands over the account's short-lived access token
        /// and the borrower sends on its own IP).
        ///
        /// Omitted keeps the mode of the grant being replaced, and `serve` for
        /// a new one. Without that, editing a hand grant's fraction would
        /// quietly turn it back into a serve grant, which is a different
        /// disclosure decision than the one the operator took.
        #[arg(long, value_enum)]
        pub mode: Option<PeerLendMode>,
        /// Path to the main config (default: `teamclaude.json` in the config
        /// directory). Read only by `--mode hand`, to refuse a grant every
        /// account in its scope is strictly pinned away from.
        #[arg(long)]
        pub config: Option<PathBuf>,
        /// Lend for a duration (`2h`, `90m`, `3d`), after which this Mac stops
        /// renewing the lease. `none` clears an end.
        #[arg(long = "for", value_name = "DURATION", conflicts_with = "until")]
        pub for_: Option<String>,
        /// Lend until a time of day (`18:00`), today or tomorrow, whichever
        /// comes next.
        #[arg(long)]
        pub until: Option<String>,
        /// Only lend between these hours, local time (`22:00-08:00`). A window
        /// that ends before it starts crosses midnight. Unlike `--until`, which
        /// ends the lending once, this closes and re-opens every day.
        #[arg(long, value_name = "HH:MM-HH:MM")]
        pub between: Option<String>,
        /// Only lend on these days (`mon,tue`), meaning the days the
        /// `--between` window may START on: an overnight window granted on
        /// `fri` runs into Saturday morning.
        #[arg(long, value_name = "DAYS")]
        pub days: Option<String>,
        /// List this peer's leases instead of changing them, one greppable
        /// line each, with the lease id `--revoke` and `--relend` take.
        #[arg(long, conflicts_with_all = ["revoke", "relend"])]
        pub list: bool,
        /// Take one lease away, by the id `--list` printed. The other leases
        /// this Mac holds are untouched.
        #[arg(long, value_name = "LEASE-ID", conflicts_with = "relend")]
        pub revoke: Option<String>,
        /// Put an ended lease back to work, with a new `--for`/`--until` or
        /// with no end at all, for "re-lend with one click".
        #[arg(long, value_name = "LEASE-ID")]
        pub relend: Option<String>,
    }

    #[derive(clap::Args)]
    pub struct PeerFindArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// `on` or `off`.
        #[arg(value_enum)]
        pub state: Switch,
        /// Whether the beacon carries this Mac's display name. **Off by
        /// default**: a name is the one field in the
        /// beacon that is not ephemeral, so it is the one field the operator
        /// has to ask for. Turn it on and a row says "studio-mac" instead of an
        /// address, which is the difference between a feature and a manual.
        ///
        /// Off leaves the beacon as presence, a port and this boot's instance
        /// id. It is NOT a privacy control for identity, the beacon never
        /// carries a node id, a public key or any other key material, whichever
        /// way this is set.
        #[arg(long, value_enum)]
        pub announce_name: Option<Switch>,
    }

    #[derive(clap::Args)]
    pub struct PeerNameArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The name to show. Omit to print the current one, which is the host
        /// name until you set something else. Refused if it carries an `@`, a
        /// uuid shape, or an organization name: a name reaches other machines.
        pub name: Option<String>,
    }

    #[derive(clap::Args)]
    pub struct PeerPairArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// `host:port` of the Mac to pair with.
        pub addr: String,
        /// The six digits the other screen is showing. Omit to start the
        /// pairing and print this side's digits.
        pub code: Option<String>,
        /// Print one JSON object per line on stdout instead of prose, for a
        /// caller that is not a terminal.
        ///
        /// The pairing holds ONE live handshake across both phases and cannot
        /// be split into two invocations, so a panel has to stay attached to
        /// this process, read these lines and answer on its stdin. The lines
        /// are `crate::peer::pair::PairEvent` and `tests/peer_pairing.rs`
        /// pins them.
        #[arg(long)]
        pub json: bool,
    }

    #[derive(clap::Args)]
    pub struct PeerInviteArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// A name for the joining Mac. Refused if it carries an `@`, a uuid
        /// shape, or an organization name: a label reaches every trusted peer.
        #[arg(long)]
        pub label: Option<String>,
        /// How long the key stays usable, in seconds. Short on purpose, while
        /// it exists, anything that can read the peers file can use it.
        #[arg(long, default_value_t = 600)]
        pub ttl: u32,
        /// How many Macs may join with this one key.
        #[arg(long, default_value_t = 1)]
        pub uses: u8,
        /// Revoke an outstanding key by id instead of minting one.
        #[arg(long)]
        pub revoke: Option<u64>,
    }

    #[derive(clap::Args)]
    pub struct PeerJoinArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The `tcr-join:…` key or the `tcr://peer/join?…` link the other
        /// Mac printed.
        ///
        /// **Typed here, the secret is visible in `ps` output to every process
        /// on this Mac and in the shell history.** Kept because it is the
        /// one-paste headless path this design is built around and a machine
        /// with no screen has no better channel; `--stdin` is the one that does
        /// not leak, and it is what the panel and the `tcr://` URL handler use.
        pub key: Option<String>,
        /// Read the key or link from standard input instead, one line,
        /// consumed by this process and never placed in an argument vector.
        ///
        /// This is the ONLY path the panel uses (its Paste-a-key sheet and its
        /// `tcr://` handler both pipe to it), because a secret in argv is a
        /// secret every other process on the Mac can read out of `ps`.
        #[arg(long, conflicts_with = "key")]
        pub stdin: bool,
        /// This Mac's name, as the other one will show it.
        #[arg(long)]
        pub label: Option<String>,
        /// Replace a network key this Mac already has.
        ///
        /// Without it, a link carrying a network key is refused when one is
        /// already set, the same refusal `tcr peer network-key join` gives and
        /// for the same reason: a second office's key pasted over the first is
        /// the commonest way a Mac disappears from its own mesh.
        #[arg(long)]
        pub replace: bool,
    }

    #[derive(clap::Args)]
    pub struct PeerForgetArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The peer id to forget, as `tcr peer ls` prints it.
        pub peer: String,
    }

    #[derive(clap::Args)]
    pub struct PeerHelloArgs {
        /// Path to the peers file (default: `tcr-peers.json` in the config directory).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The peer id to greet, in its full wire form, the `node` field of
        /// `tcr peer ls --json` (not the short `tcr-...` form `tcr peer ls`
        /// prints as text).
        pub peer: String,
    }

    #[derive(clap::Args)]
    pub struct PeerAllowArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// The peer id to change, in its full wire form, the `node` field of
        /// `tcr peer ls --json`.
        ///
        /// NOT the `tcr-…` short form `tcr peer ls` prints as text:
        /// `PeerId::parse` refuses it deliberately, because a truncated id that
        /// silently resolved to a pinned peer would be a prefix-collision
        /// attack with a friendly face.
        pub peer: String,
        /// Which grant.
        #[arg(value_enum)]
        pub grant: PeerGrant,
        /// `on` or `off`.
        #[arg(value_enum)]
        pub state: Switch,
    }

    #[derive(clap::Args)]
    pub struct PeerViaArgs {
        /// Path to the peers file (default: ~/.config/tcr-peers.json).
        #[arg(long)]
        pub peers: Option<PathBuf>,
        /// `auto` to use a trusted Mac whenever the direct path is dead, `off`
        /// to never route out through a peer, or a peer id to pin one.
        pub target: String,
        /// How long a carry may take to set up, in milliseconds. This only
        /// LOWERS the built-in bound (5000 ms): a carry runs after the direct
        /// path has already failed, so waiting longer than the default is worse
        /// for the caller than the answer it already has.
        #[arg(long)]
        pub setup_timeout_ms: Option<u64>,
    }
}

#[derive(clap::Args)]
struct AccountsArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Refresh each account's live quota before listing (network probe).
    #[arg(long)]
    probe: bool,
}

#[derive(clap::Args)]
struct RemoveArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct TokenArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct PriorityArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
    /// The explicit priority value (lower = preferred). Omit with --first/--last.
    #[arg(conflicts_with_all = ["first", "last"])]
    value: Option<i64>,
    /// Move the account to the front of rotation (min priority - 1).
    #[arg(long, conflicts_with = "last")]
    first: bool,
    /// Move the account to the back of rotation (max priority + 1).
    #[arg(long)]
    last: bool,
}

#[derive(clap::Args)]
struct EnableArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct DisableArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct ControlArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Account name — exact and case-sensitive, not a substring. Omit with
    /// `--clear` or `--show`.
    #[arg(conflicts_with_all = ["clear", "show"])]
    query: Option<String>,
    /// Clear the control account (identity traffic resolves to none).
    #[arg(long, conflicts_with = "show")]
    clear: bool,
    /// Print the current control account and change nothing.
    #[arg(long)]
    show: bool,
}

#[derive(clap::Args)]
struct GroupArgs {
    #[command(subcommand)]
    action: GroupAction,
}

/// `tcr group ls|add|rm|reserve|unreserve|park|unpark|allow-control|disallow-control|color` — the argument shape here is a
/// CONTRACT with the TcrBar panel, which shells out to it (`TcrTool.run`); do
/// not change it.
#[derive(Subcommand)]
enum GroupAction {
    /// List groups and their members.
    Ls(GroupLsArgs),
    /// Add one account to one group.
    Add(GroupAddArgs),
    /// Remove one account from one group, or `--all` to delete the group.
    Rm(GroupRmArgs),
    /// Reserve a group: an account carrying it becomes off-limits to traffic
    /// that did not ask for one of its groups. A running proxy picks this up
    /// live (no restart) on its next natural cadence check.
    Reserve(GroupReserveArgs),
    /// Clear a group's reserved flag.
    Unreserve(GroupUnreserveArgs),
    /// Park a group: every account carrying it is held out of rotation, the
    /// way `tcr disable` holds one account out — no request reaches it, not
    /// even an explicit `--group` ask. A running proxy picks this up live (no
    /// restart) on its next natural cadence check.
    Park(GroupParkArgs),
    /// Clear a group's parked flag and put its members back in rotation. An
    /// account disabled on its own stays disabled.
    Unpark(GroupUnparkArgs),
    /// Opt a group in to selecting the control account on an explicit
    /// `--group` ask — otherwise inference never selects it. A running proxy
    /// picks this up live (no restart) on its next natural cadence check.
    AllowControl(GroupAllowControlArgs),
    /// Clear a group's `allowControlAccount` flag.
    DisallowControl(GroupDisallowControlArgs),
    /// Set (or `--clear`) a group's color — the tag the panel draws for it.
    Color(GroupColorArgs),
}

#[derive(clap::Args)]
struct GroupLsArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit a machine-readable JSON equivalent instead of greppable text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct GroupAddArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to add.
    group: String,
    /// Account name — exact and case-sensitive.
    account: String,
}

#[derive(clap::Args)]
struct GroupRmArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to remove.
    group: String,
    /// Account name to drop from the group. Required unless `--all` is given;
    /// conflicts with `--all` so the parser — not a runtime panic — refuses
    /// "both" and "neither".
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    account: Option<String>,
    /// Remove the group from every member instead of one account — deletes
    /// the group, since groups exist only while some account carries the
    /// label.
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
struct GroupReserveArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to reserve.
    group: String,
}

#[derive(clap::Args)]
struct GroupUnreserveArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to unreserve.
    group: String,
}

#[derive(clap::Args)]
struct GroupParkArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to park.
    group: String,
}

#[derive(clap::Args)]
struct GroupUnparkArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to unpark.
    group: String,
}

#[derive(clap::Args)]
struct GroupAllowControlArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to opt in.
    group: String,
}

#[derive(clap::Args)]
struct GroupDisallowControlArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to opt out.
    group: String,
}

#[derive(clap::Args)]
struct GroupColorArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// The group label to color.
    group: String,
    /// The color, as `#RGB` or `#RRGGBB` (case-insensitive). Required unless
    /// `--clear` is given; conflicts with `--clear` so the parser refuses
    /// "both" and "neither" the same way `GroupRmArgs`'s `account`/`--all`
    /// does.
    #[arg(required_unless_present = "clear", conflicts_with = "clear")]
    hex: Option<String>,
    /// Revert to the color derived from the group name instead of setting one.
    #[arg(long)]
    clear: bool,
}

#[derive(clap::Args)]
struct StatusArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit the fleet status as a JSON array instead of greppable text.
    #[arg(long)]
    json: bool,
}

/// `tcr doctor [--json]`: the route check. Same two options every read-only
/// verb here takes, and no third: the question it answers is about THIS
/// machine's configuration, so there is nothing to select.
#[derive(clap::Args)]
struct DoctorArgs {
    /// Path to the config file (default: the usual per-user path).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit one JSON object instead of greppable `key: value` lines.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct SessionsArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit `{"supported": bool, "sessions": [...]}` instead of greppable text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct WrapArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json) —
    /// consulted only for pricing overrides, never for accounts.
    #[arg(long)]
    config: Option<PathBuf>,
    /// How many days back to report, ending today (UTC).
    #[arg(long, default_value_t = 7)]
    days: u32,
    /// Emit the report as one JSON object instead of plain text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct UpdateArgs {
    /// Rebuild even when `git pull` reports the checkout is already up to date.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct LoginArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Skip the refusal a running proxy would otherwise trigger, and write the
    /// login to the config file directly. Still probes the proxy first and still
    /// prefers its live account-add route when one is confirmed and safe — this
    /// only overrides the cases login would otherwise refuse on: an older proxy
    /// with no account-add route, or one that answered but not usably (wedged or
    /// timed out). Never overrides a confirmed live route (already safe, nothing
    /// to force) or a rejected api-key (proof the proxy is alive, so writing the
    /// file beside it is the worst-informed moment to do it) — both still refuse
    /// under --force. Unsafe when it takes the file path: the running server's
    /// next token refresh can overwrite what was just written.
    #[arg(long)]
    force: bool,
    /// Re-login a specific existing account. The identity that comes back must
    /// match, or nothing is written.
    #[arg(long)]
    account: Option<String>,
    /// Add an account from a `claude setup-token` credential instead of the
    /// browser flow — no value here. The token is read from stdin (prompted
    /// when stdin is a TTY), never from argv: an argv value is visible in
    /// `ps` and lands in shell history, both worse leaks than a stdin prompt.
    /// A setup-token credential carries only the `user:inference` scope, so
    /// there is no refresh token (the account serves until the token expires,
    /// about a year, then goes dead — see the warning `tcr login --token`
    /// prints) and usually no email (name it with `--name`, or answer the
    /// prompt). Refuses to combine with `--account`: an inference-only token
    /// carries no identity for that flag to confirm, and an assertion that
    /// cannot be evaluated must fail closed.
    #[arg(long)]
    token: bool,
    /// Add an account from the login the `claude` CLI on this machine already
    /// holds, instead of the browser flow — no value here. The credential is
    /// read from the login Keychain (item `Claude Code-credentials`) on macOS,
    /// else from `~/.claude/.credentials.json`; `TCR_CLAUDE_CODE_CREDENTIALS`
    /// overrides both with a file path. It carries a refresh token and a real
    /// expiry, so the account keeps working the same way a browser login's
    /// does. The cost, printed on import: refresh tokens are single-use, so the
    /// first time tcr refreshes this one, the `claude` CLI's own copy dies and
    /// `claude` asks for a browser login once. Refuses to combine with
    /// `--token`, which is a different credential from a different place.
    /// A first run does this by itself when it finds no accounts — this flag is
    /// the explicit redo (after `tcr remove`, or onto a second config).
    #[arg(long, conflicts_with = "token")]
    from_claude_code: bool,
    /// Drive the login from another program instead of a terminal: stdin is
    /// never read, the browser is never opened here, and progress goes to
    /// stdout as one JSON object per line — `{"event":"browser","url":…}`,
    /// `{"event":"waiting"}`, `{"event":"saved","account":…}`,
    /// `{"event":"error","reason":…}`. The caller opens the URL; the loopback
    /// callback is then the only way the login can complete, and the same
    /// 2-minute timeout exits non-zero with the reason on one stderr line.
    /// Refuses to combine with `--token`, which reads the credential from
    /// stdin: that is the one input this mode has no way to supply.
    #[arg(long, conflicts_with = "token")]
    non_interactive: bool,
    /// Name this account explicitly, overriding the name login would mint for
    /// it. Refused if some other account already has that name — names are
    /// unique, and taking one from an existing row is how a login overwrites
    /// the wrong credential. `--token` needs it most (an inference-only
    /// credential's profile fetch usually comes back with no email to name it
    /// from), but the browser flow honours it too.
    #[arg(long)]
    name: Option<String>,
}

#[derive(clap::Args)]
struct MintArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Mint a long-lived token for one existing account — exact name, not a
    /// substring (see `resolve_account`). Exactly one of `--account` /
    /// `--group` is required.
    #[arg(long, required_unless_present = "group", conflicts_with = "group")]
    account: Option<String>,
    /// Mint a long-lived token for every account carrying this group label.
    #[arg(long)]
    group: Option<String>,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Prefer accounts labelled with this group when picking an account for a
    /// NEW request — falls back to the whole pool when the group has no
    /// capacity, and once a session settles onto an account (a "pin"), that
    /// pin is honoured group-blind for the rest of the session (correct for
    /// this PREFER semantics; the restricting form that also constrains an
    /// existing pin is Phase 2).
    #[arg(long)]
    group: Option<String>,
    /// Args passed verbatim to `claude` (e.g. `tcr run -- -p "hi"`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(clap::Args)]
struct ServerArgs {
    /// Port to bind (overrides `proxy.port` from the config).
    #[arg(long)]
    port: Option<u16>,
    /// Path to the config file (default: ~/.config/teamclaude.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Run without the TUI, logging to stdout.
    #[arg(long)]
    headless: bool,
    /// Take over the port: kill a proxy already listening on it, then bind. The
    /// default is to leave a healthy incumbent alone and exit — replacing it wipes
    /// its session→account pin map and cold-starts every live session's prompt
    /// cache, which is the most expensive event in this system.
    #[arg(long)]
    replace: bool,
    /// DEPRECATED and now a no-op: this is the default. Kept accepted so existing
    /// scripts and launch agents that pass it keep working. Pass `--replace` for
    /// the old default (take the port over).
    ///
    /// `conflicts_with` rather than a silent precedence rule: the two flags are a
    /// contradiction, and the previous wiring resolved it by quietly discarding
    /// `--replace`. An operator whose launchd plist or shell alias already carries
    /// `--no-replace`, adding `--replace` to force a rebuilt binary onto the port,
    /// got a stand-down and exit 0 — while `--help` told them the flag they left
    /// in place does nothing. clap now rejects the pair by name, which is the only
    /// outcome that cannot be misread.
    #[arg(long, conflicts_with = "replace")]
    no_replace: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Server(args)) => run_server(args).await,
        Some(Command::Run(args)) => run_claude(args),
        Some(Command::Login(args)) => run_login(args).await,
        Some(Command::Mint(args)) => run_mint(args).await,
        Some(Command::Accounts(args)) => run_accounts(args).await,
        Some(Command::Remove(args)) => run_remove(args).await,
        Some(Command::Token(args)) => run_token(args),
        Some(Command::Priority(args)) => run_priority(args),
        Some(Command::Enable(args)) => run_enable(args).await,
        Some(Command::Disable(args)) => run_disable(args).await,
        Some(Command::Control(args)) => run_control(args).await,
        Some(Command::Group(args)) => run_group(args),
        Some(Command::Peer(args)) => run_peer(args).await,
        Some(Command::Status(args)) => run_status(args).await,
        Some(Command::Sessions(args)) => run_sessions(args).await,
        Some(Command::Wrap(args)) => run_wrap(args),
        Some(Command::Update(args)) => update::run_update(args.force),
        Some(Command::Demo) => demo::run_demo().await.map_err(anyhow::Error::from),
        Some(Command::Ui) => run_ui(),
        Some(Command::Doctor(args)) => run_doctor(args).await,
        None => run_server(cli.server).await,
    }
}

/// `tcr accounts [--probe]` — list the configured accounts (offline).
async fn run_accounts(args: AccountsArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::list_accounts(&config_path, args.probe).await
}

/// `tcr remove <query>` — delete an account from the config, applying
/// a live disable through the RUNNING proxy first where there is one.
async fn run_remove(args: RemoveArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::remove_account(&config_path, &args.query).await
}

/// `tcr token <query>` — print the account's access token.
fn run_token(args: TokenArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::print_access_token(&config_path, &args.query)
}

/// `tcr priority <query> [N|--first|--last]` — set rotation priority.
fn run_priority(args: PriorityArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    let priority = if args.first {
        PriorityArg::First
    } else if args.last {
        PriorityArg::Last
    } else if let Some(n) = args.value {
        PriorityArg::N(n)
    } else {
        anyhow::bail!("provide a priority value, or one of --first / --last");
    };
    cli::set_priority(&config_path, &args.query, priority)
}

/// `tcr enable <query>` — clear an account's `disabled` flag, in the
/// RUNNING proxy where there is one (async for that reason alone).
async fn run_enable(args: EnableArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::set_enabled(&config_path, &args.query, false).await
}

/// `tcr disable <query>` — hold an account out of rotation, in the RUNNING
/// proxy where there is one.
async fn run_disable(args: DisableArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::set_enabled(&config_path, &args.query, true).await
}

/// `tcr control <query> | --clear | --show` — set, clear, or show the
/// identity-bound control account, in the RUNNING proxy where there is one.
async fn run_control(args: ControlArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    if args.show {
        return cli::show_control(&config_path).await;
    }
    if args.clear {
        return cli::set_control(&config_path, None).await;
    }
    let Some(query) = args.query else {
        anyhow::bail!("provide an account query, or --clear / --show");
    };
    cli::set_control(&config_path, Some(&query)).await
}

/// `tcr peer account <query> --exits-from local|<peer> [--must|--no-must]`:
/// write one account's exit lock.
///
/// The write goes through `cli::set_account_egress`, which is `edit_account`,
/// which is the chain `tcr disable` and `tcr control` already use: one
/// resolution of the account query, one ambiguous-query message, no partial
/// write. This function is argv, a name lookup and the printed line.
///
/// # A NAME is resolved here and a peer id is not
///
/// `--exits-from` takes either, because an operator reads names off
/// `tcr peer ls` and ids are 52 characters. A name is matched against the
/// pinned rows and must match exactly one; a value that parses as a peer id is
/// taken as one WITHOUT checking it is pinned, which is deliberate: pinning an
/// account to a Mac that is not trusted yet is an order the operator may give
/// before pairing, and the request path refuses it on its own terms (an
/// unreachable exit falls back, or refuses under `--must`). A name that
/// matches nothing cannot be that, because there is nothing to have meant.
fn run_peer_account(a: peer_cli::PeerAccountArgs) -> anyhow::Result<()> {
    let config_path = a.config.unwrap_or_else(config::default_path);
    let egress = match a.exits_from.as_deref() {
        None => None,
        Some(raw) if raw.trim() == config::Egress::LOCAL => Some(config::Egress::Local),
        Some(raw) => {
            let raw = raw.trim();
            match tcr_peer_wire::PeerId::parse(raw) {
                Ok(peer) => Some(config::Egress::Via(peer)),
                Err(_) => {
                    let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
                    let store = peer::config::PeerStore::open(&peers_path)?;
                    let matched: Vec<peer::config::PeerRow> = store
                        .peers()
                        .into_iter()
                        .filter(|row| row.label == raw)
                        .collect();
                    match matched.as_slice() {
                        [only] => Some(config::Egress::Via(only.node)),
                        [] => anyhow::bail!(
                            "no pinned Mac is named `{raw}`, and it is not a peer id either \
                             (`tcr peer ls` prints both, and `local` is the word for this \
                             Mac's own socket)"
                        ),
                        several => anyhow::bail!(
                            "`{raw}` names {} pinned Macs, so it cannot say which one this \
                             account leaves from; give the peer id instead (`tcr peer ls \
                             --json` prints it as `node`)",
                            several.len()
                        ),
                    }
                }
            }
        }
    };
    let strict = match (a.must, a.no_must) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    if egress.is_none() && strict.is_none() {
        anyhow::bail!(
            "nothing to change: give --exits-from, --must or --no-must (`tcr peer ls --json` \
             prints every account that has an exit lock under `exits`)"
        );
    }
    let (name, pin) = cli::set_account_egress(&config_path, &a.account, egress, strict)?;
    println!(
        "peer account {name}: exits from {}{}",
        pin.egress,
        if pin.strict {
            ", and a request it cannot carry is refused rather than sent from this Mac"
        } else {
            ", falling back to this Mac when that exit is unavailable"
        }
    );
    println!(
        "the exit lock is read at boot, so a proxy already running keeps the old one until it \
         restarts"
    );
    Ok(())
}

/// When the bearer THIS grant would hand over stops working, in unix seconds.
///
/// The CLI's answer to the question `Manager::handoff_bearer` answers in the
/// running proxy, and deliberately the narrower of the two: it applies the
/// filters the config can see (the grant's scope, a disabled account, a
/// strictly pinned account, which is never handed over) and cannot apply the
/// one only a running proxy has, an account its own health checks have marked
/// errored. So a figure here is "the key this grant is entitled to hand over",
/// which is what a panel drawing a countdown needs; the proxy may still refuse
/// to hand it for a reason no file records.
///
/// `None` for every serve grant and every ended grant, which is `handoff_for`'s
/// own first two refusals, in the same order and for the same reasons.
fn peer_ls_handed_key_until(
    grant: &teamclaude_rs::peer::config::LendGrant,
    accounts: &[config::Account],
    now_s: u64,
) -> Option<u64> {
    if grant.mode != teamclaude_rs::peer::config::LendMode::Hand {
        return None;
    }
    if grant.has_ended(now_s) {
        return None;
    }
    accounts
        .iter()
        .filter(|account| !account.disabled.unwrap_or(false))
        .filter(|account| !account.cannot_be_handed())
        .filter(|account| {
            let label = tcr_peer_wire::sanitize_label(&account.name).unwrap_or_default();
            peer::lease::scope_covers(
                &grant.scope,
                &label,
                &account.groups.clone().unwrap_or_default(),
            )
        })
        .find_map(|account| account.expires_at)
        .and_then(|expires_at_ms| u64::try_from(expires_at_ms / 1_000).ok())
}

/// Every account that HAS an exit lock, keyed by the label a scope names it by.
///
/// An account with neither a pin nor strictness is absent rather than present
/// with `local`, for the reason the `lentTo` map gives: the panel hides the
/// line when there is nothing to say, and a map of every account saying
/// "local" is a list the reader has to filter before it means anything.
fn peer_ls_exits(
    accounts: &[config::Account],
    peers: &[teamclaude_rs::peer::config::PeerRow],
    last_seen: &[(tcr_peer_wire::PeerId, i64)],
    now_ms: i64,
) -> std::collections::BTreeMap<String, status::PeerExitJson> {
    let mut out = std::collections::BTreeMap::new();
    for account in accounts {
        let pin = account.egress_pin();
        if pin.egress.is_local() && !pin.strict {
            continue;
        }
        let Ok(label) = tcr_peer_wire::sanitize_label(&account.name) else {
            // An account whose name is not a label still has an exit lock, but
            // there is no key to file it under and the panel matches its cards
            // by this name. Dropping it here is the one honest option; the pin
            // itself is unaffected, and `tcr status` still shows the account.
            continue;
        };
        let (peer_down, waiting_seconds) = match pin.egress.peer() {
            None => (false, None),
            Some(peer) => {
                let reachable = peers
                    .iter()
                    .any(|row| row.node == peer && row.has_endpoint());
                let waited = last_seen
                    .iter()
                    .find(|(node, _)| *node == peer)
                    .map(|(_, at)| *at)
                    .and_then(|at| u64::try_from(now_ms.saturating_sub(at).max(0) / 1_000).ok());
                (!reachable, if reachable { None } else { waited })
            }
        };
        out.insert(
            label,
            status::PeerExitJson {
                egress: pin.egress.to_string(),
                egress_strict: pin.strict,
                peer_down,
                waiting_seconds,
            },
        );
    }
    out
}

/// The row-level `until` and `ended`, over every lease this row is
/// party to: the operator's grants, the leases the ledger has actually minted
/// for it (`state.leases`), and the leases that Mac has granted THIS one
/// (`state.borrowed`), both sections of the same state file.
///
/// The ledger's rows are in here and not only the grants because the two can
/// disagree: a lease minted before the operator shortened the grant runs to
/// its own `until`, and a borrower that is still being served must not be
/// drawn as ended.
fn peer_ls_ends(
    row: &teamclaude_rs::peer::config::PeerRow,
    leases: &[teamclaude_rs::peer::state::LeaseRow],
    borrowed: &[teamclaude_rs::peer::state::BorrowedRow],
    now_s: u64,
) -> (Option<u64>, bool) {
    // (end, has it passed) for every lease this row is party to.
    let mut ends: Vec<(Option<u64>, bool)> = row
        .lend
        .iter()
        .map(|grant| (grant.until, grant.has_ended(now_s)))
        .collect();
    ends.extend(
        leases
            .iter()
            .filter(|held| held.peer == row.node)
            .map(|held| {
                (
                    held.lease.until,
                    held.lease.until.is_some_and(|until| until <= now_s),
                )
            }),
    );
    // The BORROWED side, on the same clock and the same `until`: a lease this
    // Mac holds FROM that row's Mac ends when the lender said it ends, and the
    // row renders "ends in 1 h" off the same number either way. Read from the
    // state file rather than from any running process, which is what makes it
    // answerable by a CLI invocation at all.
    ends.extend(
        borrowed
            .iter()
            .filter(|held| held.lender == row.node)
            .map(|held| {
                (
                    held.lease.until,
                    held.lease.until.is_some_and(|until| until <= now_s),
                )
            }),
    );

    let live: Vec<Option<u64>> = ends
        .iter()
        .filter(|(_, ended)| !ended)
        .map(|(until, _)| *until)
        .collect();
    if !live.is_empty() {
        // The soonest END among the live leases, and `None` when every live
        // lease is open-ended: a row with one lease that ends at 18:00 and one
        // that never ends has not ended at 18:00, but 18:00 is still the next
        // thing that happens to it.
        return (live.into_iter().flatten().min(), false);
    }
    if ends.is_empty() {
        // No lease at all is not an ended lease: `Add a lease…` is the zero
        // state, and a Mac that was never lent to must not be greyed.
        return (None, false);
    }
    (ends.iter().filter_map(|(until, _)| *until).max(), true)
}

/// `tcr peer id|ls|find|name|share|pair|invite|join|forget|allow|via`, the
/// LAN peer mesh.
///
/// A thin dispatcher, the same shape as [`run_group`]: every body belongs to the
/// phase that builds that verb, and the phase each one waits on is named in its
/// `todo!`. Reading this function tells you what the mesh will do; reading
/// `src/peer/` tells you what it has to hold while doing it.
async fn run_peer(args: peer_cli::PeerArgs) -> anyhow::Result<()> {
    use peer_cli::PeerAction;
    use teamclaude_rs::peer;

    match args.action {
        PeerAction::Id(a) => {
            let peers_path = a.peers.clone().unwrap_or_else(peer::config::default_path);
            let config_dir = peers_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(peer::id::default_config_dir);

            if a.regenerate {
                if !a.yes {
                    anyhow::bail!(
                        "--regenerate mints a NEW node key and evicts every peer that pinned \
                         this one; pass --yes to confirm"
                    );
                }

                // The peers about to be orphaned, read against the OLD key.
                // An unreadable peers file stops the regenerate outright, a
                // malformed or unowned file must not be silently read as "no
                // peers pinned" right before the key underneath them changes.
                let evicted = peer::config::PeerStore::open(&peers_path)?.peers();
                if evicted.is_empty() {
                    println!("no peers are pinned; regenerating evicts nobody");
                } else {
                    println!("regenerating evicts {} pinned peer(s):", evicted.len());
                    for row in &evicted {
                        println!("  {} ({})", masked_label(&row.label), row.node.display());
                    }
                }

                for path in [
                    peer::id::private_key_path(&config_dir),
                    peer::id::public_key_path(&config_dir),
                ] {
                    if path.exists() {
                        std::fs::remove_file(&path)
                            .with_context(|| format!("removing {}", path.display()))?;
                    }
                }
            }

            let key = peer::id::NodeKey::load_or_mint(&config_dir)?;
            println!("{}", key.id().display());
            Ok(())
        }
        PeerAction::Ls(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let store = peer::config::PeerStore::open(&peers_path)?;
            let rows = store.peers();
            let state_path = peer_state_path(&peers_path);
            let state = peer::state::load(&state_path, peer::pair::now_ms())?;

            if a.json {
                // The clock the two derived keys are answered against, taken
                // ONCE for the whole listing: two rows read a second apart
                // could otherwise disagree about whether the same 18:00 has
                // arrived.
                let now_s = u64::try_from(peer::pair::now_ms().max(0) / 1_000).unwrap_or(0);

                // The claimed name on a pending row is text a stranger on this
                // network chose. It was already sanitized on arrival; it is
                // masked again on the way out for the reason `masked_label`
                // gives, this repository is public and the state file is
                // hand-editable JSON.
                //
                // `visible_pending` and never `state.pending`: a reservation
                // placeholder holds an address's slot against the cap before
                // any handshake has earned it a name or an instance id, and
                // rendering it shows the operator a pairing request from
                // nobody that no Accept can complete. The row still counts
                // toward the cap in the listener. See
                // `PeerState::visible_pending`, which is why this is a second
                // reader and not a narrower field.
                let pending: Vec<peer::state::Knock> = state
                    .visible_pending()
                    .into_iter()
                    .map(pending_row_for_readers)
                    .collect();
                // The account labels every lease is measured against, and
                // their groups, because a `group:` scope is resolved by name
                // against the account's own group list.
                //
                // EVERY account is here, and that is the fix. This read
                // was `filter_map(|a| sanitize_label(&a.name).ok())`, so an
                // account whose own name is not a label (an email, a uuid)
                // was DROPPED from the map entirely and its card never showed
                // its "Lent to …" line, even under an `all` lease
                // that plainly lends it. Being un-nameable in a scope and
                // being un-lent are two different facts and the filter
                // conflated them.
                //
                // What is handed over is the name `tcr status` prints, which
                // is what the panel matches its account card against; `lent_to`
                // keys the map on it and sanitizes it itself for the scope
                // match, so an `account:<label>` scope still reaches exactly
                // the accounts a `--scope` could name and nothing else.
                //
                // A missing or unreadable config is an EMPTY map and not a
                // failure: `tcr peer ls` is about the peers file, and an
                // operator running it on a Mac with no accounts configured
                // still wants the pinned rows.
                let config_path = a.config.unwrap_or_else(config::default_path);
                let config_accounts: Vec<config::Account> = match config::load(&config_path) {
                    Ok(config) => config.accounts,
                    Err(err) => {
                        tracing::debug!(
                            error = %err,
                            path = %config_path.display(),
                            "peer ls: no account labels to key `lentTo` by"
                        );
                        Vec::new()
                    }
                };
                let accounts: Vec<(String, Vec<String>)> = config_accounts
                    .iter()
                    .map(|account| {
                        (
                            account.name.clone(),
                            account.groups.clone().unwrap_or_default(),
                        )
                    })
                    .collect();
                let lent_to = peer::lease::lent_to(&store, &accounts);
                let masked: Vec<status::PeerLsRow> = rows
                    .into_iter()
                    .map(|mut row| {
                        // Derived per grant and never read off the file, the
                        // rule `ended` already follows: a bearer's deadline is
                        // a fact about a credential and a clock.
                        for grant in &mut row.lend {
                            grant.handed_key_until =
                                peer_ls_handed_key_until(grant, &config_accounts, now_s);
                        }
                        let (until, ended) =
                            peer_ls_ends(&row, &state.leases, &state.borrowed, now_s);
                        status::PeerLsRow::from_row(row, until, ended)
                    })
                    .collect();
                let file = store.file();
                let exits = peer_ls_exits(
                    &config_accounts,
                    &file.peers,
                    &state.last_seen,
                    peer::pair::now_ms(),
                );
                let out = status::PeerLsJson {
                    supported: true,
                    peers: masked,
                    lent_to,
                    internet: file.internet,
                    network: status::network_fact::network_present(),
                    exits,
                    pending_count: pending.len(),
                    pending,
                    blocked_count: state.banned.len(),
                    blocked: state.banned.clone(),
                    muted_count: state.muted.len(),
                    muted: state.muted.clone(),
                    limited: 0,
                    caps: status::PeerCapsJson {
                        found_rows: peer::discovery::MAX_FOUND_ROWS,
                        found_per_address: peer::discovery::MAX_FOUND_PER_ADDRESS,
                        pending: peer::state::MAX_PENDING_KNOCKS,
                        knock_interval_ms: peer::listener::KNOCK_INTERVAL_MS,
                        knock_burst: peer::listener::KNOCK_BURST,
                        unauthenticated_sockets: peer::listener::MAX_UNAUTHENTICATED_SOCKETS,
                    },
                };
                println!("{}", serde_json::to_string(&out)?);
            } else {
                if rows.is_empty() {
                    println!("no peers pinned");
                }
                for row in &rows {
                    println!("{}  {}", row.node.display(), masked_label(&row.label));
                }
                // Greppable, `key: value`-shaped, and printed even at zero:
                // "pending=0" is a fact an operator can act on and a missing
                // line is one they have to go and check.
                println!(
                    "peer ls: pending={} blocked={} muted={}",
                    state.visible_pending().len(),
                    state.banned.len(),
                    state.muted.len()
                );
            }
            Ok(())
        }
        PeerAction::Find(a) => run_peer_find(a).await,
        PeerAction::Name(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            // THE LOCK, and the review's M3. `FileLock`'s own doc says it
            // "stops two `tcr` processes racing the same file, which is the real
            // case (a listener enrolling a joiner while the operator runs
            // `tcr peer allow`)", and that sentence was not true: every peers-file
            // mutation in this CLI was a bare read-modify-write, so an operator's
            // revoke and a listener's enrolment each wrote over the other's row,
            // silently, in a window of milliseconds. `accept_enrolment` takes the
            // same lock, which is what makes the pair exclusive.
            let _lock = peer::config::FileLock::acquire(&peers_path)?;
            let mut file = peer::config::read_or_default(&peers_path)?;

            match a.name {
                None => println!("{}", file.display_name()),
                Some(name) => {
                    let sanitized = tcr_peer_wire::sanitize_label(&name)
                        .with_context(|| format!("refusing name {name:?}"))?;
                    file.name = Some(sanitized.clone());
                    peer::config::save(&peers_path, &file)?;
                    println!("{sanitized}");
                }
            }
            Ok(())
        }
        PeerAction::Share(a) => {
            let peers_path = a.peers.clone().unwrap_or_else(peer::config::default_path);
            let _lock = peer::config::FileLock::acquire(&peers_path)?;
            let mut file = peer::config::read_or_default(&peers_path)?;
            if file.peers.is_empty() {
                println!(
                    "peer share: no peers are pinned, so there is nobody to share with \
                     (`tcr peer pair <host:port>` or `tcr peer invite` first)"
                );
                return Ok(());
            }

            let window = tcr_peer_wire::Window::from(a.window);
            match a.state {
                peer_cli::Switch::On => {
                    let scope = tcr_peer_wire::LendScope::parse(&a.scope)
                        .map_err(|refusal| anyhow::anyhow!("peer share: {refusal}"))?;
                    // Zero is a removal here for the same reason it is in `peer
                    // lend`: a grant that mints leases the first request
                    // overdraws is worse than no grant, because it advertises a
                    // capability. `inspect` is left exactly as it was, since
                    // removing a grant is not the moment to grant a disclosure.
                    if a.fraction <= 0.0 {
                        let mut removed = 0_usize;
                        for row in &mut file.peers {
                            let before = row.lend.len();
                            row.lend.retain(|existing| {
                                !(existing.window == window && existing.scope == scope)
                            });
                            removed += before - row.lend.len();
                        }
                        if file.default_lend.as_ref().is_some_and(|default| {
                            default.window == window && default.scope == scope
                        }) {
                            file.default_lend = None;
                        }
                        peer::config::save(&peers_path, &file)?;
                        println!(
                            "peer share: removed leases={removed} scope={scope} window={}",
                            peer_window_name(window)
                        );
                        println!(
                            "peer share: every peer that still holds `inspect` may still OPEN \
                             a serve; each request on it is refused for want of a grant \
                             (`tcr peer share off` closes the streams too)"
                        );
                        return Ok(());
                    }
                    // AN END THAT HAS ALREADY PASSED IS REFUSED, BEFORE
                    // ANYTHING IS WRITTEN. This verb keeps the end, the mode
                    // and the schedule an operator set per peer, which is
                    // right; what it did with a `--until` that has already gone
                    // by was carry it onto the new grant and print `on`. Every
                    // request against that grant is then refused for want of a
                    // live lending, and the only surface saying so is
                    // `peer lend --list`'s `ended=true`. `peer lend` cannot
                    // produce one at all (`parse_lend_end` rolls a time of day
                    // to tomorrow and refuses a duration that is not into the
                    // future), so this verb is the one that has to.
                    //
                    // Refused rather than silently cleared: the end is a
                    // decision the operator took, and a verb that quietly
                    // removes it lends past a deadline somebody meant.
                    let now_s = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())
                        .unwrap_or(0);
                    let closed: Vec<String> = file
                        .peers
                        .iter()
                        .filter(|row| {
                            row.lend.iter().any(|existing| {
                                existing.window == window
                                    && existing.scope == scope
                                    && existing.has_ended(now_s)
                            })
                        })
                        .map(|row| row.node.display())
                        .collect();
                    if !closed.is_empty() {
                        anyhow::bail!(
                            "peer share: the grant this would keep the end of has already \
                             ended on {} (scope={scope} window={}), so turning sharing on \
                             would write a grant that lends nothing and print success. Give \
                             it a new end first (`tcr peer lend <peer> --relend --for 2h`), \
                             or drop the end (`tcr peer lend <peer> --for none`)",
                            closed.join(", "),
                            peer_window_name(window)
                        );
                    }
                    let mut grant = peer_lend_grant(window, a.fraction, a.ttl, a.max_inflight)?;
                    grant.scope = scope.clone();
                    for row in &mut file.peers {
                        row.allow.inspect = true;
                        // What this row already granted on the same (window,
                        // scope), read BEFORE the retain below drops it.
                        //
                        // A hand grant, an end date and a daily window are
                        // decisions the operator took per peer, and this verb
                        // used to overwrite all three with `LendGrant::new`'s
                        // defaults: mode serve, no end, no schedule. Turning a
                        // hand grant into a serve grant is a different
                        // disclosure than the one that was taken, and it
                        // happened silently. `peer lend` already carries the
                        // same rule and says why.
                        let replacing = row
                            .lend
                            .iter()
                            .find(|existing| existing.window == window && existing.scope == scope)
                            .map(|existing| {
                                (
                                    existing.mode,
                                    existing.until,
                                    existing.between,
                                    existing.days.clone(),
                                )
                            });
                        // By (window, SCOPE), for the reason `peer lend` gives:
                        // a Mac may hold one lease per scope and turning
                        // sharing on with a scope must not delete the others.
                        row.lend.retain(|existing| {
                            !(existing.window == window && existing.scope == scope)
                        });
                        // A fresh id per row: a lease id is the handle for ONE
                        // peer's lease, and two peers sharing one id would make
                        // `--revoke` ambiguous.
                        let mut per_peer = grant.clone();
                        if let Some((mode, until, between, days)) = replacing {
                            per_peer.mode = mode;
                            per_peer.until = until;
                            per_peer.between = between;
                            per_peer.days = days;
                        }
                        per_peer.id = 0;
                        per_peer.ensure_id()?;
                        row.lend.push(per_peer);
                    }
                    // What the Sharing defaults sheet shows. A RECORD of the
                    // defaults, never a second enforcement point. See
                    // `PeerFile::default_lend`.
                    let mut default = grant.clone();
                    default.id = 0;
                    file.default_lend = Some(default);
                    // Sharing means those Macs reach this one, which needs a
                    // port. On this arm only: the removal above and the `off`
                    // arm below are not opt-ins and must open nothing.
                    let listen_written = ensure_listen_for_opt_in(&mut file);
                    peer::config::save(&peers_path, &file)?;
                    if let Some(chosen) = listen_written {
                        print_listen_written(chosen, &peers_path);
                    }
                    println!(
                        "peer share: on peers={} scope={} window={} fraction={} ttl_s={} \
                         max_inflight={}",
                        file.peers.len(),
                        grant.scope,
                        peer_window_name(window),
                        grant.fraction,
                        grant.ttl_s,
                        grant.max_inflight
                    );
                    println!(
                        "peer share: those Macs may now serve requests on this Mac's accounts, \
                         which means they READ those requests in full, prompts included"
                    );
                    // The other direction is a separate act on purpose. This verb
                    // says "my accounts may serve their requests"; `disclose` says
                    // "my requests may be read by them", and turning both on from
                    // one switch would grant a disclosure the operator did not ask
                    // for. Granting `inspect` alone is deliberate.
                    println!(
                        "peer share: to borrow the other way, grant it per peer: \
                         `tcr peer allow <peer> disclose on`"
                    );
                }
                peer_cli::Switch::Off => {
                    for row in &mut file.peers {
                        row.allow.inspect = false;
                        row.lend.clear();
                    }
                    peer::config::save(&peers_path, &file)?;
                    println!("peer share: off peers={}", file.peers.len());
                    println!(
                        "peer share: a live borrowed request dies within one frame, the \
                         listener re-reads this file and re-runs the gate before each one"
                    );
                }
            }
            Ok(())
        }
        PeerAction::Pair(a) => {
            use teamclaude_rs::peer::pair::PairEvent;

            /// Print one `--json` line, or nothing at all in prose mode.
            ///
            /// A function rather than a bare `println!` at four call sites:
            /// every arm below has to be either prose or JSON and never both,
            /// and the one that forgets is the one that puts an English
            /// sentence into a stream a parser is reading.
            fn announce(json: bool, event: &PairEvent) -> anyhow::Result<()> {
                if json {
                    println!("{}", event.line()?);
                }
                Ok(())
            }

            let path = a
                .peers
                .clone()
                .unwrap_or_else(teamclaude_rs::peer::config::default_path);
            let store = teamclaude_rs::peer::config::PeerStore::open(&path)?;
            // A bare host is taken too, and defaulted: see `peer_dial_addr`.
            // `tcr peer pending` prints a bare address for a knock that named
            // no port, and this command has to accept what that row printed.
            let addr: std::net::SocketAddr = peer_dial_addr(&a.addr)?;
            // **Phase one: knock.** This replaced the direct-`XX` dial.
            // The older `tcr peer pair` opened a
            // two-minute window on THIS Mac and dialled, so any host that
            // could reach this port during those two minutes got message 2 and
            // this node's static key with it. Now nothing is disclosed to
            // anyone until an operator at the far Mac has seen a request and
            // pressed Accept.
            let file = teamclaude_rs::peer::config::read_or_default(&path)?;
            let proposed_name = file.display_name();
            let instance = teamclaude_rs::peer::id::boot_instance_id();
            if let Err(err) = teamclaude_rs::peer::pair::knock(&store, addr, &proposed_name).await {
                announce(
                    a.json,
                    &PairEvent::Refused {
                        message: format!("{err:#}"),
                    },
                )?;
                return Err(err);
            }
            announce(
                a.json,
                &PairEvent::Asking {
                    addr: addr.to_string(),
                    instance: instance.to_string(),
                    wait_seconds: teamclaude_rs::peer::pair::PAIR_WAIT.as_secs(),
                },
            )?;
            if !a.json {
                println!(
                    "peer pair: asked {addr} to pair and waiting for them, as instance {instance}"
                );
                println!(
                    "peer pair: on that Mac, `tcr peer pending` shows the request and \
                     `tcr peer accept {instance}` approves it; nothing has been disclosed to it \
                     yet"
                );
            }

            // **Phase two: `XX`, once their Accept opens a window for this
            // instance id.** Retried, because approval can come minutes later.
            // See `pair::PAIR_WAIT`.
            let pending = match teamclaude_rs::peer::pair::pair(&store, addr).await {
                Ok(pending) => pending,
                Err(err) => {
                    // The ten-minute deadline lands here, and it is the one
                    // failure a panel MUST be able to draw: it is what "nobody
                    // was at the other Mac" looks like.
                    announce(
                        a.json,
                        &PairEvent::Refused {
                            message: format!("{err:#}"),
                        },
                    )?;
                    return Err(err);
                }
            };
            announce(
                a.json,
                &PairEvent::Comparing {
                    code: pending.code.clone(),
                },
            )?;
            if !a.json {
                println!("peer pair: this Mac shows {}", pending.code);
                println!(
                    "peer pair: the other Mac shows six digits for the same pairing; they must \
                     be identical, and a mismatch means a machine in the middle relayed it"
                );
            }
            let offered = match a.code.clone() {
                Some(code) => code,
                None => {
                    // The prompt is prose only. In `--json` the `comparing`
                    // line above IS the prompt, and a reader that got an
                    // English sentence on the same stream would have to
                    // decide which of the two to believe.
                    if !a.json {
                        println!("peer pair: type the digits the other Mac is showing:");
                    }
                    let mut line = String::new();
                    std::io::stdin()
                        .read_line(&mut line)
                        .context("peer pair: could not read the compared code")?;
                    line
                }
            };
            if !pending.matches(&offered) {
                let message = format!(
                    "peer pair: refused, {} here, {} there. A mismatch is the one signal \
                     this path exists to produce, so it is not a retry prompt",
                    pending.code,
                    offered.trim()
                );
                announce(
                    a.json,
                    &PairEvent::Refused {
                        message: message.clone(),
                    },
                )?;
                anyhow::bail!(message);
            }
            teamclaude_rs::peer::pair::confirm(&store, &pending.peer, &pending.code, Some(addr))?;
            announce(
                a.json,
                &PairEvent::Trusted {
                    peer: pending.peer.display(),
                },
            )?;
            if !a.json {
                println!(
                    "peer pair: trusted, and a bare pin can say hello and nothing else \
                     (`tcr peer allow` grants one thing at a time, with no restart)"
                );
                println!(
                    "peer pair: run the same command on the other Mac, pointed back here, so \
                     both sides hold a pin"
                );
            }
            Ok(())
        }
        PeerAction::Invite(a) => {
            let path = a
                .peers
                .clone()
                .unwrap_or_else(teamclaude_rs::peer::config::default_path);
            let store = teamclaude_rs::peer::config::PeerStore::open(&path)?;
            if let Some(id) = a.revoke {
                let removed = teamclaude_rs::peer::pair::revoke_invite(&store, id)?;
                println!(
                    "peer invite: {} revoke id={id}",
                    if removed { "ok" } else { "not-found" }
                );
                return Ok(());
            }
            let label = a.label.clone().unwrap_or_else(|| "joining-mac".to_string());
            let (invite, token) =
                teamclaude_rs::peer::pair::mint_invite(&store, &label, a.ttl, a.uses)?;
            println!(
                "peer invite: ok id={} label={} ttl_s={} uses={}",
                invite.id, invite.label, a.ttl, a.uses
            );
            println!("{}", token.to_token());
            // One line per address the key carries, in the order the joiner
            // will try them, so the operator sending this key can see which
            // paths their friend actually has. The key itself is unchanged by
            // what is printed here.
            for entry in &token.addrs {
                println!("peer invite: {} {}", entry.kind.label(), entry.addr);
            }
            if !token
                .addrs
                .iter()
                .any(|entry| entry.kind == teamclaude_rs::peer::pair::DialAddressKind::Internet)
            {
                println!(
                    "peer invite: this key carries no internet address, so a friend who is not \
                     on this network or this tailnet needs this Mac's router to forward the \
                     port; `tcr peer reach` reports where that stands"
                );
            }
            println!(
                "peer invite: this key is join-capable by anything that can read {} until it \
                 is used or expires, `tcr peer invite --revoke {}` ends it early",
                path.display(),
                invite.id
            );
            Ok(())
        }
        PeerAction::Join(a) => {
            let path = a
                .peers
                .clone()
                .unwrap_or_else(teamclaude_rs::peer::config::default_path);
            let store = teamclaude_rs::peer::config::PeerStore::open(&path)?;
            // Parsed once into one typed value, so no later line here has to
            // ask "link or key?" with a different test. Nothing below prints
            // any field of it: both halves are live secrets and a terminal
            // keeps scrollback.
            let source = if a.stdin {
                teamclaude_rs::peer::pair::TokenSource::Stdin
            } else {
                let Some(key) = a.key.as_deref() else {
                    anyhow::bail!(
                        "peer join: give the key or link as an argument, or pass --stdin and \
                         pipe it in (which keeps it out of `ps` and the shell history)"
                    );
                };
                teamclaude_rs::peer::pair::TokenSource::Argv(key)
            };
            let input = teamclaude_rs::peer::pair::read_join_input(source)?;

            // The network key first, because it is what a link with no join
            // key is FOR, and because setting it is what lets this Mac see the
            // office at all.
            //
            // A refusal here used to `bail!` the whole command, which meant a
            // link carrying both a network key and a join key paired nothing
            // at all when the network key alone was refused: the join token
            // was never even read. The refusal is now reported and the
            // command carries on to the join token below, so a link with
            // both keys still pairs on a Mac that keeps its own network key.
            let mut network_key_refusal: Option<String> = None;
            if let Some(network_key) = input.network_key() {
                let _lock = teamclaude_rs::peer::config::FileLock::acquire(&path)?;
                let mut file = teamclaude_rs::peer::config::read_or_default(&path)?;
                let replaced = file.network_key.is_some();
                // The same refusal `tcr peer network-key join` gives, in the
                // same words. This arm used to overwrite in silence, which made
                // one clicked link enough to cut a Mac off from the mesh it was
                // already on, and the documented promise that the key is not
                // replaced without asking was true of one command out of two.
                if replaced && !a.replace {
                    network_key_refusal = Some(format!(
                        "peer join: a network key is already set on this Mac, and this link \
                         carries another. Taking it cuts this Mac off from every Mac still \
                         holding the old one, {} pinned peer(s) here. Pass --replace to mean \
                         it, or `tcr peer network-key clear` first",
                        file.peers.len()
                    ));
                } else {
                    file.network_key = Some(network_key);
                    teamclaude_rs::peer::config::save(&path, &file)?;
                    println!(
                        "peer join: network key {} file={}",
                        if replaced { "replaced" } else { "set" },
                        path.display()
                    );
                }
            }

            let Some(token) = input.join_token() else {
                // No join token to fall back on: the network-key refusal, if
                // any, is the whole outcome, so it is the error.
                if let Some(refusal) = network_key_refusal {
                    anyhow::bail!(refusal);
                }
                println!(
                    "peer join: that link carries no join key, so nothing was paired, it \
                     sets the network key and stops there. Pair from either Mac with \
                     `tcr peer pair <host:port>`"
                );
                return Ok(());
            };
            // A join token exists, so this link still pairs even though its
            // network key was refused above: say so, in the same line shape
            // as the "ok" line below, rather than leaving the earlier
            // refusal looking like the whole outcome.
            if let Some(refusal) = &network_key_refusal {
                println!(
                    "{refusal} (this link also carries a join key, which was still used below)"
                );
            }
            let label = a.label.clone().unwrap_or_else(|| "this-mac".to_string());
            // The address that ANSWERED, not the first one in the key: a key
            // carries every address that Mac can be reached at, and the one
            // that worked is the only one worth printing back.
            let joined = teamclaude_rs::peer::pair::join(&store, token, &label).await?;
            println!("peer join: ok addr={} file={}", joined.addr, path.display());
            Ok(())
        }
        PeerAction::Forget(a) => {
            let path = a
                .peers
                .clone()
                .unwrap_or_else(teamclaude_rs::peer::config::default_path);
            let store = teamclaude_rs::peer::config::PeerStore::open(&path)?;
            let peer = tcr_peer_wire::PeerId::parse(&a.peer)
                .map_err(|refusal| anyhow::anyhow!("peer forget: {refusal}"))?;
            if !teamclaude_rs::peer::pair::forget(&store, &peer)? {
                println!("peer forget: not-found peer={}", a.peer);
                return Ok(());
            }
            println!("peer forget: ok peer={}", a.peer);
            println!(
                "peer forget: its next handshake fails the pin check before message 2, and any \
                 live session with it closes within one frame"
            );
            let file = teamclaude_rs::peer::config::read_or_default(&path)?;
            let relays = teamclaude_rs::peer::pair::peers_still_holding_relay(&file);
            if !relays.is_empty() {
                println!(
                    "peer forget: WARNING this does NOT revoke egress still reachable through \
                     {} peer(s) holding `forward`",
                    relays.len()
                );
                println!(
                    "peer forget: revoke that grant on the middle peer, or set maxHops 0, which \
                     makes revocation mesh-wide again"
                );
            }
            Ok(())
        }
        PeerAction::Hello(a) => {
            let path = a
                .peers
                .clone()
                .unwrap_or_else(teamclaude_rs::peer::config::default_path);
            let store = teamclaude_rs::peer::config::PeerStore::open(&path)?;
            let peer = tcr_peer_wire::PeerId::parse(&a.peer)
                .map_err(|refusal| anyhow::anyhow!("peer hello: {refusal}"))?;
            match teamclaude_rs::peer::serve::say_hello(&store, &peer).await? {
                Some(_theirs) => println!("peer hello: ok peer={}", a.peer),
                None => println!(
                    "peer hello: no-answer peer={} (not pinned, or nothing on its row answered)",
                    a.peer
                ),
            }
            Ok(())
        }
        PeerAction::Allow(a) => {
            let peers_path = a.peers.clone().unwrap_or_else(peer::config::default_path);
            let _lock = peer::config::FileLock::acquire(&peers_path)?;
            let mut file = peer::config::read_or_default(&peers_path)?;
            let peer_id = tcr_peer_wire::PeerId::parse(&a.peer)
                .map_err(|refusal| anyhow::anyhow!("peer allow: {refusal}"))?;
            let on = matches!(a.state, peer_cli::Switch::On);

            let Some(row) = file.peers.iter_mut().find(|row| row.node == peer_id) else {
                anyhow::bail!(
                    "peer allow: {} is not pinned here, and a grant for an unpinned peer \
                     would be a row nothing enforces (`tcr peer ls` lists the pinned ones)",
                    a.peer
                );
            };
            let grant = match a.grant {
                peer_cli::PeerGrant::Gateway => {
                    row.allow.gateway = on;
                    "gateway"
                }
                peer_cli::PeerGrant::Carry => {
                    row.allow.carry = on;
                    "carry"
                }
                peer_cli::PeerGrant::Forward => {
                    row.allow.relay = on;
                    "forward"
                }
                peer_cli::PeerGrant::Inspect => {
                    row.allow.inspect = on;
                    "inspect"
                }
                peer_cli::PeerGrant::Disclose => {
                    row.allow.allow_disclose = on;
                    "disclose"
                }
                peer_cli::PeerGrant::AcceptMove => {
                    row.allow.accept_move = on;
                    "accept-move"
                }
                peer_cli::PeerGrant::ControlBriefs => {
                    row.allow.control.briefs = on;
                    "control-briefs"
                }
                peer_cli::PeerGrant::ControlLendable => {
                    row.allow.control.lendable = on;
                    "control-lendable"
                }
                peer_cli::PeerGrant::ControlDiag => {
                    row.allow.control.diag = on;
                    "control-diag"
                }
            };
            peer::config::save(&peers_path, &file)?;
            println!(
                "peer allow: ok peer={} grant={grant} state={}",
                peer_id.display(),
                if on { "on" } else { "off" }
            );
            println!(
                "peer allow: in effect now, the listener re-reads {} whenever its mtime \
                 moves, so no restart and no cold prompt cache",
                peers_path.display()
            );
            Ok(())
        }
        PeerAction::Lend(a) => {
            let peers_path = a.peers.clone().unwrap_or_else(peer::config::default_path);
            let _lock = peer::config::FileLock::acquire(&peers_path)?;
            let mut file = peer::config::read_or_default(&peers_path)?;
            let peer_id = tcr_peer_wire::PeerId::parse(&a.peer)
                .map_err(|refusal| anyhow::anyhow!("peer lend: {refusal}"))?;
            let window = tcr_peer_wire::Window::from(a.window);

            // The scope FIRST, so a refused `--scope` spelling changes nothing
            // on disk: a lend that half-applied is worse than one that did not
            // run.
            let scope = tcr_peer_wire::LendScope::parse(&a.scope)
                .map_err(|refusal| anyhow::anyhow!("peer lend: {refusal}"))?;
            // The end, from whichever of the two flags was given.
            // One parser for both spellings, and the clock is the real one
            // here, `parse_lend_end`'s own gates inject theirs.
            let end_spec = a.for_.as_deref().or(a.until.as_deref());
            let end = match end_spec {
                Some(spec) => peer::lease::parse_lend_end(spec, time::OffsetDateTime::now_utc())?,
                None => None,
            };
            // The window and days, parsed here for the same
            // reason the scope is: a refused spelling must change nothing on
            // disk. Both are refused with the value named, a typo that
            // silently became "every hour" would read to the operator as a
            // schedule that does not work.
            let between = match a.between.as_deref() {
                Some(raw) => Some(
                    raw.parse::<peer::schedule::Between>()
                        .map_err(|refusal| anyhow::anyhow!("peer lend --between: {refusal}"))?,
                ),
                None => None,
            };
            let days = match a.days.as_deref() {
                Some(raw) => Some(
                    raw.parse::<peer::schedule::Days>()
                        .map_err(|refusal| anyhow::anyhow!("peer lend --days: {refusal}"))?,
                ),
                None => None,
            };

            let Some(row) = file.peers.iter_mut().find(|row| row.node == peer_id) else {
                anyhow::bail!(
                    "peer lend: {} is not pinned here, so a grant for it would be a row \
                     nothing enforces (`tcr peer ls` lists the pinned ones)",
                    a.peer
                );
            };

            // --list, --revoke and --relend read or edit the leases that are
            // already there and never mint one, so each returns before the
            // lend below.
            if a.list {
                if row.lend.is_empty() {
                    println!("peer lend: none peer={}", peer_id.display());
                }
                for grant in &row.lend {
                    println!(
                        "peer lend: lease={} peer={} scope={} window={} fraction={} ttl_s={} \
                         max_inflight={} until={} between={} days={} ended={}",
                        peer::config::lease_id_string(grant.id),
                        peer_id.display(),
                        grant.scope,
                        peer_window_name(grant.window),
                        grant.fraction,
                        grant.ttl_s,
                        grant.max_inflight,
                        grant.until.map_or("none".to_string(), |at| at.to_string()),
                        grant
                            .between
                            .map_or("any".to_string(), |window| window.to_string()),
                        grant
                            .days
                            .as_ref()
                            .map_or("any".to_string(), |days| days.to_string()),
                        grant.ended
                    );
                }
                return Ok(());
            }

            if let Some(raw) = a.revoke.as_deref() {
                let id = peer::config::parse_lease_id(raw)
                    .context("peer lend --revoke: that is not a lease id")?;
                let before = row.lend.len();
                row.lend.retain(|grant| grant.id != id);
                if row.lend.len() == before {
                    anyhow::bail!(
                        "peer lend: no lease {} on peer {} (`tcr peer lend {} --list`)",
                        peer::config::lease_id_string(id),
                        peer_id.display(),
                        a.peer
                    );
                }
                let left = row.lend.len();
                peer::config::save(&peers_path, &file)?;
                println!(
                    "peer lend: revoked lease={} peer={} leases_left={left}",
                    peer::config::lease_id_string(id),
                    peer_id.display()
                );
                println!(
                    "peer lend: a live borrowed request on it dies within one frame, the \
                     listener re-reads {} whenever its mtime moves",
                    peers_path.display()
                );
                return Ok(());
            }

            if let Some(raw) = a.relend.as_deref() {
                let id = peer::config::parse_lease_id(raw)
                    .context("peer lend --relend: that is not a lease id")?;
                let Some(grant) = row.lend.iter_mut().find(|grant| grant.id == id) else {
                    anyhow::bail!(
                        "peer lend: no lease {} on peer {} (`tcr peer lend {} --list`)",
                        peer::config::lease_id_string(id),
                        peer_id.display(),
                        a.peer
                    );
                };
                // The new end, or none at all. `ended` is derived on read and
                // never written by hand, so clearing `until` is the whole of
                // putting a lease back to work.
                grant.until = end;
                grant.ended = false;
                let (window, until) = (grant.window, grant.until);
                peer::config::save(&peers_path, &file)?;
                println!(
                    "peer lend: relent lease={} peer={} window={} until={}",
                    peer::config::lease_id_string(id),
                    peer_id.display(),
                    peer_window_name(window),
                    until.map_or("none".to_string(), |at| at.to_string())
                );
                return Ok(());
            }

            // A HAND grant is refused when every account it could draw on is
            // strictly pinned away from this Mac. The bearer would be handed to
            // a borrower that must send on its own IP, and a strict pin says
            // this account's requests leave through a named peer or not at all,
            // so every borrowed request would be refused by the exit lock. The
            // grant would look granted and buy nothing.
            //
            // Read BEFORE any write, for the same reason the scope spelling is:
            // a refusal must change nothing on disk.
            //
            // Not strict, or not pinned at all, is allowed and says so: a
            // non-strict pin falls back to local, which is exactly what a
            // hand-mode borrower does.
            if matches!(a.mode, Some(peer_cli::PeerLendMode::Hand)) {
                let config_path = a.config.clone().unwrap_or_else(config::default_path);
                let main = config::load(&config_path).with_context(|| {
                    format!(
                        "peer lend: --mode hand reads {} to check this scope's exit locks",
                        config_path.display()
                    )
                })?;
                let covered: Vec<&config::Account> = main
                    .accounts
                    .iter()
                    .filter(|account| {
                        let label =
                            tcr_peer_wire::sanitize_label(&account.name).unwrap_or_default();
                        peer::lease::scope_covers(
                            &scope,
                            &label,
                            account.groups.as_deref().unwrap_or(&[]),
                        )
                    })
                    .collect();
                if !covered.is_empty() && covered.iter().all(|account| account.cannot_be_handed()) {
                    anyhow::bail!(
                        "peer lend: every account this scope covers has egressStrict on, so a \
                         hand-mode grant would hand over a bearer the exit lock refuses to \
                         send with: the borrower sends from ITS own Mac, which a strict pin \
                         forbids whether it names this Mac or another. Lend it as `--mode \
                         serve`, or clear the pin on an account in {} first",
                        a.scope
                    );
                }
            }

            // The mode of the grant being replaced, read BEFORE the retain
            // below drops it. Editing a hand grant's fraction must not quietly
            // turn it back into a serve grant: that is a different disclosure
            // decision than the one the operator took, and the CLI would have
            // taken it for them silently.
            let replacing = row
                .lend
                .iter()
                .find(|existing| existing.window == window && existing.scope == scope)
                .map(|existing| existing.mode);
            let mode = a
                .mode
                .map(teamclaude_rs::peer::config::LendMode::from)
                .or(replacing)
                .unwrap_or_default();

            // A lease is replaced by (window, SCOPE) and not by window alone:
            // one Mac may hold several leases at once, one per
            // scope, so lending `group:work` must not silently delete the
            // `all` lease on the same window.
            row.lend
                .retain(|existing| !(existing.window == window && existing.scope == scope));
            // Zero is a removal, not a lease of nothing: a row that mints
            // leases the first request overdraws is worse than no row, because
            // it advertises a capability. `clamp_to_grant` refuses anything
            // under MIN_DEBIT anyway, so this says the same thing on disk.
            if a.fraction <= 0.0 {
                let inspect_still_on = row.allow.inspect;
                peer::config::save(&peers_path, &file)?;
                println!(
                    "peer lend: removed peer={} window={}",
                    peer_id.display(),
                    peer_window_name(window)
                );
                if inspect_still_on {
                    println!(
                        "peer lend: that peer still holds `inspect`, so it may still OPEN a \
                         SERVE, every request on it is refused for want of a grant \
                         (`tcr peer allow {} inspect off` closes the stream itself)",
                        peer_id.display()
                    );
                }
                return Ok(());
            }
            let mut grant = peer_lend_grant(window, a.fraction, a.ttl, a.max_inflight)?;
            grant.mode = mode;
            grant.scope = scope;
            grant.until = end;
            grant.between = between;
            grant.days = days;
            // Minted here rather than left to `save`, so the line below can
            // print the handle `--revoke` takes.
            grant.ensure_id()?;
            let printed = grant.clone();
            row.lend.push(grant);
            let leases = row.lend.len();
            peer::config::save(&peers_path, &file)?;
            println!(
                "peer lend: ok lease={} peer={} scope={} window={} fraction={} ttl_s={} \
                 max_inflight={} until={} between={} days={} leases={leases}",
                peer::config::lease_id_string(printed.id),
                peer_id.display(),
                printed.scope,
                peer_window_name(window),
                printed.fraction,
                printed.ttl_s,
                printed.max_inflight,
                printed
                    .until
                    .map_or("none".to_string(), |at| at.to_string()),
                printed
                    .between
                    .map_or("any".to_string(), |window| window.to_string()),
                printed
                    .days
                    .as_ref()
                    .map_or("any".to_string(), |days| days.to_string())
            );
            println!(
                "peer lend: a grant is a CEILING on one lease, not a promise, the lender's \
                 own guard band clamps it again at grant time and can leave nothing"
            );
            Ok(())
        }
        PeerAction::Via(a) => {
            // `auto`, `off`, or one pinned Mac. The word is parsed here so a
            // typo is refused before anything else happens, and a peer id is
            // checked against the pinned rows so `via <peer>` cannot name a
            // Mac this one has never trusted.
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            // The lock is taken BEFORE the read, because this is a
            // read-modify-write of the same file `tcr peer allow` and the
            // listener's enrolment write also round-trip: without it the last
            // writer wins and one of the two operators' choices disappears.
            let lock = peer::config::FileLock::acquire(&peers_path)?;
            let file = peer::config::read_or_default(&peers_path)?;
            let asked = match a.setup_timeout_ms {
                None => peer::egress::ViaSetting::parse(&a.target)?,
                Some(ms) => {
                    peer::egress::ViaSetting::parse(&a.target)?.with_setup_timeout_ms(ms)?
                }
            };
            if let peer::egress::ViaRoute::Pinned(peer_id) = &asked.route {
                let known = file.peers.iter().any(|row| row.node == *peer_id);
                if !known {
                    anyhow::bail!(
                        "peer via: {} is not a Mac this one has pinned, so there is nothing \
                         to route through (`tcr peer ls` prints the pinned ones)",
                        peer_id.display()
                    );
                }
            }

            let current = peer::egress::via_setting(&file);
            if asked == current {
                println!("peer via: {} (unchanged)", current.to_spec());
                println!(
                    "peer via: `auto` carries a request through a trusted Mac only after the \
                     direct path has failed with nothing having left this Mac, and the carry \
                     is blind, the credential stays inside this Mac's own TLS"
                );
                return Ok(());
            }
            // The whole write: one field on the struct every other `tcr peer`
            // mutation round-trips, so the choice survives the next `allow`,
            // `lend` or `name` instead of being dropped by it.
            let mut file = file;
            file.via = asked.clone();
            peer::config::save(&peers_path, &file)?;
            drop(lock);
            println!("peer via: ok {}", asked.to_spec());
            if let Some(ms) = a.setup_timeout_ms {
                println!("peer via: a carry gives a Mac {ms}ms to take it, then gives up");
            }
            match asked.route {
                peer::egress::ViaRoute::Off => println!(
                    "peer via: off, this Mac never routes out through a peer, and a request \
                     whose direct path fails gets exactly the answer it got before this \
                     feature existed"
                ),
                peer::egress::ViaRoute::Auto => println!(
                    "peer via: auto carries a request through a trusted Mac only after the \
                     direct path has failed with nothing having left this Mac, and the carry \
                     is blind, the credential stays inside this Mac's own TLS"
                ),
                peer::egress::ViaRoute::Pinned(peer_id) => println!(
                    "peer via: only {} is asked, and never any other Mac, a pin that stops \
                     answering means no carry at all, which is the point of pinning one",
                    peer_id.display()
                ),
            }
            println!("peer via: it takes effect on the next request; nothing needs a restart");
            Ok(())
        }
        PeerAction::Pending(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let state_path = peer_state_path(&peers_path);
            let now = peer::pair::now_ms();
            let state = peer::state::load(&state_path, now)?;
            // Every read surface in this verb reads `visible_pending`, for the
            // reason `PeerState::visible_pending` gives: a reservation
            // placeholder is a slot held against the cap, not a Mac the
            // operator saw ask.
            // `pending_row_for_readers` for the reason it gives: what a reader
            // of a pending row does with the address is dial it, so both
            // surfaces here print the port the knocker said to answer on, and
            // the name a stranger's Mac proposed is masked on the way out of
            // both, which this verb used to skip while `ls --json` did it.
            let pending: Vec<peer::state::Knock> = state
                .visible_pending()
                .into_iter()
                .map(pending_row_for_readers)
                .collect();

            if a.json {
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct PendingJson {
                    pending: Vec<peer::state::Knock>,
                    muted: Vec<peer::state::Mute>,
                    banned: Vec<peer::state::Ban>,
                }
                println!(
                    "{}",
                    serde_json::to_string(&PendingJson {
                        pending: pending.clone(),
                        muted: state.muted.clone(),
                        banned: state.banned.clone(),
                    })?
                );
                return Ok(());
            }

            if pending.is_empty() {
                println!("no Macs are asking to pair");
            }
            for knock in &pending {
                // `<name or address> wants to pair`, which is the row
                // `abuse-resistance.md` specifies, plus the instance id so the
                // operator can name it to `accept` unambiguously.
                println!(
                    "{}: pending: {} ({}) wants to pair  instance={} wire={}",
                    knock.addr,
                    knock.proposed_name.as_deref().unwrap_or(&knock.addr),
                    knock.addr,
                    knock.instance_id,
                    knock.wire_version
                );
            }
            for mute in &state.muted {
                println!("{}: muted: until={} unix-ms", mute.addr, mute.until_ms);
            }
            for ban in &state.banned {
                println!(
                    "{}: blocked: reason={} since={} unix-ms key={}",
                    ban.addr,
                    ban.reason,
                    ban.since_ms,
                    ban.key
                        .map(|key| key.display())
                        .unwrap_or_else(|| "not-learned".to_string())
                );
            }
            Ok(())
        }
        PeerAction::Accept(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let state_path = peer_state_path(&peers_path);
            let now = peer::pair::now_ms();
            let _lock = peer::config::FileLock::acquire(&state_path)?;
            let mut state = peer::state::load(&state_path, now)?;
            let Some(window) = state.accept_knock(&a.target, now, peer::pair::PAIRING_WINDOW_SECS)
            else {
                anyhow::bail!(
                    "peer accept: no pairing request matches {:?} (`tcr peer pending` lists \
                     them; a request expires on its own after ten minutes)",
                    a.target
                );
            };
            peer::state::save(&state_path, &state)?;
            println!(
                "peer accept: ok instance={} addr={} until={} unix-ms",
                window.instance_id, window.addr, window.until_ms
            );
            println!(
                "peer accept: that ONE Mac may now start a first pairing for {} seconds; \
                 both screens will show six digits and both of you press trust",
                peer::pair::PAIRING_WINDOW_SECS
            );
            Ok(())
        }
        PeerAction::Ignore(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let state_path = peer_state_path(&peers_path);
            let now = peer::pair::now_ms();
            let _lock = peer::config::FileLock::acquire(&state_path)?;
            let mut state = peer::state::load(&state_path, now)?;
            let addr = match state.find_pending(&a.target) {
                Some(knock) => knock.addr.clone(),
                // An address the operator wants quiet need not have a live
                // request: "make this stop" is a reasonable thing to type at a
                // row that just expired, and refusing it would send them to
                // `block` for something a mute answers.
                //
                // It does have to BE an address, though. Anything else used to
                // be stored verbatim, and the listener compares against the
                // bare IP a connection arrives on, so the mute was one nothing
                // could ever equal and the command printed ok.
                None if peer::state::PeerState::is_knock_address(&a.target) => a.target.clone(),
                None => anyhow::bail!(
                    "peer ignore: {:?} matches no pairing request and is not an address \
                     either (`tcr peer pending` lists the requests; an address is the bare \
                     IP a knock arrived from, with no port)",
                    a.target
                ),
            };
            state.mute(&addr, now);
            peer::state::save(&state_path, &state)?;
            println!(
                "peer ignore: ok addr={addr} muted_for_s={}",
                peer::state::MUTE_MS / 1000
            );
            println!("peer ignore: knocks from it get nothing until the mute lifts on its own");
            Ok(())
        }
        PeerAction::Block(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let state_path = peer_state_path(&peers_path);
            let now = peer::pair::now_ms();
            let _lock = peer::config::FileLock::acquire(&state_path)?;
            let mut state = peer::state::load(&state_path, now)?;
            let addr = match state.find_pending(&a.target) {
                Some(knock) => knock.addr.clone(),
                // Same rule as `ignore`, and it matters more here: a ban
                // recorded against something that is not an address is a ban
                // the listener's comparison never equals, and the command said
                // ok. See `PeerState::is_knock_address`.
                None if peer::state::PeerState::is_knock_address(&a.target) => a.target.clone(),
                None => anyhow::bail!(
                    "peer block: {:?} matches no pairing request and is not an address either \
                     (`tcr peer pending` lists the requests; an address is the bare IP a knock \
                     arrived from, with no port)",
                    a.target
                ),
            };
            // **Both halves, and this is the only moment the key half is
            // reachable.** A knock reveals no static key, so a request blocked
            // before it ever dialled can only be banned by address; one that
            // got as far as an `XX` handshake left its key on the accepted
            // window, which is kept after the window closes for exactly this
            // reason (`PeerState::expire`), and banning it is what stops a DHCP
            // move from undoing the block.
            let key = state.key_learned_at(&addr);
            state.ban(&addr, key, peer::state::BanReason::Blocked, now);
            peer::state::save(&state_path, &state)?;
            println!(
                "peer block: ok addr={addr} key={}",
                key.map(|key| key.display())
                    .unwrap_or_else(|| "not-learned".to_string())
            );
            if key.is_none() {
                println!(
                    "peer block: no handshake with it ever revealed a static key, so this \
                     blocks the ADDRESS only, a new address on the same Mac would get \
                     through"
                );
            }
            println!("peer block: `tcr peer unblock {addr}` lifts it; nothing else does");
            Ok(())
        }
        PeerAction::Unblock(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let state_path = peer_state_path(&peers_path);
            let now = peer::pair::now_ms();
            let _lock = peer::config::FileLock::acquire(&state_path)?;
            let mut state = peer::state::load(&state_path, now)?;
            if !state.unblock(&a.addr) {
                println!("peer unblock: not-found addr={}", a.addr);
                return Ok(());
            }
            peer::state::save(&state_path, &state)?;
            println!("peer unblock: ok addr={}", a.addr);
            Ok(())
        }
        PeerAction::NetworkKey(a) => run_peer_network_key(a),
        PeerAction::Link(a) => {
            let peers_path = a.peers.clone().unwrap_or_else(peer::config::default_path);
            let file = peer::config::read_or_default(&peers_path)?;
            let Some(network_key) = file.network_key else {
                anyhow::bail!(
                    "peer link: this Mac has no network key, and a share link is a network \
                     key (`tcr peer network-key set` mints one). Without one there is \
                     nothing for a link to carry, and `tcr peer invite` is the headless \
                     path on its own"
                );
            };
            let join = if a.invite {
                let store = peer::config::PeerStore::open(&peers_path)?;
                let label = a.label.clone().unwrap_or_else(|| "joining-mac".to_string());
                let (_invite, token) = peer::pair::mint_invite(
                    &store,
                    &label,
                    peer::pair::INVITE_DEFAULT_TTL_SECS,
                    1,
                )?;
                Some(token)
            } else {
                None
            };
            let link = peer::pair::ShareLink { network_key, join };
            println!("{}", link.to_link());
            if a.invite {
                println!(
                    "peer link: that link carries a ONE-USE join key good for {} seconds, \
                     treat it like a password in a chat window",
                    peer::pair::INVITE_DEFAULT_TTL_SECS
                );
            } else {
                println!(
                    "peer link: that link carries the network key alone. Opening it lets a \
                     Mac see and be seen on this mesh; it pairs nothing and grants nothing"
                );
            }
            Ok(())
        }
        PeerAction::Moved(a) => run_peer_moved(a),
        PeerAction::Reach(a) => run_peer_reach(a),
        PeerAction::Internet(a) => {
            let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
            let on = matches!(a.state, peer_cli::Switch::On);
            // A port first, because what `internet on` asks the router for is a
            // mapping TO the listener's port, and with none it used to write
            // the flag, print "no listener is configured" and leave the
            // operator with a switch that does nothing.
            //
            // In its own scope: `reach::set_internet` takes the peers-file lock
            // itself, and the lock is a lockfile, not a reentrant one, so a
            // hold still open here would make the whole verb wait out
            // `LOCK_WAIT_MS` and then refuse.
            if on {
                let written = {
                    let _lock = peer::config::FileLock::acquire(&peers_path)?;
                    let mut file = peer::config::read_or_default(&peers_path)?;
                    match ensure_listen_for_opt_in(&mut file) {
                        Some(chosen) => {
                            peer::config::save(&peers_path, &file)?;
                            Some(chosen)
                        }
                        None => None,
                    }
                };
                if let Some(chosen) = written {
                    print_listen_written(chosen, &peers_path);
                }
            }
            // The whole verb. The flag, the mapping delete and the refusal
            // wording live in `peer::reach`, so this arm is argv and nothing
            // else; see `reach::set_internet`.
            match peer::reach::set_internet(&peers_path, on)? {
                peer::reach::InternetSwitch::On {
                    listen_port: Some(port),
                } => println!(
                    "peer.internet: on (the listener on port {port} is mapped at boot and \
                     renewed every 30 minutes)"
                ),
                // Unreachable through this arm now, since the block above gives
                // this Mac a port before the switch is written. Kept because
                // the type says it can happen and a caller of `set_internet`
                // that is not this arm may still see it; it no longer tells
                // anybody to go and hand-edit a file.
                peer::reach::InternetSwitch::On { listen_port: None } => println!(
                    "peer.internet: on, but this Mac has no peer port, so there is nothing \
                     to map yet"
                ),
                peer::reach::InternetSwitch::Off { deleted: Some(_) } => {
                    println!("peer.internet: off (the router mapping was deleted)")
                }
                peer::reach::InternetSwitch::Off { deleted: None } => {
                    println!("peer.internet: off (there was no mapping to delete)")
                }
            }
            Ok(())
        }
        PeerAction::Account(a) => run_peer_account(a),
        PeerAction::Status(a) => run_peer_status(a).await,
        PeerAction::Graph(a) => run_peer_graph(a).await,
    }
}

/// `tcr peer status --json`: the peers block, off the RUNNING proxy.
///
/// # Why this is not `tcr peer ls`, and not `tcr status` either
///
/// `peer ls` projects the peers file and the state file, and answers with no
/// server at all. `tcr status --json` renders the ACCOUNTS array and has done
/// since long before the mesh existed; giving it a peers key would mean
/// turning an array into an object under every reader that already parses it.
/// This verb is the third question, what the process that is serving holds
/// for each pinned Mac, and it is the one the Peers tab's live half asks.
///
/// A proxy that is not running is a NON-ZERO exit and never an empty list. The
/// tab reads that as "keep the file half", which is the truth; an empty list
/// would say "you have no peers", which is not.
async fn run_peer_status(args: peer_cli::PeerStatusArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    let config = config::load(&config_path)
        .with_context(|| format!("peer status: could not read {}", config_path.display()))?;
    let peers = teamclaude_rs::cli::live_peers(&config)
        .await
        .map_err(|why| anyhow::anyhow!("peer status: {why}"))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "peers": peers }))?
        );
        return Ok(());
    }
    if peers.is_empty() {
        println!("peer status: none pinned");
    }
    for row in &peers {
        println!(
            "peer status: peer={} name={} address={} last_seen_ms={} paths={} trusted={}",
            // The short form: this is a line a person reads. The full key is
            // `id` on the `--json` payload, which is what a panel joins on.
            row.display,
            row.name,
            row.address.as_deref().unwrap_or("none"),
            row.last_seen_ms
                .map_or("never".to_string(), |at| at.to_string()),
            row.paths.len(),
            row.trusted
        );
    }
    Ok(())
}

/// `tcr peer graph`: one node per Mac, one edge per way to reach one, one edge
/// per live lease, as [`teamclaude_rs::status::peer_graph_block`] derives it
/// off the peers file and the runtime-state file beside it.
///
/// Read-only and local, the same contract `tcr peer reach` already gives:
/// nothing here opens a socket to a peer or to the running proxy, so asking
/// for a picture of the mesh cannot change it. `--serve` is the one exception
/// that opens a socket at all, and it is a LISTENING one, on loopback only.
async fn run_peer_graph(args: peer_cli::PeerGraphArgs) -> anyhow::Result<()> {
    let peers_path = args
        .peers
        .clone()
        .unwrap_or_else(peer::config::default_path);
    let config_dir = peers_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(peer::id::default_config_dir);
    let this = peer::id::NodeKey::load_or_mint(&config_dir)?.id();
    let this_label = peer::config::read_or_default(&peers_path)?.display_name();

    if args.serve {
        return serve_peer_graph(peers_path, this, this_label, &args.addr).await;
    }

    let graph =
        teamclaude_rs::status::peer_graph_block(&peers_path, &this, &this_label, now_unix_ms());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
        return Ok(());
    }

    println!(
        "peer graph: nodes={} edges={}",
        graph.nodes.len(),
        graph.edges.len()
    );
    for node in &graph.nodes {
        println!(
            "peer graph: node id={} name={} role={}",
            node.id,
            node.name,
            match node.role {
                teamclaude_rs::status::GraphRole::ThisMac => "self",
                teamclaude_rs::status::GraphRole::Peer => "peer",
            }
        );
    }
    for edge in &graph.edges {
        match &edge.detail {
            teamclaude_rs::status::GraphEdgeDetail::Path {
                endpoint,
                path_kind,
                rtt_ms,
                loss_pct,
                ..
            } => println!(
                "peer graph: edge path from={} to={} endpoint={} kind={:?} rtt_ms={} loss_pct={}",
                edge.from,
                edge.to,
                endpoint,
                path_kind,
                rtt_ms.map_or("unmeasured".to_string(), |v| v.to_string()),
                loss_pct.map_or("unmeasured".to_string(), |v| v.to_string()),
            ),
            teamclaude_rs::status::GraphEdgeDetail::Lease {
                lease_id, spent, ..
            } => println!(
                "peer graph: edge lease from={} to={} lease_id={} spent={:.2}",
                edge.from, edge.to, lease_id, spent
            ),
        }
    }
    Ok(())
}

/// Milliseconds since the unix epoch, the graph's own clock.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

/// `tcr peer graph --serve`: the inline HTML page from `GRAPH_PAGE_HTML`, and
/// the `/graph.json` it polls every 5 s, on ONE listener this verb binds
/// itself.
///
/// # Why loopback only
///
/// A graph carries every trusted Mac's address and, on a lease edge, its
/// spend, which nothing on this LAN needed to disclose to serve the mesh
/// before this verb existed. The mesh-served variant, any trusted Mac asking
/// any other's graph over the peer wire, is deferred; `docs/peers.md` says
/// so in its own paragraph. This verb binds ONLY an address whose
/// `ip().is_loopback()` is true and refuses to start on any other, so the
/// worst a operator can do by mistyping `--addr` is a refused start, never an
/// accidental LAN-wide disclosure surface.
async fn serve_peer_graph(
    peers_path: PathBuf,
    this: tcr_peer_wire::PeerId,
    this_label: String,
    addr: &str,
) -> anyhow::Result<()> {
    use axum::extract::{Request, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::middleware::{self, Next};
    use axum::response::{Html, IntoResponse, Json, Response};
    use axum::routing::get;
    use axum::Router;

    let socket: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("peer graph --serve: {addr:?} is not a socket address"))?;
    if !socket.ip().is_loopback() {
        anyhow::bail!(
            "peer graph --serve: {addr:?} is not loopback; this page carries every trusted \
             Mac's address and lease spend, and this verb refuses to serve it off 127.0.0.1 \
             or ::1"
        );
    }

    /// Is `host` (a `Host` header value, `:port` and IPv6 brackets included)
    /// a name for THIS machine's loopback interface?
    ///
    /// The bind check above only proves the SOCKET is loopback; DNS rebinding
    /// answers from that same socket to a request whose `Host` names a public
    /// domain that resolves to `127.0.0.1` in the victim's browser only.
    ///
    /// **The answer is the proxy's, not a second copy of it.** This verb held
    /// its own transcription of that check for a while, and two
    /// spellings of "is this loopback" is two things to get wrong on a gate
    /// whose whole job is refusing a name. `proxy::host_is_loopback` takes a
    /// host with no port, which is what `proxy::strip_port` is for and what
    /// the transcription had inlined.
    fn host_is_loopback(host: &str) -> bool {
        teamclaude_rs::proxy::host_is_loopback(teamclaude_rs::proxy::strip_port(host))
    }

    /// The one check every route on this listener passes before its handler
    /// runs: the request's `Host` header must name a loopback address.
    /// Without it, a page open in a browser on this Mac can be pointed at
    /// `http://evil.example:18081/` (a DNS name the attacker controls,
    /// resolved to this loopback socket) and the bind check above would wave
    /// it through, because the bind check only ever asked what socket this
    /// server listens on, never what host the request claims to be for.
    async fn require_loopback_host(headers: HeaderMap, req: Request, next: Next) -> Response {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok());
        match host {
            Some(host) if host_is_loopback(host) => next.run(req).await,
            _ => (
                StatusCode::FORBIDDEN,
                "peer graph --serve: this Host header does not name a loopback address",
            )
                .into_response(),
        }
    }

    #[derive(Clone)]
    struct GraphState {
        peers_path: PathBuf,
        this: tcr_peer_wire::PeerId,
        this_label: String,
    }

    async fn graph_json(State(state): State<GraphState>) -> impl IntoResponse {
        let graph = teamclaude_rs::status::peer_graph_block(
            &state.peers_path,
            &state.this,
            &state.this_label,
            now_unix_ms(),
        );
        Json(graph)
    }

    async fn graph_page() -> impl IntoResponse {
        Html(GRAPH_PAGE_HTML)
    }

    let state = GraphState {
        peers_path,
        this,
        this_label,
    };
    let app = Router::new()
        .route("/", get(graph_page))
        .route("/graph.json", get(graph_json))
        .with_state(state)
        .layer(middleware::from_fn(require_loopback_host));

    let listener = tokio::net::TcpListener::bind(socket)
        .await
        .with_context(|| format!("peer graph --serve: could not bind {socket}"))?;
    println!("peer graph: serving on http://{socket}/ (loopback only, ctrl-c to stop)");
    axum::serve(listener, app)
        .await
        .context("peer graph --serve: the server stopped")?;
    Ok(())
}

/// The inline page `tcr peer graph --serve` returns at `/`: no build step, no
/// framework, one `<script>` that polls `/graph.json` every 5 s and redraws
/// an SVG from what it gets back. The colour rule mirrors
/// `PeerFormat.pathLine` on the Peers tab (`apps/macos/Sources/TcrBarCore/PeerFormat.swift`):
/// an unmeasured path is grey, `0` loss is "no loss", and a measured loss is
/// its own percentage; the three loss bands below are this page's own
/// addition, since a page has room for colour a text sub-line does not.
const GRAPH_PAGE_HTML: &str = include_str!("peer_graph_page.html");

/// `tcr peer moved mint|open`: the sealed link one Mac sends one friend after
/// it changed networks.
///
/// Split out of [`run_peer`]'s match for the reason [`run_peer_network_key`]
/// is: a verb with its own sub-verbs, folded in, makes the arms that matter
/// harder to find.
///
/// # What a link can do to this Mac, and what makes that true
///
/// `open --yes` is the only writing path here, and what it writes goes through
/// [`teamclaude_rs::peer::discovery::admissible_moved_endpoints`] and then
/// [`teamclaude_rs::peer::config::observe_endpoints`], through nothing else.
/// That pair is what makes the bound true by construction rather than by
/// review: the second is the peers file's one endpoint writer and answers
/// `Ok(false)` for a peer it does not already hold, so a link can never pin a
/// Mac, un-forget one, or bring back a row an operator revoked; the first caps
/// how much of a row a link may ever own and leaves every stronger endpoint
/// alone, neither re-dated nor rewritten as a link's.
///
/// Nothing else on a row is reachable from here. No grant, no label, no
/// network key, no switch: `observe_endpoints` writes `PeerRow::endpoints` and
/// there is no second call.
///
/// # Two prefixes, one sentence each
///
/// A line this function wrote itself reads `peer moved: …`, the greppable
/// shape every other verb uses. A refusal that came out of
/// [`teamclaude_rs::peer::moved`] is printed in the module's own words, which
/// start `moved link:`, rather than re-spelled here: one sentence with two
/// spellings is two sentences that drift, and the prefix says which layer
/// refused.
fn run_peer_moved(args: peer_cli::PeerMovedArgs) -> anyhow::Result<()> {
    let peers_path = args
        .peers
        .clone()
        .unwrap_or_else(peer::config::default_path);
    match args.action {
        peer_cli::PeerMovedAction::Mint => run_peer_moved_mint(&args, &peers_path),
        peer_cli::PeerMovedAction::Open => run_peer_moved_open(&args, &peers_path),
    }
}

/// `tcr peer moved mint <peer>`: one link for one pinned Mac.
///
/// # Where the addresses come from, and the one that is left out
///
/// Two at most: the configured listen socket, which is what a peer on this LAN
/// acts on, and the router mapping a serving process holds, which is what a
/// peer off it acts on. The mapping is read off the runtime-state file the way
/// [`run_peer_reach`] reads it, and for the reason that function states: the
/// register [`teamclaude_rs::peer::reach::external_socket`] answers from is
/// filled by a keeper thread, a CLI process has run none, so asking it here
/// would silently ship the LAN socket alone on a Mac whose mapping is live. A
/// record past its deadline reads as no mapping.
///
/// `PeerRow::sees_us_at`, the address that peer last said it sees this Mac at,
/// is left out **on purpose**. It is written per completed session, and in the
/// case this feature exists for no session has completed since the move, so it
/// holds the address this Mac had BEFORE it moved. A link headed "here is
/// where I am now" carrying it would ship a known wrong answer.
fn run_peer_moved_mint(args: &peer_cli::PeerMovedArgs, peers_path: &Path) -> anyhow::Result<()> {
    if args.stdin {
        anyhow::bail!(
            "peer moved: `--stdin` is how `open` reads a link; `mint` takes the peer id to \
             seal for"
        );
    }
    let Some(target) = args.target.as_deref() else {
        anyhow::bail!(
            "peer moved: `mint` needs the Mac to seal for, in its full wire form, the `node` \
             field of `tcr peer ls --json`"
        );
    };
    let peer_id = tcr_peer_wire::PeerId::parse(target)
        .map_err(|refusal| anyhow::anyhow!("peer moved: {refusal}"))?;

    let file = peer::config::read_or_default(peers_path)?;
    let Some(row) = file.peers.iter().find(|row| row.node == peer_id) else {
        anyhow::bail!(
            "peer moved: that Mac is not pinned here, and a link for one this Mac does not \
             trust would have no key to be sealed under (`tcr peer ls` lists the pinned ones)"
        );
    };
    // The option is handed over whole. There is no default to reach for: a row
    // with no secret is the one case this verb cannot serve, and sealing under
    // thirty-two zero bytes instead would be a key anybody can guess.
    let keys = peer::moved::MovedKeys::for_row(row.rendezvous_secret.as_ref())
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;

    let now_ms = peer::pair::now_ms();
    let held = peer::state::load(&peer_state_path(peers_path), now_ms)
        .ok()
        .and_then(|state| state.mapping)
        .filter(|record| record.expires_at_ms > now_ms);

    let mut addresses: Vec<std::net::SocketAddr> = Vec::new();
    if let Some(listen) = file.listen {
        addresses.push(listen);
    }
    if let Some(record) = &held {
        match record.external_address.as_deref() {
            Some(text) => match text.parse::<std::net::SocketAddr>() {
                Ok(addr) => addresses.push(addr),
                // Said out loud rather than skipped: a held mapping this verb
                // could not read is the difference between a link a friend off
                // the LAN can act on and one they cannot.
                Err(why) => println!(
                    "peer moved: the held mapping's external address did not parse ({why}), \
                     so this link carries the listen socket alone"
                ),
            },
            None => println!(
                "peer moved: the router mapped a port and would not name its own external \
                 address, so this link carries the listen socket alone"
            ),
        }
    }
    let mut carried: Vec<std::net::SocketAddr> = Vec::new();
    for addr in addresses {
        if !carried.contains(&addr) {
            carried.push(addr);
        }
    }
    carried.truncate(peer::drop::MAX_RECORD_ENDPOINTS);
    if carried.is_empty() {
        anyhow::bail!(
            "peer moved: this Mac has no address to put in a link: no peer listener is \
             configured and no serving process holds a router mapping. `tcr peer reach` \
             says what this Mac can be reached on, and `tcr peer internet on` is what asks \
             the router for the second one"
        );
    }

    let config_dir = peers_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(peer::id::default_config_dir);
    let this = peer::id::NodeKey::load_or_mint(&config_dir)?.id();

    let record = peer::moved::MovedRecord {
        v: peer::moved::MOVED_VERSION,
        at: now_unix_secs(),
        eps: carried,
    };
    let link = peer::moved::mint_link(&keys, &this, &record)
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;

    println!("{link}");
    println!(
        "peer moved: sealed for {}; only that Mac can read it, and it goes stale in {} hours",
        masked_label(&row.label),
        peer::moved::MAX_MOVED_AGE.as_secs() / 3_600
    );
    println!(
        "peer moved: it carries {} address(es) and nothing else: it joins nothing, grants \
         nothing and pairs nothing",
        record.eps.len()
    );
    Ok(())
}

/// `tcr peer moved open [link]`: read a link somebody sent, and write only when
/// the operator said so.
///
/// # Every pinned row is tried, and they all answer alike
///
/// The link carries no peer id, so the row whose key opens it IS the answer.
/// What that costs is a loop, and the loop's one rule is that its refusal must
/// not depend on the rows it walked: a link forwarded into the wrong group chat
/// must teach its reader nothing about whose Mac it was for or whether this Mac
/// came close. Every key that did not open it answers
/// [`teamclaude_rs::peer::moved::MovedRefusal::NotForThisMac`], which is one
/// sentence naming nothing, and the only refusals that can outrank it are the
/// ones decided after a key DID open the bytes.
fn run_peer_moved_open(args: &peer_cli::PeerMovedArgs, peers_path: &Path) -> anyhow::Result<()> {
    let raw = match (args.stdin, args.target.as_deref()) {
        (true, Some(_)) => anyhow::bail!(
            "peer moved: the link goes on the command line or on standard input, not both"
        ),
        (true, None) => {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
                .context("peer moved: could not read the link from standard input")?;
            if line.trim().is_empty() {
                anyhow::bail!(
                    "peer moved: standard input carried no link (`--stdin` expects one line)"
                );
            }
            line
        }
        (false, Some(text)) => {
            // On stderr, so a panel or a script reading stdout sees the same
            // lines whichever way the link arrived.
            eprintln!(
                "peer moved: a link typed on the command line is visible in `ps` to every \
                 process on this Mac and in this shell's history; `--stdin` is the path that \
                 does not leak, and it is the one the panel uses"
            );
            text.to_string()
        }
        (false, None) => anyhow::bail!(
            "peer moved: `open` needs a link, on the command line or on standard input with \
             `--stdin`"
        ),
    };

    let field = peer::moved::link_field(&raw).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    let now_s = now_unix_secs();
    let now_ms = peer::pair::now_ms();
    let file = peer::config::read_or_default(peers_path)?;

    let mut opened = None;
    let mut past_the_seal = None;
    for row in &file.peers {
        // A row with no shared secret has no key to try, and that is not an
        // error: its own doc says absent means no session has completed since
        // the key existed.
        let Ok(keys) = peer::moved::MovedKeys::for_row(row.rendezvous_secret.as_ref()) else {
            continue;
        };
        match peer::moved::open_link_field(&keys, &row.node, field, now_s) {
            Ok(record) => {
                opened = Some((row, record));
                break;
            }
            // Everything except "this key did not open it" was decided either
            // before any key was tried, which makes it the same for every row,
            // or after one opened the bytes, which makes it that row's own
            // diagnosis. Either way it outranks the quiet refusal and stops the
            // walk.
            Err(peer::moved::MovedRefusal::NotForThisMac) => continue,
            Err(refusal) => {
                past_the_seal = Some(refusal);
                break;
            }
        }
    }

    let Some((row, record)) = opened else {
        let refusal = past_the_seal.unwrap_or(peer::moved::MovedRefusal::NotForThisMac);
        println!(
            "peer moved: nothing was written and no row was created; a link only ever adds \
             an address to a Mac this one already pins"
        );
        return Err(anyhow::anyhow!("{refusal}"));
    };

    let label = masked_label(&row.label);
    println!(
        "peer moved: from {label} ({}), sealed {}s ago",
        row.node.display(),
        now_s.saturating_sub(record.at)
    );

    let learned: Vec<peer::config::Endpoint> = record
        .eps
        .iter()
        .map(|addr| {
            peer::config::Endpoint::direct(*addr, now_ms, peer::config::EndpointSource::Moved)
        })
        .collect();
    let admissible = peer::discovery::admissible_moved_endpoints(row, &learned);
    // What is already on the row is dropped here rather than refreshed, which
    // is the whole of a second paste of one link costing nothing: the admission
    // rules refresh a locator this band already holds, and refreshing rewrites
    // the operator's file to move one timestamp. A person pasting the same link
    // twice is telling this Mac what it already knows.
    let fresh: Vec<peer::config::Endpoint> = admissible
        .into_iter()
        .filter(|endpoint| {
            !row.endpoints
                .iter()
                .any(|held| held.locator == endpoint.locator)
        })
        .collect();

    if fresh.is_empty() {
        let all_held = learned.iter().all(|endpoint| {
            row.endpoints
                .iter()
                .any(|held| held.locator == endpoint.locator)
        });
        if all_held {
            println!(
                "peer moved: already-known; that Mac is already reachable at everything this \
                 link says, so nothing was written"
            );
        } else {
            println!(
                "peer moved: nothing written; this row already holds the {} addresses a link \
                 may ever add to one, and the rest of its slots are for what this Mac proved",
                peer::discovery::MAX_MOVED_ENDPOINTS_PER_PEER
            );
        }
        return Ok(());
    }

    if !args.yes {
        for endpoint in &fresh {
            match endpoint.direct_addr() {
                Some(addr) => println!("peer moved: would add {addr}"),
                None => println!("peer moved: would add a path that is not a socket"),
            }
        }
        println!("peer moved: nothing written; pass --yes to keep these");
        return Ok(());
    }

    let node = row.node;
    let wrote = peer::config::observe_endpoints(peers_path, &node, &fresh)?;
    if !wrote {
        println!(
            "peer moved: that Mac has no row here now, so nothing was written; a link never \
             creates one"
        );
        return Ok(());
    }
    println!(
        "peer moved: added {} address(es) to {label}; nothing else changed",
        fresh.len()
    );
    Ok(())
}

/// `tcr peer reach`: the three ways a Mac off this LAN could reach this one.
///
/// Read-only by default, and synchronous, because every one of the three is a
/// local question: a route-table read, a route lookup on an unconnected socket,
/// and a UDP exchange with the router on the local link. Nothing here dials a
/// peer and nothing touches the running proxy.
///
/// # The three, and why the third has no number yet
///
/// A global IPv6 address needs no router cooperation at all. A NAT-PMP mapping
/// is the IPv4 answer, and a refusal is the ORDINARY outcome -- most routers
/// ship with port mapping off -- so every failure here prints and the verb
/// still exits 0. The third is the time-derived port
/// ([`teamclaude_rs::peer::reach::derived_port`]), which needs the secret both
/// ends share, and that secret is the pair's Noise handshake hash: **it is not
/// persisted anywhere today**, so the per-peer rows print the slot and say the
/// port is unavailable rather than inventing a number from public key material,
/// which a scanner could compute too.
fn run_peer_reach(args: peer_cli::PeerReachArgs) -> anyhow::Result<()> {
    use teamclaude_rs::peer::reach;

    /// How long a probe asks a mapping to be held for. Two minutes: long
    /// enough for the operator to test a dial, short enough that a forgotten
    /// probe leaves nothing open.
    const PROBE_LIFETIME_SECS: u32 = 120;

    let peers_path = args
        .peers
        .clone()
        .unwrap_or_else(peer::config::default_path);
    let file = peer::config::read_or_default(&peers_path)?;
    let store = peer::config::PeerStore::open(&peers_path)?;
    let rows = store.peers();

    // This process has run no session, so its reflexive register is empty and
    // every readout below would say "not told" on a Mac whose peers file
    // plainly records where it is seen. The rows carry both facts and the
    // serving process restores them at boot for the same reason; a CLI that
    // reports on reach restores them here.
    reach::restore_from_peers(&file);

    let v6 = reach::global_v6_addresses();
    let internal_port = file.listen.map(|listen| listen.port());
    let slot = reach::current_slot(now_unix_secs());

    // What a SERVING process recorded, read off the state file and never asked
    // of the router: a probe would change the mapping table it is reporting
    // on, and the keeper inside a running `tcr` already knows the answer.
    //
    // A record whose deadline has passed reads as no mapping. It is what a
    // process that died without deleting its mapping leaves behind, and
    // reporting it would tell an operator a port is open that nothing renews.
    let now_ms = peer::pair::now_ms();
    let held = peer::state::load(&peer_state_path(&peers_path), now_ms)
        .ok()
        .and_then(|state| state.mapping)
        .filter(|record| record.expires_at_ms > now_ms);

    // Every NAT-PMP outcome is a string, including the failures, because the
    // reader of this verb wants to see WHICH refusal the router gave.
    let client = reach::NatPmp::on_default_gateway();
    let (gateway, external, mapping) = match &client {
        Ok(client) => {
            let external = match client.external_address() {
                Ok(found) => format!("{}", found.addr),
                Err(err) => format!("unavailable: {err}"),
            };
            let mapping = match (args.map, internal_port) {
                (false, _) => "not asked for (pass --map)".to_string(),
                (true, None) => {
                    "no peer listener is configured, so there is no port to map".to_string()
                }
                (true, Some(port)) => {
                    // The same fallback the keeper runs, so this verb reports
                    // what `peer.internet on` would actually get rather than
                    // what NAT-PMP alone would: a router that speaks only UPnP
                    // is exactly the one an operator runs this verb about.
                    // Every UPnP refusal keeps its own name here,
                    // `UntrustedLocation`, `ControlUrlElsewhere` and
                    // `Redirected` included, because those three say a device
                    // answered and THIS NODE refused it, which "router did not
                    // answer" would hide.
                    let mut keeper = reach::MappingKeeper::new(*client, port, PROBE_LIFETIME_SECS)
                        .with_upnp(reach::upnp_discoverer());
                    match keeper.map() {
                        Ok(granted) => {
                            let over =
                                if keeper.steps().contains(&reach::MappingStep::MappedOverUpnp) {
                                    " (over upnp; nat-pmp did not answer)"
                                } else {
                                    ""
                                };
                            format!(
                                "tcp {} -> {} for {}s{over}",
                                granted.external_port, granted.internal_port, granted.lifetime_secs
                            )
                        }
                        Err(reach::ReachError::Silent { .. }) => {
                            "router did not answer".to_string()
                        }
                        Err(err) => format!("refused: {err}"),
                    }
                }
            };
            (format!("{}", client.gateway()), external, mapping)
        }
        Err(err) => (
            format!("unavailable: {err}"),
            "unavailable: no gateway".to_string(),
            "unavailable: no gateway".to_string(),
        ),
    };

    if args.json {
        let peers: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "label": masked_label(&row.label),
                    "node": row.node.display(),
                    "derivedPort": serde_json::Value::Null,
                    "derivedPortUnavailable": PORT_SECRET_NOT_PERSISTED,
                    // Where that Mac says it sees THIS one, and whether the
                    // two could punch. Both come out of `reach` rather than
                    // being derived here, so this readout and the dial cannot
                    // disagree about what "possible" means.
                    "seesUsAt": reach::observed_self_for(&row.node)
                        .map(|addr| addr.to_string()),
                    "punchPossible": reach::punch_target(&row.node).is_ok(),
                })
            })
            .collect();
        let payload = serde_json::json!({
            "ipv6": v6.iter().map(|addr| addr.to_string()).collect::<Vec<_>>(),
            "gateway": gateway,
            "externalAddress": external,
            "mapping": mapping,
            "listenPort": internal_port,
            "heldMapping": held,
            "slot": slot,
            "slotSeconds": reach::SLOT_SECONDS,
            "peers": peers,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if v6.is_empty() {
        println!(
            "reach: ipv6: none: this Mac has no globally routable IPv6 address, so a peer \
             cannot reach it over IPv6"
        );
    } else {
        for addr in &v6 {
            println!("reach: ipv6: {addr}");
        }
    }
    println!("reach: gateway: {gateway}");
    println!("reach: external-address: {external}");
    println!("reach: mapping: {mapping}");
    match internal_port {
        Some(port) => println!("reach: listen-port: {port}"),
        None => println!("reach: listen-port: none (the peer listener is off)"),
    }
    match &held {
        Some(record) => println!(
            "reach: held-mapping: {} -> {} until {} (recorded by the serving process, not \
             asked of the router)",
            record
                .external_address
                .clone()
                .unwrap_or_else(|| format!("port {}", record.external_port)),
            record.internal_port,
            record.expires_at_ms
        ),
        None => println!(
            "reach: held-mapping: none (no serving process on this Mac holds one right now)"
        ),
    }
    println!(
        "reach: slot: {slot} ({}s each, the slot before and after also accepted)",
        reach::SLOT_SECONDS
    );
    if rows.is_empty() {
        println!("reach: peers: none pinned");
    }
    for row in &rows {
        println!(
            "reach: peer: {} {}: derived-port: unavailable: {PORT_SECRET_NOT_PERSISTED}",
            masked_label(&row.label),
            row.node.display()
        );
        // The punch half, from `reach` and formatted there: one function so a
        // reader of this verb and a dial that punches cannot disagree.
        println!(
            "{}",
            reach::reach_punch_line(&masked_label(&row.label), &row.node)
        );
    }
    Ok(())
}

/// Why `tcr peer reach` prints no derived port, in the one wording both the
/// text and the JSON use.
const PORT_SECRET_NOT_PERSISTED: &str =
    "the pair's handshake secret is not stored, and deriving a port from public key \
     material instead would give every scanner the same number";

/// Seconds since the unix epoch, for the port slot.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// The runtime-state file that goes with a peers file.
///
/// Derived from the peers path rather than taken as a second argument, for the
/// reason `PeerIdArgs::peers` already gives for the node-key directory: one
/// `--peers` points the WHOLE peer surface at a temp dir, and a test that had
/// to remember a second flag would be a test that forgets it and reads the
/// operator's real cache directory.
fn peer_state_path(peers_path: &Path) -> PathBuf {
    // The DEFAULT peers file lives in the config directory and the state file
    // belongs in the cache directory, a split `state::default_path` already owns,
    // so the default path is never derived here, and only an explicit `--peers`
    // somewhere else moves the state file with it.
    if peers_path == peer::config::default_path() {
        return peer::state::default_path();
    }
    match peers_path.parent() {
        Some(parent) => parent.join("peer-state.json"),
        None => peer::state::default_path(),
    }
}

/// `tcr peer network-key set|join|clear|show`.
///
/// Split out of [`run_peer`]'s match for the same reason `run_peer_find` is: it
/// is a verb with its own sub-verbs, and folding four more arms into an
/// already-long match makes the one that matters harder to find.
fn run_peer_network_key(args: peer_cli::PeerNetworkKeyArgs) -> anyhow::Result<()> {
    use teamclaude_rs::peer;

    let peers_path = args
        .peers
        .clone()
        .unwrap_or_else(peer::config::default_path);
    let _lock = peer::config::FileLock::acquire(&peers_path)?;
    let mut file = peer::config::read_or_default(&peers_path)?;

    match args.action {
        peer_cli::PeerNetworkKeyAction::Show => {
            // The value is never printed, only its presence. `set` prints it
            // once at the moment the operator asked for it and nothing prints
            // it again: a key an operator can re-read from a terminal is a key
            // in every scrollback buffer on the Mac.
            println!(
                "peer network-key: {}",
                if file.network_key.is_some() {
                    "set"
                } else {
                    "not-set"
                }
            );
            Ok(())
        }
        peer_cli::PeerNetworkKeyAction::Set => {
            let replaced = file.network_key.is_some();
            // A key already here is not something to overwrite on the strength
            // of one word of argv: every Mac still holding it goes dark to this
            // one, and the refusal names how many that is.
            if replaced && !args.replace {
                anyhow::bail!(
                    "peer network-key: a key is already set on this Mac, and minting another \
                     cuts off every Mac still holding the old one, {} pinned peer(s) here, \
                     and any Mac whose beacon this one would stop verifying. Pass --replace to \
                     mean it, or `tcr peer network-key clear` first",
                    file.peers.len()
                );
            }
            let key = peer::config::NetworkKey::mint()?;
            file.network_key = Some(key);
            peer::config::save(&peers_path, &file)?;
            println!("{}", key.to_paste_string());
            println!(
                "peer network-key: {} file={}, this is the only time it is printed",
                if replaced { "replaced" } else { "set" },
                peers_path.display()
            );
            if replaced {
                println!(
                    "peer network-key: every Mac still holding the OLD key now sees nothing \
                     of this one and cannot knock at it; paste the new one on each"
                );
            }
            println!(
                "peer network-key: paste it on every other Mac with \
                 `tcr peer network-key join --stdin`, or share it as a link \
                 (`tcr peer link`)"
            );
            Ok(())
        }
        peer_cli::PeerNetworkKeyAction::Join => {
            let raw = if args.stdin {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
                    .context("peer network-key: could not read the key from standard input")?;
                line
            } else {
                let Some(key) = args.key else {
                    anyhow::bail!(
                        "peer network-key join: give the key as an argument, or pass --stdin \
                         and pipe it in (which keeps it out of `ps` and the shell history)"
                    );
                };
                key
            };
            if raw.trim().is_empty() {
                anyhow::bail!(
                    "peer network-key join: nothing was given (`--stdin` expects the key on \
                     one line)"
                );
            }
            // The refusal names the FIELD and its shape and never the value:
            // a paste that failed to parse is still a live shared secret.
            let key = peer::config::NetworkKey::try_from(raw)
                .map_err(|refusal| anyhow::anyhow!("{refusal}"))
                .context("peer network-key join: that is not 32 base32-encoded bytes")?;
            let replaced = file.network_key.is_some();
            // Same refusal as `set`, and for the same reason: pasting a second
            // office's key over the first is the commonest way a Mac
            // disappears from its own mesh.
            if replaced && !args.replace {
                anyhow::bail!(
                    "peer network-key: a key is already set on this Mac, and pasting another \
                     cuts off every Mac still holding the old one, {} pinned peer(s) here. \
                     Pass --replace to mean it, or `tcr peer network-key clear` first",
                    file.peers.len()
                );
            }
            file.network_key = Some(key);
            peer::config::save(&peers_path, &file)?;
            println!(
                "peer network-key: {} file={}",
                if replaced { "replaced" } else { "set" },
                peers_path.display()
            );
            Ok(())
        }
        peer_cli::PeerNetworkKeyAction::Clear => {
            if file.network_key.is_none() {
                println!("peer network-key: not-set (nothing to clear)");
                return Ok(());
            }
            file.network_key = None;
            peer::config::save(&peers_path, &file)?;
            println!("peer network-key: cleared file={}", peers_path.display());
            println!(
                "peer network-key: this Mac now sees every `tcr` on the network and they see \
                 it, the caps, the mutes, the blocks and the two-phase approval all still \
                 hold, and every pairing still needs you to press accept"
            );
            Ok(())
        }
    }
}

/// One lend grant, with the operator's numbers clamped and the clamp SAID.
///
/// The fraction ceiling is `0.5`, mirroring the clamp the main config already
/// applies to `controlReserve` (`src/config.rs:855-856`) rather than inventing a
/// second number for the same kind of quantity. A clamp is printed, never
/// silent: an operator who typed `0.9` and got `0.5` has to be able to see that
/// happen, or they will believe the file says something it does not.
fn peer_lend_grant(
    window: tcr_peer_wire::Window,
    fraction: f64,
    ttl_s: u32,
    max_inflight: u8,
) -> anyhow::Result<teamclaude_rs::peer::config::LendGrant> {
    // REFUSED, not clamped, and this is the only fraction that is. `NaN`
    // compares false against both of `clamp`'s bounds, so it passed through the
    // clamp below unchanged and printed nothing; `serde_json` then writes a
    // non-finite float as `null`, and `null` is not an `f64`, so the peers file
    // this command had just written would not load again on any build.
    if !fraction.is_finite() {
        anyhow::bail!(
            "peer lend: --fraction {fraction} is not a number this can lend; give a fraction \
             of the window between 0 and {MAX_LEND_FRACTION}"
        );
    }
    let clamped = teamclaude_rs::peer::config::lend_fraction(fraction);
    if (clamped - fraction).abs() > f64::EPSILON {
        println!(
            "peer lend: clamped fraction {fraction} to {clamped} (the ceiling on one lease is \
             {MAX_LEND_FRACTION}, the same clamp the main config applies to its own reserve)"
        );
    }
    Ok(teamclaude_rs::peer::config::LendGrant::new(
        window,
        clamped,
        ttl_s,
        max_inflight,
    ))
}

/// The ceiling on one lease's fraction, and it is the LIBRARY's constant: the
/// flag that accepts a fraction and the reader that parses one off disk answer
/// to one number, or a hand-edited file carries what this flag would have
/// refused.
const MAX_LEND_FRACTION: f64 = teamclaude_rs::peer::config::MAX_LEND_FRACTION;

#[cfg(test)]
mod peer_lend_fraction_tests {
    use super::{peer_lend_grant, MAX_LEND_FRACTION};
    use teamclaude_rs::peer::config::LendGrant;

    /// **A fraction that is not a number is refused at the flag**, and the
    /// ordinary over-large one is still clamped and still written.
    ///
    /// `--fraction nan` passed the clamp in silence, because `NaN` compares
    /// false against both bounds and `(NaN - NaN).abs() > EPSILON` is false
    /// too, so nothing was printed and nothing was capped. The second half
    /// below is what that cost: `serde_json` writes a non-finite float as
    /// `null`, `LendGrant::fraction` is not an `Option`, so the peers file the
    /// command had just written would not load again, on this build or any
    /// other.
    ///
    /// Watch it fail by deleting the `is_finite` refusal in `peer_lend_grant`.
    #[test]
    fn a_fraction_that_is_not_a_number_is_refused_at_the_flag() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                peer_lend_grant(tcr_peer_wire::Window::SevenDay, bad, 300, 2).is_err(),
                "--fraction {bad} has to be refused, not written"
            );
        }

        let clamped = peer_lend_grant(tcr_peer_wire::Window::SevenDay, 0.9, 300, 2)
            .expect("an over-large fraction is an ordinary clamp and not a refusal");
        assert_eq!(clamped.fraction, MAX_LEND_FRACTION);

        // What the refusal above prevents, measured rather than asserted from
        // the documentation.
        let planted = LendGrant {
            fraction: f64::NAN,
            ..LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.2, 300, 2)
        };
        let written = serde_json::to_string(&planted).expect("a grant serializes");
        assert!(
            written.contains("\"fraction\":null"),
            "a non-finite fraction lands on disk as null: {written}"
        );
        assert!(
            serde_json::from_str::<LendGrant>(&written).is_err(),
            "and a file carrying it does not load again: {written}"
        );
    }
}

/// The wire's own name for a window, so a printed line and a JSON field cannot
/// disagree about what `7d` is called.
fn peer_window_name(window: tcr_peer_wire::Window) -> &'static str {
    match window {
        tcr_peer_wire::Window::FiveHour => "5h",
        tcr_peer_wire::Window::SevenDay => "7d",
        tcr_peer_wire::Window::SevenDayOi => "7d_oi",
        tcr_peer_wire::Window::Unknown => "unknown",
    }
}

/// The one sanitizer every peer label passes through, applied a SECOND time
/// on the way out of `tcr peer ls --json` and `tcr peer id --regenerate`'s
/// eviction list.
///
/// Every entry point that ACCEPTS a label already refuses one that fails
/// [`tcr_peer_wire::sanitize_label`] (an `@`, a UUID shape, an organization
/// name, over-length), so a row read back from a file this program wrote
/// should always already be clean. This repository is public, though, and the
/// peers file is hand-editable JSON: a corrupt or hand-planted row must not
/// reach a fixture or a screen unmasked just because it skipped the accepting
/// check. A label that fails re-sanitizing is replaced outright rather than
/// partially redacted, because a partial redaction ("al***@example.com") is
/// itself a disclosure of the shape it is trying to hide.
fn masked_label(label: &str) -> String {
    tcr_peer_wire::sanitize_label(label).unwrap_or_else(|_| "[masked]".to_string())
}

/// One pending row as a READER of it wants it: `addr` carrying the port the
/// knocker said to answer on, when it said one, and the proposed name masked.
///
/// The row in the file keeps the bare IP, which is the key the mutes, the bans
/// and the accepted windows are all matched on
/// (`peer::state::Knock::addr`). What comes out of `tcr peer ls --json` and
/// `tcr peer pending` is the address to DIAL, because that string is handed
/// straight to `tcr peer pair`, by an operator reading a terminal and by the
/// panel's own Accept. Printing the key there sent every answer to the default
/// port.
///
/// **The name is masked here and not at one call site.** A proposed name is
/// text a stranger's Mac chose, already sanitized on arrival, and masked again
/// on the way out for the reason [`masked_label`] gives: this repository is
/// public and the state file is hand-editable JSON. `ls --json` masked it and
/// `pending` did not, which is one rule with two answers, and the surface that
/// skipped it is the one a menu bar and a notification banner now read.
///
/// One function for both surfaces rather than the same two maps twice: the
/// panel and the terminal have to be told the same address and the same name,
/// and the copy that drifts is the one nobody runs.
fn pending_row_for_readers(mut knock: peer::state::Knock) -> peer::state::Knock {
    knock.addr = knock.dial_address();
    knock.proposed_name = knock.proposed_name.as_deref().map(masked_label);
    knock
}

/// What `tcr peer pair` dials, from what an operator typed.
///
/// A `host:port` is taken as it is. A BARE address gets the port this Mac's
/// own default listener uses, because that is the only port a knock with no
/// port in it could have come from, and it is what every answer dialled before
/// a knock carried a port at all. `tcr peer pending` prints a bare address on
/// exactly that row, and a command that refused what the row beside it prints
/// sends the operator to look up a number the file already knows.
fn peer_dial_addr(text: &str) -> anyhow::Result<std::net::SocketAddr> {
    let text = text.trim();
    if let Ok(addr) = text.parse::<std::net::SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(host) = text.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(
            host,
            peer::config::default_listen().port(),
        ));
    }
    anyhow::bail!("peer pair: {text} is not a host:port, or a bare host, to dial")
}

#[cfg(test)]
mod peer_dial_addr_tests {
    use super::peer_dial_addr;

    #[test]
    fn a_host_and_port_is_dialled_exactly_as_typed() {
        let addr = peer_dial_addr("192.0.2.10:7766").expect("a host:port parses");
        assert_eq!(addr.port(), 7766);
        assert_eq!(addr.ip().to_string(), "192.0.2.10");
    }

    #[test]
    fn a_bare_host_takes_the_default_listen_port() {
        let addr = peer_dial_addr("192.0.2.10").expect("a bare host parses");
        assert_eq!(
            addr.port(),
            teamclaude_rs::peer::config::default_listen().port(),
            "a knock that named no port is answered where the default listener is"
        );
    }

    #[test]
    fn a_bracketed_ipv6_host_and_port_is_dialled_as_typed() {
        let addr = peer_dial_addr("[2001:db8::1]:7766").expect("a bracketed v6 host:port parses");
        assert_eq!(addr.port(), 7766);
    }

    #[test]
    fn a_bare_ipv6_host_takes_the_default_listen_port() {
        let addr = peer_dial_addr("2001:db8::1").expect("a bare v6 host parses");
        assert_eq!(
            addr.port(),
            teamclaude_rs::peer::config::default_listen().port()
        );
    }

    #[test]
    fn a_name_that_is_no_address_at_all_is_refused_rather_than_guessed() {
        let err = peer_dial_addr("not-an-address").expect_err("a hostname is not dialled");
        assert!(
            format!("{err:#}").contains("is not a host:port"),
            "the refusal names what was wrong with it: {err:#}"
        );
    }
}

#[cfg(test)]
mod peer_ls_masking_tests {
    use super::masked_label;

    #[test]
    fn an_email_a_uuid_and_an_org_name_come_out_masked() {
        assert_eq!(masked_label("alice@example.com"), "[masked]");
        assert_eq!(
            masked_label("11111111-1111-1111-1111-111111111111"),
            "[masked]"
        );
        // "Acme Corp!" carries a character the sanitizer's whitelist refuses,
        // the same defence that would refuse a known organization name,
        // since this crate holds no live org list to match against.
        assert_eq!(masked_label("Acme Corp!"), "[masked]");

        // A plain label is untouched, so the mask is not swallowing everything,
        // the failure mode a lone masking test cannot see.
        assert_eq!(masked_label("studio-mac"), "studio-mac");
    }
}

/// `tcr peer find on|off [--announce-name on|off]`, one switch that starts or
/// stops both [`peer::discovery::advertise`] and [`peer::discovery::browse`].
///
/// An earlier version of this function wrote its own
/// `tcr-peers.json` read/write and its own default-path/default-name
/// fallbacks because `PeerStore::open`, `peer::config::default_path` and
/// `PeerFile::display_name` were `todo!()` at the time. Phase 1 landed, so
/// this now sits entirely on [`peer::config`]'s real API: no second reader,
/// no second writer, no second host-name fallback.
async fn run_peer_find(args: peer_cli::PeerFindArgs) -> anyhow::Result<()> {
    run_peer_find_with(args, &RealPeerFinder).await
}

/// The one "start" call `find on` makes from the CLI: begin browsing.
///
/// A trait so [`find_on_starts_a_browse`] below can prove it fired without a
/// real mDNS daemon behind it, [`RealPeerFinder`] is the only production
/// implementation and just delegates to [`peer::discovery::browse`].
///
/// **Announcing is NOT here any more.** This command used to register the
/// beacon in this process and exit, which took the mdns-sd daemon, and the
/// beacon, down with the command; and the beacon carried this process's
/// per-boot instance id, which the server's own knocks could never match. The
/// server announces (`server::spawn_beacon`), and what this command does is
/// write the flag the server reads.
trait PeerFinder {
    async fn start_browse(&self, store: &peer::config::PeerStore) -> anyhow::Result<()>;
}

/// The real mesh.
struct RealPeerFinder;

impl PeerFinder for RealPeerFinder {
    async fn start_browse(&self, store: &peer::config::PeerStore) -> anyhow::Result<()> {
        // `find on` proves browsing works and discards the one scan's
        // results here: nothing renders a discovered list yet (the panel
        // does that), so there is nothing
        // useful to do with them beyond confirming the call started.
        peer::discovery::browse(store).await.map(|_found| ())
    }
}

/// Give this Mac a peer port when the operator turns one of the three features
/// on and there is none yet. Returns the address written, or `None` when
/// `listen` was already set, which this never touches.
///
/// The caller writes the file and then prints [`print_listen_written`]: the
/// three verbs that call this already hold the peers-file lock and already have
/// a save of their own, and a second write here would be a second writer for
/// one key.
///
/// It is called on the ON arm only. Turning something OFF, or removing a grant,
/// is not an opt-in and must not open a port.
fn ensure_listen_for_opt_in(file: &mut peer::config::PeerFile) -> Option<std::net::SocketAddr> {
    if file.listen.is_some() {
        return None;
    }
    let chosen = peer::config::default_listen();
    file.listen = Some(chosen);
    Some(chosen)
}

/// What every verb that just wrote a peer port says, in one place so the three
/// of them say the same thing.
///
/// The second line is the honest half. The peer socket is bound once, at boot,
/// from the file as it was then (`src/server.rs`, the `file.listen` read before
/// `listener::bind`); `PeerStore::reload_if_changed` re-reads the policy half
/// of this file whenever its mtime moves but nothing re-binds a socket, so a
/// port written now is a port that opens at the next start and not before.
fn print_listen_written(addr: std::net::SocketAddr, peers_path: &std::path::Path) {
    println!(
        "peer.listen: {addr} written into {} (this Mac had no peer port, and turning this on \
         needs one)",
        peers_path.display()
    );
    println!(
        "peer.listen: nothing is listening on it yet. The port opens the next time the proxy \
         starts, so quit TcrBar and open it again to finish turning this on"
    );
}

async fn run_peer_find_with(
    args: peer_cli::PeerFindArgs,
    finder: &impl PeerFinder,
) -> anyhow::Result<()> {
    let peers_path = args.peers.unwrap_or_else(peer::config::default_path);
    let _lock = peer::config::FileLock::acquire(&peers_path)?;
    let mut file = peer::config::read_or_default(&peers_path)?;

    // The flag flips the config key on BOTH arms. It used to be read only
    // inside the `on` arm, so `tcr peer find off --announce-name on` accepted
    // the flag, wrote the file without it and still exited 0, a silently
    // ignored preference, which is worse than a refusal. The flag is a stored
    // preference and does not depend on which way discovery is being set.
    if let Some(state) = args.announce_name {
        file.announce_name = matches!(state, peer_cli::Switch::On);
    }

    // What this run wrote into `listen`, if anything. Said after the save and
    // not before it, because a scan that fails exits non-zero having written
    // nothing, and a line promising a port in that case would be a lie.
    let mut listen_written = None;

    match args.state {
        peer_cli::Switch::Off => {
            file.discovery = false;
            // The flag, and nothing else: the beacon lives in the serving
            // process, which reads this file and withdraws it. A `stop_all`
            // here would reach only this command's own (empty) daemon.
            println!("peer.find: off");
            println!("a running server stops announcing within a minute");
        }
        peer_cli::Switch::On => {
            file.discovery = true;

            // This used to refuse here and send the operator to a text editor
            // for a key no walkthrough names, which stopped a first-time
            // reader at their very first command. Finding peers needs a port
            // to announce, so the verb provides one instead of asking for it.
            listen_written = ensure_listen_for_opt_in(&mut file);

            let store = peer::config::PeerStore::open(&peers_path)?;
            // One scan, so `find on` reports what is already on the LAN. The
            // ANNOUNCING half is the server's: see [`PeerFinder`].
            finder.start_browse(&store).await?;
            println!("peer.find: on (announceName={})", file.announce_name);
            // Only when there was already a port. A server that is running
            // right now has no peer socket for one this command just wrote, so
            // promising it would announce within a minute is exactly the claim
            // `print_listen_written` exists to correct.
            if listen_written.is_none() {
                println!("a running server starts announcing within a minute");
            }
        }
    }

    peer::config::save(&peers_path, &file)?;
    if let Some(chosen) = listen_written {
        print_listen_written(chosen, &peers_path);
    }
    Ok(())
}

#[cfg(test)]
mod peer_find_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Records that its call happened; no real mDNS daemon, no real `browse`.
    #[derive(Default)]
    struct FakePeerFinder {
        browsed: AtomicBool,
    }

    impl PeerFinder for FakePeerFinder {
        async fn start_browse(&self, _store: &peer::config::PeerStore) -> anyhow::Result<()> {
            self.browsed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// `find on` writes the flag and scans once, and announces NOTHING from
    /// this process: the beacon belongs to the server, which is the only
    /// process that outlives the command and the only one whose instance id a
    /// neighbour's knock can name.
    ///
    /// Watch it fail: comment out the `finder.start_browse(&store).await?;`
    /// line above and this goes red on the browse assertion, confirmed by
    /// hand while writing this test.
    #[tokio::test]
    async fn find_on_writes_the_flag_and_scans_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        let seed = peer::config::PeerFile {
            listen: Some("127.0.0.1:0".parse().expect("valid socket addr")),
            ..Default::default()
        };
        peer::config::save(&peers_path, &seed).expect("seed peers file");

        let finder = FakePeerFinder::default();
        run_peer_find_with(
            peer_cli::PeerFindArgs {
                peers: Some(peers_path.clone()),
                state: peer_cli::Switch::On,
                announce_name: None,
            },
            &finder,
        )
        .await
        .expect("run_peer_find_with(on)");

        assert!(
            finder.browsed.load(Ordering::SeqCst),
            "find on must scan once"
        );
        assert!(
            peer::config::read_or_default(&peers_path)
                .expect("the peers file reads back")
                .discovery,
            "and it must write the flag the server reads, which is the whole of what turns \
             announcing on"
        );
    }

    /// `find off` starts neither: the default state, and the one documented
    /// as "peer.find off (the default) registers nothing".
    #[tokio::test]
    async fn find_off_starts_neither() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");

        let finder = FakePeerFinder::default();
        run_peer_find_with(
            peer_cli::PeerFindArgs {
                peers: Some(peers_path),
                state: peer_cli::Switch::Off,
                announce_name: None,
            },
            &finder,
        )
        .await
        .expect("run_peer_find_with(off)");

        assert!(
            !finder.browsed.load(Ordering::SeqCst),
            "find off must not browse"
        );
    }
}

#[cfg(test)]
mod peer_grant_tests {
    use super::*;

    /// One pinned peer and nothing else: what `share`, `allow` and `lend` all
    /// need before they will write anything, and what the operator has after
    /// `tcr peer pair`.
    ///
    /// Obviously fake node id; this repository is public and a fixture is the
    /// easiest place to leak a real one.
    const PINNED: [u8; 32] = [4_u8; 32];

    fn pinned_peers_file(dir: &Path) -> PathBuf {
        let peers_path = dir.join("tcr-peers.json");
        let file = peer::config::PeerFile {
            peers: vec![peer::config::PeerRow {
                node: tcr_peer_wire::PeerId(PINNED),
                label: "studio-mac".to_string(),
                endpoints: vec![peer::config::Endpoint::direct(
                    "127.0.0.1:1"
                        .parse()
                        .expect("a literal socket address parses"),
                    0,
                    peer::config::EndpointSource::Paired,
                )],
                added_at: 0,
                rendezvous_secret: None,
                sees_us_at: None,
                allow: peer::config::Allow::default(),
                lend: Vec::new(),
            }],
            ..peer::config::PeerFile::default()
        };
        peer::config::save(&peers_path, &file).expect("seed the peers file");
        peers_path
    }

    fn read_back(peers_path: &Path) -> peer::config::PeerRow {
        peer::config::read_or_default(peers_path)
            .expect("the peers file reads back")
            .peers
            .into_iter()
            .next()
            .expect("the seeded row is still there")
    }

    /// **`tcr peer share on` persists `inspect` and a lend grant**, and
    /// `share off` takes both away.
    ///
    /// Persistence is the whole claim: the verb printed a confident line and
    /// the listener reads the FILE, so a write that did not land is a switch
    /// that says "on" and serves nobody. Asserted by re-reading through
    /// `peer::config::read_or_default` (the same reader the listener uses)
    /// rather than by trusting the in-memory value the verb just set.
    ///
    /// Watch it fail by deleting the `peer::config::save` call in the `Share`
    /// arm's `On` branch: the row comes back with `inspect` false and no lend
    /// grant. Measured that way before this test was kept.
    #[tokio::test]
    async fn peer_share_persists_inspect_and_a_lend_grant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Share(peer_cli::PeerSwitchArgs {
                peers: Some(peers_path.clone()),
                state: peer_cli::Switch::On,
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.10,
                ttl: 600,
                max_inflight: 2,
                scope: "all".to_string(),
            }),
        })
        .await
        .expect("peer share on");

        let row = read_back(&peers_path);
        assert!(row.allow.inspect, "share on grants `inspect`");
        assert!(
            !row.allow.allow_disclose,
            "share on is one direction only: it does NOT grant `disclose`, which is what \
             would let the other Mac read THIS Mac's requests"
        );
        assert_eq!(row.lend.len(), 1, "one grant, for the window asked for");
        assert_eq!(row.lend[0].window, tcr_peer_wire::Window::SevenDay);
        assert!((row.lend[0].fraction - 0.10).abs() < f64::EPSILON);
        assert_eq!(row.lend[0].ttl_s, 600);
        assert_eq!(row.lend[0].max_inflight, 2);

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Share(peer_cli::PeerSwitchArgs {
                peers: Some(peers_path.clone()),
                state: peer_cli::Switch::Off,
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.10,
                ttl: 600,
                max_inflight: 2,
                scope: "all".to_string(),
            }),
        })
        .await
        .expect("peer share off");

        let row = read_back(&peers_path);
        assert!(!row.allow.inspect, "share off revokes `inspect`");
        assert!(
            row.lend.is_empty(),
            "share off clears the grants too, or a re-enabled `inspect` would resurrect a \
             ceiling the operator never re-typed"
        );
    }

    /// **`tcr peer share on` refuses to keep an end that has already gone
    /// by.**
    ///
    /// This verb keeps the end, the mode and the schedule an operator set per
    /// peer rather than overwriting them with defaults, which is right. What it
    /// did with an `until` that had already passed was carry it onto the new
    /// grant and print `on`: every request against that grant is then refused
    /// for want of a live lending, and the only surface that says so is
    /// `peer lend --list`'s `ended=true`. `peer lend` cannot write one of those
    /// at all, so this verb is the one that has to refuse.
    ///
    /// The file is re-read afterwards, because "refused" and "refused without
    /// writing" are different claims and only the second one is safe: a
    /// half-applied share leaves `inspect` granted with no grant behind it.
    ///
    /// Watch it fail by deleting the `closed` block from the `Share` arm's `On`
    /// branch: the command answers `Ok`, the grant comes back with yesterday's
    /// `until` and a fraction of 0.10, and the operator is told sharing is on.
    #[tokio::test]
    async fn peer_share_on_refuses_a_lending_window_that_has_already_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        let now_s = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())
            .expect("a unix timestamp after 1970");

        // A grant whose lending ended an hour ago, which is what a `--until
        // 18:00` looks like the next morning.
        {
            let mut file = peer::config::read_or_default(&peers_path).expect("the seeded file");
            let mut ended =
                peer::config::LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.05, 300, 2);
            ended.until = Some(now_s - 3_600);
            file.peers[0].lend = vec![ended];
            peer::config::save(&peers_path, &file).expect("the ended grant is seeded");
        }

        let share_on = |path: PathBuf| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Share(peer_cli::PeerSwitchArgs {
                peers: Some(path),
                state: peer_cli::Switch::On,
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.10,
                ttl: 600,
                max_inflight: 2,
                scope: "all".to_string(),
            }),
        };
        let refusal = run_peer(share_on(peers_path.clone()))
            .await
            .expect_err("an end that has passed is refused, not printed as success")
            .to_string();
        assert!(
            refusal.contains("already"),
            "the refusal says the lending has ended: {refusal}"
        );
        assert!(
            refusal.contains("--relend") || refusal.contains("--for"),
            "and it says what to type instead, or an operator reads it as a bug: {refusal}"
        );

        let row = read_back(&peers_path);
        assert_eq!(
            row.lend.len(),
            1,
            "a refusal writes nothing, so the one ended grant is still the only one"
        );
        assert!(
            (row.lend[0].fraction - 0.05).abs() < f64::EPSILON,
            "and it is untouched: the refused fraction of 0.10 is not on disk"
        );
        assert!(
            !row.allow.inspect,
            "nor is the `inspect` this verb would have granted, which would be a disclosure \
             bought by a command that failed"
        );

        // The same verb on a grant whose end is ahead still works, which is the
        // control: without it this test passes on a verb that refuses always.
        {
            let mut file = peer::config::read_or_default(&peers_path).expect("the seeded file");
            file.peers[0].lend[0].until = Some(now_s + 3_600);
            peer::config::save(&peers_path, &file).expect("the live grant is seeded");
        }
        run_peer(share_on(peers_path.clone()))
            .await
            .expect("an end still ahead is shared on as before");
        let row = read_back(&peers_path);
        assert!(
            (row.lend[0].fraction - 0.10).abs() < f64::EPSILON,
            "the new fraction lands"
        );
        assert_eq!(
            row.lend[0].until,
            Some(now_s + 3_600),
            "and the operator's own end is still kept"
        );
    }

    /// **`tcr peer allow <peer> disclose on` persists that one grant and no
    /// other.**
    ///
    /// `disclose` is the one the borrowing seam reads
    /// (`fallback::peer_lease_provider`), and it is the opposite direction from
    /// `inspect`, so a verb that set both, or the wrong one, would turn on a
    /// disclosure the operator never asked for. Both halves are asserted.
    ///
    /// Watch it fail by deleting the `peer::config::save` call in the `Allow`
    /// arm: `disclose` comes back false.
    #[tokio::test]
    async fn peer_allow_persists_one_grant_and_leaves_the_others_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        // The WIRE form, not `display()`: the display form is deliberately
        // one-way and `PeerId::parse` refuses it (`crates/tcr-peer-wire`'s type
        // docs). `tcr peer ls --json` emits this form; the text output emits
        // the display form, which neither of these verbs can take: a surface
        // gap, not papered over here.
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Allow(peer_cli::PeerAllowArgs {
                peers: Some(peers_path.clone()),
                peer: peer_id.clone(),
                grant: peer_cli::PeerGrant::Disclose,
                state: peer_cli::Switch::On,
            }),
        })
        .await
        .expect("peer allow disclose on");

        let row = read_back(&peers_path);
        assert!(row.allow.allow_disclose, "the named grant is on");
        assert!(
            !row.allow.inspect && !row.allow.gateway && !row.allow.relay && !row.allow.accept_move,
            "one verb, one grant: {:?}",
            row.allow
        );

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Allow(peer_cli::PeerAllowArgs {
                peers: Some(peers_path.clone()),
                peer: peer_id,
                grant: peer_cli::PeerGrant::Disclose,
                state: peer_cli::Switch::Off,
            }),
        })
        .await
        .expect("peer allow disclose off");
        assert!(
            !read_back(&peers_path).allow.allow_disclose,
            "off is persisted too, a revoke that does not land is the dangerous direction"
        );

        // An UNPINNED peer is refused rather than written: a grant for a peer
        // nothing enforces is a row that reads like access.
        let stranger = tcr_peer_wire::PeerId([9_u8; 32]).to_wire();
        let refused = run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Allow(peer_cli::PeerAllowArgs {
                peers: Some(peers_path.clone()),
                peer: stranger,
                grant: peer_cli::PeerGrant::Disclose,
                state: peer_cli::Switch::On,
            }),
        })
        .await;
        assert!(refused.is_err(), "an unpinned peer gets no grant");
        assert_eq!(
            read_back(&peers_path).node,
            tcr_peer_wire::PeerId(PINNED),
            "and the refusal wrote nothing"
        );
    }

    /// A main config with one account, pinned or not, for `--mode hand`'s
    /// exit-lock check to read.
    ///
    /// Written as JSON text rather than through `Config`, for the reason the
    /// peers fixtures give: a field added to `Config` later must not turn this
    /// into a compile error.
    fn config_with_account(dir: &Path, egress: &str) -> PathBuf {
        let path = dir.join("teamclaude.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                  "proxy": {{ "port": 0 }},
                  "upstream": "https://api.anthropic.com",
                  "accounts": [
                    {{ "name": "alice@example.com", "accessToken": "at-alice"{egress} }}
                  ]
                }}"#
            ),
        )
        .expect("write the main config");
        path
    }

    /// **`tcr peer lend --mode hand` mints a hand grant, and a replace without
    /// `--mode` keeps it.**
    ///
    /// The CLI could not mint a hand grant at all: `LendMode` existed on the
    /// grant and on the wire, and no argv reached it, so the whole mode was
    /// unreachable from the surface an operator uses. Worse,
    /// the replace-by-(window, scope) path rewrote an existing hand grant as
    /// serve, so editing a hand grant's fraction silently took a disclosure
    /// decision back.
    ///
    /// The second half is the one with teeth: an omitted `--mode` has to mean
    /// "keep what is there", not "default to serve".
    ///
    /// Watched red by dropping the `.or(replacing)` from the mode resolution:
    /// the fraction edit turns the grant back into serve.
    #[tokio::test]
    async fn peer_lend_mints_a_hand_grant_and_a_replace_keeps_the_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        let config_path = config_with_account(dir.path(), "");
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();

        let lend = |fraction: f64, mode: Option<peer_cli::PeerLendMode>| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Lend(peer_cli::PeerLendArgs {
                peers: Some(peers_path.clone()),
                peer: peer_id.clone(),
                window: peer_cli::PeerWindow::SevenDay,
                fraction,
                ttl: 300,
                max_inflight: 3,
                scope: "all".to_string(),
                mode,
                config: Some(config_path.clone()),
                for_: None,
                until: None,
                between: None,
                days: None,
                list: false,
                revoke: None,
                relend: None,
            }),
        };

        run_peer(lend(0.20, Some(peer_cli::PeerLendMode::Hand)))
            .await
            .expect("peer lend --mode hand");
        let row = read_back(&peers_path);
        assert_eq!(
            row.lend[0].mode,
            peer::config::LendMode::Hand,
            "the operator asked for hand mode and the file has to say so: {:?}",
            row.lend
        );

        // The same grant, a different fraction, no `--mode`.
        run_peer(lend(0.30, None))
            .await
            .expect("peer lend, fraction only");
        let row = read_back(&peers_path);
        assert_eq!(
            row.lend.len(),
            1,
            "replaced, never appended: {:?}",
            row.lend
        );
        assert_eq!(
            row.lend[0].mode,
            peer::config::LendMode::Hand,
            "an omitted --mode keeps the mode that was there, or editing a fraction takes \
             a disclosure decision back without saying so: {:?}",
            row.lend
        );

        // And a NEW grant, on a window nothing covers yet, is serve.
        let mut fresh = lend(0.10, None);
        if let peer_cli::PeerAction::Lend(args) = &mut fresh.action {
            args.window = peer_cli::PeerWindow::FiveHour;
        }
        run_peer(fresh).await.expect("peer lend 5h");
        let row = read_back(&peers_path);
        let five_hour = row
            .lend
            .iter()
            .find(|grant| grant.window == tcr_peer_wire::Window::FiveHour)
            .expect("the 5h grant is there");
        assert_eq!(
            five_hour.mode,
            peer::config::LendMode::Serve,
            "a grant with nothing to inherit from is serve, which is what every grant \
             written before modes existed already meant"
        );
    }

    /// **`--mode hand` is refused when every account in the scope is strictly
    /// pinned away from this Mac.**
    ///
    /// A hand grant hands the borrower a bearer to spend from its OWN IP, and a
    /// strict pin says this account's requests leave through a named peer or
    /// not at all. The grant would look granted and every borrowed request
    /// would be refused by the exit lock, which is the worst of both: a
    /// capability advertised and not there.
    ///
    /// The non-strict leg is the control, and it is the assertion that makes
    /// this about STRICTNESS rather than about pinning: a pin without
    /// `egressStrict` falls back to local, which is exactly what a hand-mode
    /// borrower does, so it is allowed.
    ///
    /// Watched red by deleting the refusal block: the strict case writes a hand
    /// grant and the first assertion reads `Ok`.
    #[tokio::test]
    async fn peer_lend_mode_hand_is_refused_when_the_scope_is_pinned_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();
        let carrier = tcr_peer_wire::PeerId([0x44; 32]).to_wire();

        let lend_against = |config_path: PathBuf| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Lend(peer_cli::PeerLendArgs {
                peers: Some(peers_path.clone()),
                peer: peer_id.clone(),
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.20,
                ttl: 300,
                max_inflight: 3,
                scope: "all".to_string(),
                mode: Some(peer_cli::PeerLendMode::Hand),
                config: Some(config_path),
                for_: None,
                until: None,
                between: None,
                days: None,
                list: false,
                revoke: None,
                relend: None,
            }),
        };

        let strict = config_with_account(
            dir.path(),
            &format!(r#", "egress": "via {carrier}", "egressStrict": true"#),
        );
        let refusal = run_peer(lend_against(strict))
            .await
            .expect_err("a hand grant on a strictly pinned scope buys nothing");
        let said = refusal.to_string();
        assert!(
            said.contains("egressStrict") && said.contains("--mode serve"),
            "the refusal has to name the lock AND what to do instead, or an operator reads \
             it as a bug in the CLI: {said}"
        );
        assert!(
            read_back(&peers_path).lend.is_empty(),
            "and it wrote nothing: a refusal that half-applied would leave a grant the \
             operator was told they did not get"
        );

        // The control: pinned but NOT strict falls back to local, which is what
        // a hand-mode borrower does anyway, so it is allowed.
        let soft = dir.path().join("soft");
        std::fs::create_dir_all(&soft).expect("a second scratch directory");
        let soft_config = config_with_account(&soft, &format!(r#", "egress": "via {carrier}""#));
        run_peer(lend_against(soft_config))
            .await
            .expect("a pin without egressStrict is not a refusal");
        assert_eq!(
            read_back(&peers_path).lend[0].mode,
            peer::config::LendMode::Hand,
            "and it wrote the hand grant"
        );
    }

    /// **`tcr peer lend` persists one grant per window, replaces rather than
    /// duplicates, clamps the fraction, and removes at zero.**
    ///
    /// Four claims in one test because they are one write: a second grant for
    /// the same window that appended instead of replacing would leave
    /// `Ledger::grant`'s `find` reading whichever row came first, which is not
    /// the one the operator just typed.
    ///
    /// Watch it fail by deleting the `row.lend.retain(...)` line in the `Lend`
    /// arm: the second call leaves two `7d` grants and the length assertion
    /// goes red.
    #[tokio::test]
    async fn peer_lend_persists_one_grant_per_window_and_removes_at_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        // The WIRE form, not `display()`: the display form is deliberately
        // one-way and `PeerId::parse` refuses it (`crates/tcr-peer-wire`'s type
        // docs). `tcr peer ls --json` emits this form; the text output emits
        // the display form, which neither of these verbs can take: a surface
        // gap, not papered over here.
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();

        let lend = |fraction: f64, window: peer_cli::PeerWindow| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Lend(peer_cli::PeerLendArgs {
                peers: Some(peers_path.clone()),
                peer: peer_id.clone(),
                window,
                fraction,
                ttl: 300,
                max_inflight: 3,
                scope: "all".to_string(),
                mode: None,
                config: None,
                for_: None,
                until: None,
                between: None,
                days: None,
                list: false,
                revoke: None,
                relend: None,
            }),
        };

        run_peer(lend(0.20, peer_cli::PeerWindow::SevenDay))
            .await
            .expect("peer lend 7d 0.20");
        let row = read_back(&peers_path);
        assert_eq!(row.lend.len(), 1);
        assert!((row.lend[0].fraction - 0.20).abs() < f64::EPSILON);
        assert_eq!(row.lend[0].ttl_s, 300);
        assert_eq!(row.lend[0].max_inflight, 3);

        // A second grant for the SAME window replaces it, and the fraction is
        // clamped to the ceiling with the clamp printed (see `peer_lend_grant`).
        run_peer(lend(0.90, peer_cli::PeerWindow::SevenDay))
            .await
            .expect("peer lend 7d 0.90");
        let row = read_back(&peers_path);
        assert_eq!(
            row.lend.len(),
            1,
            "replaced, never appended: {:?}",
            row.lend
        );
        assert!(
            (row.lend[0].fraction - MAX_LEND_FRACTION).abs() < f64::EPSILON,
            "0.90 is clamped to the {MAX_LEND_FRACTION} ceiling, got {}",
            row.lend[0].fraction
        );

        // A DIFFERENT window is a second row, not a replacement.
        run_peer(lend(0.10, peer_cli::PeerWindow::FiveHour))
            .await
            .expect("peer lend 5h 0.10");
        assert_eq!(read_back(&peers_path).lend.len(), 2);

        // Zero is a removal, and only of the window named.
        run_peer(lend(0.0, peer_cli::PeerWindow::SevenDay))
            .await
            .expect("peer lend 7d 0");
        let row = read_back(&peers_path);
        assert_eq!(row.lend.len(), 1, "the 7d grant is gone: {:?}", row.lend);
        assert_eq!(
            row.lend[0].window,
            tcr_peer_wire::Window::FiveHour,
            "and the 5h one is untouched"
        );
    }
}

/// `tcr group ls|add|rm|reserve|unreserve|allow-control|disallow-control|color` — manage account group membership. Argument shape is
/// the TcrBar panel contract — see [`GroupAction`]'s doc-comment.
fn run_group(args: GroupArgs) -> anyhow::Result<()> {
    match args.action {
        GroupAction::Ls(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::list_groups(&config_path, a.json)
        }
        GroupAction::Add(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::add_to_group(&config_path, &a.group, &a.account)
        }
        GroupAction::Rm(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::remove_from_group(&config_path, &a.group, a.account.as_deref(), a.all)
        }
        GroupAction::Reserve(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::reserve_group(&config_path, &a.group)
        }
        GroupAction::Unreserve(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::unreserve_group(&config_path, &a.group)
        }
        GroupAction::Park(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::park_group(&config_path, &a.group)
        }
        GroupAction::Unpark(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::unpark_group(&config_path, &a.group)
        }
        GroupAction::AllowControl(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::allow_control_group(&config_path, &a.group)
        }
        GroupAction::DisallowControl(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            cli::disallow_control_group(&config_path, &a.group)
        }
        GroupAction::Color(a) => {
            let config_path = a.config.unwrap_or_else(config::default_path);
            // clap's `conflicts_with`/`required_unless_present` on
            // `GroupColorArgs` guarantee exactly one of `hex`/`--clear`.
            cli::set_group_color(&config_path, &a.group, a.hex.as_deref())
        }
    }
}

/// `tcr status [--json]` — probe every account's live quota and print it.
async fn run_status(args: StatusArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::status(&config_path, args.json).await
}

/// `tcr doctor [--json]`: whether Claude Code is routed to this proxy.
///
/// The exit code is the point of the verb (a script asks `tcr doctor >/dev/null
/// || …`), so it is carried out of [`cli::doctor`] and spent here rather than
/// flattened into the `anyhow::Result` every other verb returns: `Ok(())` is
/// exit 0 and there is no way to say 2 or 3 through it. `std::process::exit`
/// after the printing is done, the same shape `run_server`'s stand-down path
/// uses.
async fn run_doctor(args: DoctorArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    let code = cli::doctor(&config_path, args.json).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `tcr sessions [--json]` — the running proxy's live sessions.
async fn run_sessions(args: SessionsArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::sessions(&config_path, args.json).await
}

/// `tcr wrap [--days N] [--json]` — a usage report read from the ledger.
fn run_wrap(args: WrapArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::wrap(&config_path, args.days, args.json)
}

/// `tcr ui` — open TcrBar, the macOS menu-bar app.
///
/// This exists for discoverability, not capability: `open -a TcrBar` already
/// works. Without it nothing in `tcr --help` reveals that a UI exists at all, so
/// the app is only findable by knowing it is there.
///
/// It deliberately does NOT build the app or know where the checkout is. It asks
/// LaunchServices to open a bundle id, which resolves wherever the app was
/// installed. A `tcr` that shells into a source tree would break the moment the
/// checkout moved.
/// The shared mount/swap logic, embedded once so `install.sh` and this binary
/// never carry two copies that drift. See `scripts/install-tcrbar-from-dmg.sh`
/// for what it does and why.
#[cfg(target_os = "macos")]
const INSTALL_TCRBAR_FROM_DMG_SH: &str = include_str!("../scripts/install-tcrbar-from-dmg.sh");

/// Is a process literally named `TcrBar` running right now?
///
/// Matched by exact name (`pgrep -x`), not a path pattern like
/// `apps/macos/scripts/install.sh` uses — this runs on a machine that may
/// have TcrBar installed from a dmg, never built from source, so there is no
/// destination path to derive a pattern from. `pgrep -x` still cannot
/// distinguish it from an unrelated program that happens to share the name;
/// that gap already exists in `apps/macos/scripts/uninstall.sh`.
#[cfg(target_os = "macos")]
fn tcrbar_is_running() -> anyhow::Result<bool> {
    use anyhow::Context;
    let status = std::process::Command::new("pgrep")
        .args(["-x", "TcrBar"])
        .status()
        .context("failed to run `pgrep`")?;
    Ok(status.success())
}

/// `tcr ui` — open TcrBar, installing it first if it is missing (macOS only).
#[cfg(target_os = "macos")]
fn run_ui() -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write as _};

    use anyhow::Context;

    let status = std::process::Command::new("open")
        .args(["-b", "io.github.dhkts1.tcrbar"])
        .status()
        .context("failed to run `open`")?;

    if status.success() {
        return Ok(());
    }

    // `open -b` fails when the bundle id is not registered, which almost
    // always means "not installed" rather than "broken".
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "TcrBar is not installed. Run `tcr ui` in a terminal to install it, or \
             download the dmg from https://github.com/dhkts1/teamclaude-rs/releases/latest"
        );
    }

    if tcrbar_is_running()? {
        anyhow::bail!(
            "a process named TcrBar is already running but is not registered with \
             LaunchServices under io.github.dhkts1.tcrbar — quit it before `tcr ui` \
             installs a fresh copy, then run `tcr ui` again. Installing over a running \
             copy is refused: its own bundled `tcr` may be an executing image inside \
             the very bundle being replaced."
        );
    }

    eprint!("TcrBar is not installed. Download and install it to /Applications? [y/N] ");
    std::io::stderr()
        .flush()
        .context("failed to flush the prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed to read the answer")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        anyhow::bail!("not installing — re-run `tcr ui` when you're ready.");
    }

    let tag = update::fetch_latest_release_tag()
        .context("could not resolve the latest TcrBar release")?;
    println!("tcr: downloading TcrBar {tag}…");

    let tmp_dir = tempfile::Builder::new()
        .prefix("tcr-ui-install-")
        .tempdir()
        .context("could not create a temp directory for the download")?;
    let dmg_path = tmp_dir.path().join("TcrBar.dmg");
    update::download_tcrbar_dmg(&tag, &dmg_path)
        .with_context(|| format!("could not download the TcrBar {tag} dmg"))?;

    let script_path = tmp_dir.path().join("install-tcrbar-from-dmg.sh");
    std::fs::write(&script_path, INSTALL_TCRBAR_FROM_DMG_SH)
        .context("could not write the install script to a temp file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .context("could not make the install script executable")?;
    }

    let install_status = std::process::Command::new("bash")
        .arg(&script_path)
        .arg(&dmg_path)
        .status()
        .context("failed to run the TcrBar install script")?;
    if !install_status.success() {
        anyhow::bail!("installing TcrBar {tag} failed with {install_status}");
    }

    let status = std::process::Command::new("open")
        .args(["-b", "io.github.dhkts1.tcrbar"])
        .status()
        .context("failed to run `open`")?;
    if !status.success() {
        anyhow::bail!("TcrBar {tag} was installed but `open -b` still failed with {status}");
    }
    Ok(())
}

/// Non-macOS builds keep the subcommand so `--help` is identical everywhere, and
/// fail with the reason rather than a missing-subcommand error.
#[cfg(not(target_os = "macos"))]
fn run_ui() -> anyhow::Result<()> {
    anyhow::bail!("`tcr ui` opens the macOS menu-bar app, and this is not macOS.")
}

/// `TCR_CLAUDE_BIN` overrides which `claude` binary we launch, taking
/// priority over the legacy `CLAUDE_BIN` (kept for compatibility with
/// whatever already sets it outside tcr); neither set falls back to `claude`
/// on `PATH`. An empty value in either counts as unset — `TCR_CLAUDE_BIN=`
/// left behind by a shell must not silently win over a real `CLAUDE_BIN`.
const TCR_CLAUDE_BIN_ENV: &str = "TCR_CLAUDE_BIN";
/// See [`TCR_CLAUDE_BIN_ENV`].
const CLAUDE_BIN_ENV: &str = "CLAUDE_BIN";

/// Resolves which `claude` binary to launch, through the ordered pair of env
/// keys documented at [`TCR_CLAUDE_BIN_ENV`]. Named generally — "harness",
/// not "claude" — because a second external harness binary will need this
/// exact resolution order later; that second binary is not built here, but
/// the helper already takes its own key names and default so adding it is a
/// new pair of constants and a one-line call, not a rewrite of this function.
fn resolve_harness_bin(specific_key: &str, legacy_key: &str, default: &str) -> String {
    resolve_harness_bin_from(
        std::env::var(specific_key).ok(),
        std::env::var(legacy_key).ok(),
        default,
    )
}

/// [`resolve_harness_bin`]'s pure half, split out so the precedence order and
/// the empty-string-counts-as-unset rule are unit-testable without mutating
/// process-global env vars (which is unsound to do from parallel tests).
fn resolve_harness_bin_from(
    specific: Option<String>,
    legacy: Option<String>,
    default: &str,
) -> String {
    for value in [specific, legacy].into_iter().flatten() {
        if !value.is_empty() {
            return value;
        }
    }
    default.to_string()
}

/// The `claude` binary this process should launch or probe, per
/// [`resolve_harness_bin`].
fn claude_bin() -> String {
    resolve_harness_bin(TCR_CLAUDE_BIN_ENV, CLAUDE_BIN_ENV, "claude")
}

/// A fresh [`std::process::Command`] for [`claude_bin`] — the one place that
/// spawns or would spawn the user's harness, so every call site resolves the
/// same way.
fn claude_command() -> std::process::Command {
    std::process::Command::new(claude_bin())
}

/// `tcr run [-- args…]` — launch Claude Code already pointed at this proxy.
///
/// Mirrors the JS `teamclaude run` passthrough contract: if the proxy is not
/// listening we launch `claude` untouched, so a stopped proxy never breaks the
/// shell alias.
fn run_claude(args: RunArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let (config, _) = load_config(&config_path)?;
    let port = config.proxy.port;

    // `--group`: validate the name and the claude version BEFORE spawning
    // anything, in this order — a. name, b. version — so a typo or too-old
    // `claude` never launches a session silently ungrouped.
    if let Some(group) = args.group.as_deref() {
        validate_group(&config, group)?;
        match check_claude_version() {
            ClaudeVersionCheck::TooOld(found) => {
                anyhow::bail!(
                    "claude {found} is older than {MIN_CLAUDE_VERSION_FOR_GROUP_STR}, the minimum \
                     that forwards ANTHROPIC_CUSTOM_HEADERS as a real request header — `--group` \
                     cannot work on this install. Upgrade claude and retry."
                );
            }
            // Phase 1 is PREFER-only, so degrading to ordinary (ungrouped) routing on an
            // unreadable/missing `claude` is safe — warn and continue rather than refuse.
            // Phase 2's `--only` must refuse here instead: there the operator believes
            // they are CONTAINED to the group, and silently routing everywhere is the
            // wrong kind of wrong for that contract.
            ClaudeVersionCheck::Unknown => {
                eprintln!(
                    "[tcr] --group {group}: could not determine the installed claude version \
                     (need >= {MIN_CLAUDE_VERSION_FOR_GROUP_STR} for ANTHROPIC_CUSTOM_HEADERS) — \
                     proceeding, but if it is too old this session will silently route to the \
                     whole pool instead of the group"
                );
            }
            ClaudeVersionCheck::Ok => {}
        }
    }

    let mut cmd = claude_command();
    cmd.args(&args.args);
    mark_run_active(&mut cmd);

    // c/d. Compose and set the group header — merging with, never clobbering,
    // whatever `ANTHROPIC_CUSTOM_HEADERS` this process already inherited (a
    // user's own headers, or an outer `tcr run`'s — see [`RUN_ACTIVE_ENV`]).
    // Set unconditionally of see-through vs base-URL mode below: both apply
    // env to the same `cmd`, and this line runs before either.
    if let Some(group) = args.group.as_deref() {
        let inherited = std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok();
        let composed = compose_group_header(inherited.as_deref(), group);
        cmd.env("ANTHROPIC_CUSTOM_HEADERS", &composed);
        eprintln!("[tcr] --group {group}: routing this session via {GROUP_HEADER_NAME}");
    }

    if cli::proxy_is_up(port) {
        if let Some(notice) = withheld_api_key_notice(
            config.proxy.api_key.is_some(),
            std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        ) {
            eprintln!("{notice}");
        }
        // Two ways to route claude at ourselves, and they are NOT equivalent to
        // Claude Code — see `apply_see_through_env` for why we prefer the first.
        // Anything missing from the MITM material lands us in base-URL mode, which
        // always works; there is no half-applied third state.
        match see_through_ca() {
            Some(ca) => apply_see_through_env(&mut cmd, port, &ca),
            None => apply_base_url_env(&mut cmd, port),
        }
    } else {
        eprintln!("[tcr] proxy not listening on :{port} — launching claude directly");
    }

    let status = cmd
        .status()
        .context("failed to launch `claude` — is it on PATH?")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// The oldest Claude Code that forwards `ANTHROPIC_CUSTOM_HEADERS` as a real
/// outbound header on every `/v1/messages` request — verified against
/// Anthropic's gateway-protocol documentation. Older than this, `--group`
/// would set an env var claude silently never sends.
const MIN_CLAUDE_VERSION_FOR_GROUP: (u64, u64, u64) = (2, 1, 227);
const MIN_CLAUDE_VERSION_FOR_GROUP_STR: &str = "2.1.227";

// `validate_group_label_chars` moved to `teamclaude_rs::cli` (imported below) so
// `tcr group add`'s surgical write can reuse the same Phase 1 validator instead
// of a second one drifting out of sync with this one.

/// `--group`'s validation half (step a): the requested name must be a label
/// SOME configured account actually carries, and every label involved —
/// the requested one AND every configured one, since either could end up in
/// `compose_group_header`'s output — must pass [`validate_group_label_chars`].
/// A typo must never resolve to an empty set — with Phase 1's prefer-only
/// semantics that would silently route every session across the whole pool
/// with no error at all, which is the quiet-wrong-answer this refusal exists
/// to prevent.
fn validate_group(config: &Config, group: &str) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        anyhow::bail!("--group {group:?}: invalid group label — {reason}");
    }

    let mut configured: Vec<&str> = config
        .accounts
        .iter()
        .filter_map(|a| a.groups.as_ref())
        .flatten()
        .map(String::as_str)
        .collect();
    for label in &configured {
        if let Err(reason) = validate_group_label_chars(label) {
            anyhow::bail!("config carries an invalid group label {label:?}: {reason}");
        }
    }
    configured.sort_unstable();
    configured.dedup();

    if configured.contains(&group) {
        return Ok(());
    }
    if configured.is_empty() {
        anyhow::bail!(
            "--group {group}: no account in the config carries a `groups` label — nothing to route to"
        );
    }
    anyhow::bail!(
        "--group {group}: not a configured group. Configured groups: {}",
        configured.join(", ")
    );
}

/// Three-state result of checking the installed `claude` against
/// [`MIN_CLAUDE_VERSION_FOR_GROUP`] — kept distinct from a plain bool even
/// though Phase 1 collapses `TooOld` and `Unknown` to different outcomes,
/// because Phase 2's `--only` must refuse on BOTH: there, degrading silently
/// on an unreadable version would tell the operator they were contained to
/// the group while the session actually ran across the whole fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeVersionCheck {
    Ok,
    /// The parsed version, for the error message.
    TooOld(String),
    /// `claude --version` failed to run, exited non-zero, or its output did
    /// not parse as `MAJOR.MINOR.PATCH …`.
    Unknown,
}

fn check_claude_version() -> ClaudeVersionCheck {
    let output = match claude_command().arg("--version").output() {
        Ok(o) if o.status.success() => o,
        _ => return ClaudeVersionCheck::Unknown,
    };
    classify_claude_version_output(&String::from_utf8_lossy(&output.stdout))
}

/// [`check_claude_version`]'s classification half, split out so it is testable
/// without spawning a `claude` process: feed it `claude --version`'s stdout
/// directly.
fn classify_claude_version_output(stdout: &str) -> ClaudeVersionCheck {
    match parse_claude_version(stdout) {
        Some(found) if found >= MIN_CLAUDE_VERSION_FOR_GROUP => ClaudeVersionCheck::Ok,
        Some((maj, min, patch)) => ClaudeVersionCheck::TooOld(format!("{maj}.{min}.{patch}")),
        None => ClaudeVersionCheck::Unknown,
    }
}

/// Parse the leading `MAJOR.MINOR.PATCH` off `claude --version`'s output
/// (observed shape: `"2.1.237 (Claude Code)"`, first token before whitespace).
/// `None` on anything else — garbage, a `v`-prefixed or two-component
/// version, empty output — which is exactly what routes [`check_claude_version`]
/// to `Unknown` rather than a wrong guess.
fn parse_claude_version(output: &str) -> Option<(u64, u64, u64)> {
    let first_token = output.split_whitespace().next()?;
    let mut parts = first_token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// `--group`'s header-composition half (steps c/d): the new value for
/// `ANTHROPIC_CUSTOM_HEADERS`, merging with whatever this process already
/// inherited rather than clobbering it — a user may have set their own
/// headers, and silently dropping them is a bug.
///
///  - No inherited value (or an empty one) → just `x-tcr-group: <name>`.
///  - Inherited value present → append ours on a new line.
///  - Inherited value already carries an `x-tcr-group` line (matched
///    case-insensitively on the header NAME) → that line is replaced, not
///    duplicated. Not hypothetical: `tcr run` nests (see `mark_run_active` /
///    [`RUN_ACTIVE_ENV`]), and two conflicting group headers on one request
///    is a worse failure than either value alone.
///  - Every OTHER inherited line's text and order is preserved exactly.
///
/// Pure — takes the inherited value and the group name, returns the new
/// value — so all of the above is testable without spawning a process.
fn compose_group_header(inherited: Option<&str>, group: &str) -> String {
    let ours = format!("{GROUP_HEADER_NAME}: {group}");
    let Some(inherited) = inherited.filter(|s| !s.is_empty()) else {
        return ours;
    };
    let mut lines: Vec<&str> = inherited
        .lines()
        .filter(|line| {
            let name = line.split(':').next().unwrap_or(line).trim();
            !name.eq_ignore_ascii_case(GROUP_HEADER_NAME)
        })
        .collect();
    lines.push(&ours);
    lines.join("\n")
}

/// The marker `tcr run` leaves on its child: **a `tcr run` is already above you
/// in this process chain, so do not start another one.**
///
/// `tcr run` resolves `claude` from `PATH`, and on a machine where something else
/// also wraps `claude` that lookup can land back on a launcher that wraps in
/// `tcr run` — which then resolves `claude` from `PATH` again. The chain still
/// terminates and the routing environment applied twice is identical, so nothing
/// breaks; what you see is every startup line printed twice and a second `tcr`
/// process parked in the tree for the life of the session. Measured 2026-08-17
/// inside a cmux pane: a hand-typed `tcr run` produced two see-through banners,
/// and dropping cmux's shim directory from `PATH` produced one.
///
/// A launcher cannot infer this from the routing variables — those are also what
/// a `tcr`-launched shell exports to everything it runs — so we state it, and a
/// wrapper that understands the marker hands off instead of wrapping again.
///
/// The name is deliberately **not** `CMUX_`-prefixed. cmux's own claude wrapper
/// clears every variable matching that prefix before exec'ing the real binary, so
/// a marker named for the wrapper it has to survive would be erased in transit by
/// exactly the process it exists to inform. [`marker_survives_the_cmux_prefix_sweep`]
/// pins that.
const RUN_ACTIVE_ENV: &str = "TCR_RUN_ACTIVE";

/// Set on **every** `tcr run` child, including the proxy-down passthrough: the
/// claim is about this chain already containing a `tcr run`, which is true there
/// too, and a launcher re-wrapping that case just prints the passthrough notice
/// twice instead of the routing banner.
fn mark_run_active(cmd: &mut std::process::Command) {
    cmd.env(RUN_ACTIVE_ENV, "1");
}

/// The CA to advertise for see-through mode, or `None` when we must fall back to
/// base-URL mode. Prints the reason on every `None` — a silent downgrade would
/// look exactly like a working see-through session while the capabilities it
/// exists to preserve stay off.
///
/// See-through needs BOTH halves of the MITM contract: the proxy must be able to
/// present a leaf for `api.anthropic.com` (so `mitm::load_tls` has to succeed)
/// AND we must be able to name the CA that signed it (so `claude` can be told to
/// trust it). `load_tls` is the same loader the server ran at boot against the
/// same dir, so it resolves to the same material rather than a second opinion.
fn see_through_ca() -> Option<PathBuf> {
    match mitm::load_tls() {
        Ok(assets) => match assets.ca_path {
            Some(ca) if ca.is_file() => Some(ca),
            // A path we cannot read is not a CA we can hand to claude.
            Some(ca) => {
                eprintln!(
                    "[tcr] see-through off: CA {} is not a readable file",
                    ca.display()
                );
                None
            }
            None => {
                eprintln!("[tcr] see-through off: no CA on disk for the MITM leaf we present");
                None
            }
        },
        Err(err) => {
            eprintln!("[tcr] see-through off: MITM TLS material unavailable ({err})");
            None
        }
    }
}

/// Why `tcr run` does NOT hand the configured proxy key to `claude` as
/// `ANTHROPIC_API_KEY`, and says so.
///
/// It used to. Setting that variable makes Claude Code treat an API key as its auth
/// source AHEAD of the claude.ai login, and that **disables every claude.ai
/// connector** — announced once, in one startup line that scrolls away, after which
/// the tools are simply absent. It bought nothing in exchange: the `x-api-key` gate
/// exempts loopback clients (see `proxy::handle`), and the server binds 127.0.0.1
/// only, so a `tcr run` child is always exempt. Measured with the variable absent:
/// `/v1/messages` served and rotated across accounts, and every connector loaded.
///
/// A value the CALLER exported is inherited untouched — an explicit choice wins, and
/// it is the escape hatch for a `claude` with no claude.ai login of its own, which
/// does need some credential to start.
fn withheld_api_key_notice(configured: bool, caller_set: bool) -> Option<&'static str> {
    (configured && !caller_set).then_some(
        "[tcr] not exporting ANTHROPIC_API_KEY from proxy.apiKey: it would outrank claude's \
         claude.ai login and disable every claude.ai connector. The proxy does not need it — \
         its api-key gate exempts loopback clients. Export it yourself if this `claude` has no \
         claude.ai login of its own.",
    )
}

/// SEE-THROUGH mode — the preferred route. `claude` keeps the REAL first-party
/// base URL and reaches us as a CONNECT proxy instead, so we still see (and
/// rotate) every request while Claude Code's first-party check keeps passing.
///
/// That check is a pure string compare on `ANTHROPIC_BASE_URL` — no DNS, no
/// socket, no certificate inspection, just `new URL(e).host === "api.anthropic.com"`.
/// So the fix is to stop lying to it: leave the base URL alone and move the
/// interception down a layer to `HTTPS_PROXY`, where tcr's CONNECT handler
/// MITM-terminates `api.anthropic.com` with a leaf `NODE_EXTRA_CA_CERTS` makes
/// node trust.
///
/// The proxy vars are set in BOTH cases deliberately: clients disagree about
/// which spelling they read, and one that reads only the one we skipped would go
/// direct — bypassing rotation entirely, silently.
fn apply_see_through_env(cmd: &mut std::process::Command, port: u16, ca: &Path) {
    let proxy = format!("http://127.0.0.1:{port}");
    cmd.env("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    cmd.env("HTTPS_PROXY", &proxy);
    cmd.env("https_proxy", &proxy);
    cmd.env("NODE_EXTRA_CA_CERTS", ca);
    // Unnecessary here — we ARE first-party in this mode, so nothing gates them
    // off — but harmless, and they keep the session whole if it ever falls back.
    apply_capability_defaults(cmd);
    eprintln!(
        "[tcr] see-through mode: claude keeps https://api.anthropic.com, tunnelling via {proxy}"
    );
    eprintln!(
        "[tcr] trusting our MITM leaf via NODE_EXTRA_CA_CERTS={}",
        ca.display()
    );
}

/// BASE-URL mode — the fallback, used when see-through material is unavailable.
/// `claude` talks plain HTTP to us on loopback, which costs first-party status
/// and everything gated on it (hence [`apply_capability_defaults`]).
fn apply_base_url_env(cmd: &mut std::process::Command, port: u16) {
    cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
    // Here we speak plain HTTP on loopback and present NO leaf. An ambient
    // HTTPS_PROXY (e.g. the JS teamclaude on :3456) would hijack claude's traffic
    // away from us, and its NODE_EXTRA_CA_CERTS would be verifying a cert we never
    // send. Strip both so `tcr run` is self-contained and can't be captured by a
    // stale env. See-through mode does the opposite — it SETS these two, which is
    // precisely why the strip cannot live at the branch above.
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NODE_EXTRA_CA_CERTS",
    ] {
        cmd.env_remove(var);
    }
    apply_capability_defaults(cmd);
    eprintln!("[tcr] base-URL mode: routing claude through http://127.0.0.1:{port}");
}

/// Re-enable the capabilities a non-first-party `ANTHROPIC_BASE_URL` silently
/// switches off.
///
/// Claude Code gates tool search, the stall watchdog, and fine-grained tool
/// streaming on `xn()==="firstParty" && Yd()`, and base-URL mode fails that check
/// -- no error, at most a [DEBUG] line. We caused it, so we carry the
/// compensation. Never applied with the proxy down: we launch claude untouched
/// there and genuinely are first-party.
///
/// Measured 2026-07-29 against Claude Code 2.1.220 (gate at bundled-JS abs offset
/// 230310702): 1 of 4 same-day sessions lost tool search outright -- the one that
/// reached the gate ~30ms sooner, before settings.json's env block was applied to
/// process.env. Setting these here puts them in the child env at EXEC time, so
/// that ordering race cannot occur at all.
///
/// Only set what the user has not already chosen; an explicit value always wins.
fn apply_capability_defaults(cmd: &mut std::process::Command) {
    for (var, val) in [
        // Without this ~130 tool schemas load eagerly every request. Requires that we
        // forward `tool_reference` blocks upstream untouched -- we do, since
        // build_upstream_headers uses a denylist rather than an allowlist.
        ("ENABLE_TOOL_SEARCH", "true"),
        // Stall detection on the response stream; without it a hung response is never
        // proactively aborted.
        ("CLAUDE_ENABLE_BYTE_WATCHDOG", "1"),
        // Incremental tool-input streaming rather than batched delivery.
        ("CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING", "true"),
    ] {
        if std::env::var_os(var).is_none() {
            cmd.env(var, val);
        }
    }
}

/// `tcr login` — browser OAuth PKCE flow that authenticates a Claude account
/// and appends (or updates) it in the drop-in config, OR (`--token`) a
/// `claude setup-token` credential read from stdin. The heavy lifting lives
/// in [`oauth::login`] / [`oauth::login_with_token`]; this just resolves the
/// config path and reports.
async fn run_login(args: LoginArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    if args.from_claude_code {
        let name = oauth::login_from_claude_code(
            &config_path,
            args.force,
            args.name.as_deref(),
            args.account.as_deref(),
        )
        .await
        .context("importing the Claude Code login failed")?;
        println!("Logged in as '{name}'.");
        return Ok(());
    }
    if args.token {
        let name = oauth::login_with_token(
            &config_path,
            args.force,
            args.name.as_deref(),
            args.account.as_deref(),
        )
        .await
        .context("setup-token login failed")?;
        println!("Logged in as '{name}'.");
        return Ok(());
    }
    let ui = if args.non_interactive {
        oauth::LoginUi::Machine
    } else {
        oauth::LoginUi::Terminal
    };
    let result = oauth::login(
        &config_path,
        args.force,
        args.account.as_deref(),
        args.name.as_deref(),
        ui,
    )
    .await
    .context("OAuth login failed");
    let name = match result {
        Ok(name) => name,
        // A machine caller gets the failure the same way it got every other
        // step — one JSON line on stdout — and the human-readable half on one
        // stderr line rather than as anyhow's indented multi-line chain,
        // which a GUI would have to render as a wall of text. Exits here
        // rather than returning the error, because `main`'s reporter would
        // print that chain on top of what was just said.
        Err(error) if args.non_interactive => {
            let reason = oauth::one_line_reason(&error);
            println!("{}", oauth::LoginEvent::Error { reason: &reason }.line());
            eprintln!("{reason}");
            std::process::exit(1);
        }
        Err(error) => return Err(error),
    };
    // The machine stream already said this, as `{"event":"saved",…}`.
    if !args.non_interactive {
        println!("Logged in as '{name}'.");
    }
    Ok(())
}

/// `tcr mint --account <name> | --group <name>` — mint a long-lived token for
/// one account or every account in a group and put the result on the
/// clipboard. Exits non-zero when any targeted account failed or mismatched,
/// so a UI driving this (the TcrBar menu item) can surface the failure.
async fn run_mint(args: MintArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    let all_ok = mint::run_mint(&config_path, args.account.as_deref(), args.group.as_deref())
        .await
        .context("mint failed")?;
    if !all_ok {
        std::process::exit(1);
    }
    Ok(())
}

/// A stand-down that resolved cleanly: a peer proxy holds the port and is
/// serving this binary's code (or something we have no reason to doubt).
const EXIT_STOOD_DOWN_OK: i32 = 0;
/// A stand-down where the incumbent is serving a DIFFERENT commit than the
/// binary that was just run.
///
/// `cargo build && tcr` used to GUARANTEE the new binary was serving; standing
/// down silently broke that guarantee, and a warning on stderr is routinely
/// unread in a headless or piped context. This is the machine-readable half, so
/// `tcr && <next step>` stops instead of proceeding as if the new build were
/// live. Not `1`: that is a genuine startup failure, and not `2`, which clap
/// uses for a usage error.
const EXIT_STOOD_DOWN_STALE: i32 = 3;
/// A stand-down where the incumbent never answered the liveness probe — it holds
/// the listening socket and serves nothing. Distinct from [`EXIT_STOOD_DOWN_STALE`]
/// because the operator's next command is different: `--replace` is a recovery
/// here, not an upgrade.
const EXIT_STOOD_DOWN_NOT_ANSWERING: i32 = 4;

/// The stand-down's exit code, as a pure function of what was actually measured.
///
/// Keyed on the probe's verdict values, never on the rendered sentence: an exit
/// code grepped out of our own prose is a gate any rewording silently disarms.
///
/// Liveness outranks build skew because it is the more urgent fact — nothing is
/// serving at all — and because a proxy that would not answer also could not
/// report a build, so its build verdict is `Unknown` by construction.
fn stand_down_exit_code(liveness: &cli::Liveness, verdict: build_info::StandDownBuild) -> i32 {
    if matches!(liveness, cli::Liveness::Silent { .. }) {
        return EXIT_STOOD_DOWN_NOT_ANSWERING;
    }
    match verdict {
        build_info::StandDownBuild::Stale => EXIT_STOOD_DOWN_STALE,
        // `Unknown` stays 0: an older tcr that answers but ships no build stamp
        // is a working proxy, and failing every such start would be noise for a
        // question that was never answered either way.
        build_info::StandDownBuild::InSync
        | build_info::StandDownBuild::DirtyBuild
        | build_info::StandDownBuild::Unknown => EXIT_STOOD_DOWN_OK,
    }
}

/// `tcr server` — the clap→[`server::ServeOptions`] adapter.
///
/// Everything that actually boots the proxy lives in [`teamclaude_rs::server`],
/// which is usable from a test or any other embedder. What stays here is what
/// only a *binary* may do: choose the logging subscriber, print the operator's
/// stand-down diagnosis, turn that stand-down into a process exit code, and pick
/// how to wait (the TUI, or a headless block on Ctrl-C).
async fn run_server(args: ServerArgs) -> anyhow::Result<()> {
    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let (config, persist_path) = load_config(&config_path)?;

    // A first boot with an empty fleet imports this machine's Claude Code
    // login, if it has one — here, BEFORE the manager and the fleet are built
    // from `config`, so the very first request already has an account to serve
    // it rather than a 429 until someone runs `tcr login`. Never on a config
    // that already has accounts (see `auto_import_claude_code_login`), and
    // never fatal: the failure paths all warn and boot with zero accounts,
    // exactly as this line did before.
    let config = if config.accounts.is_empty() {
        match oauth::auto_import_claude_code_login(&config_path).await {
            // Re-read: the import wrote the file, and re-reading is what makes
            // the running fleet the one on disk.
            Some(_) => load_config(&config_path).map_or(config, |(fresh, _)| fresh),
            None => config,
        }
    } else {
        config
    };

    init_tracing(args.headless);

    // Auto-migrating `throttle` in memory (see `config::load`) is only half the
    // fix Gil asked for — "our code should auto migrate" means the file itself
    // stops carrying the stale key, so the operator is never re-warned on every
    // boot and a later `tcr accounts`/`tcr status` sees a config that already
    // reflects the split. This is deliberately the ONLY place that persists a
    // migration: read-only CLI verbs migrate in memory and stay read-only. See
    // `migration_persist_target` for what gates the write.
    if let Some(path) = migration_persist_target(&config, &persist_path) {
        match config::save(path, &config) {
            Ok(()) => {
                let msg = format!(
                    "migrated the legacy `throttle` config key to `accountThrottle`/\
                     `fleetThrottle` and rewrote {} — this should only happen once",
                    path.display()
                );
                tracing::warn!("{msg}");
                eprintln!("[tcr] {msg}");
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "auto-migrated the legacy `throttle` key in memory, but failed to \
                     persist the migration to disk; it will re-run at every future boot \
                     until this is fixed"
                );
                eprintln!(
                    "[tcr] warning: could not persist the auto-migrated config to {}: {err}",
                    path.display()
                );
            }
        }
    }

    // Spelled out rather than built from `ServeOptions::new`, which is
    // deliberately inert: writing the config back, owning the shared pin cache
    // and signalling whatever holds the port are all things only the BINARY may
    // do, so the binary is the place they are written down.
    let options = server::ServeOptions {
        config,
        persist_path,
        port: args.port,
        incumbent: if args.replace {
            server::IncumbentPolicy::kill_the_incumbent_proxy()
        } else {
            server::IncumbentPolicy::replace_legacy_js_only()
        },
        affinity_path: Some(affinity::default_path()),
        // The Sessions/Tools panel cache, a binary-only side effect exactly like the
        // pin cache above and for the same reason: one shared file, one writer.
        wire_sessions_path: Some(teamclaude_rs::session_wire_persist::default_path()),
        // The shared usage ledger, a binary-only side effect exactly like the
        // pin cache above and for the same reason: one directory, one writer.
        usage_dir: Some(teamclaude_rs::usage::default_dir()),
        tls: server::TlsSetup::Load,
        // This is a standalone `tcr` process, stated rather than sniffed from
        // `argv[0]`: the owner file is what makes a proxy identifiable when its
        // process name is NOT `tcr` (see `teamclaude_rs::singleton`), so the value
        // has to come from the caller that knows.
        host: singleton::ProxyHost::Cli,
        // Claim the port for the next `tcr` (and for `tcr login`) to read. A
        // binary-only side effect, like the pin cache above: it is a shared
        // directory, so a library caller must opt in with its own.
        //
        // The DIRECTORY, not a path: `serve` names the file after the port it
        // actually binds. Re-deriving `--port unwrap_or config.proxy.port` here to
        // build the name would be a second copy of a resolution rule that lives in
        // `serve`, and the two silently disagreeing means a claim named for a port
        // this process never bound — which every reader looks straight past.
        owner_dir: Some(singleton::default_owner_dir()),
        // The binary always binds for itself. A handed-over socket arrives
        // through the handoff path, which constructs its own options.
        inherited_listener: None,
    };

    let handle = match server::serve(options).await? {
        server::ServeOutcome::Started(handle) => handle,
        // A binary may exit; the library returned this as a value.
        server::ServeOutcome::StoodDown(stand_down) => stand_down_exit(&stand_down),
    };
    let bound = handle.addr();

    let mut handle = handle;
    if args.headless {
        // Installed BEFORE the "listening" line below, not after: that line is
        // what a caller (or `tests/headless_sigterm.rs`) waits on before it
        // treats this process as ready to signal. A handler installed after
        // the print leaves a real window, small, but wide enough for a
        // heavily loaded box to hit it, where a SIGTERM sent the instant the
        // line appears finds no handler yet and the default disposition kills
        // the process immediately, skipping `handle.shutdown()` below
        // entirely: no drain, no final session->account pin flush, no
        // owner-file removal. Registering first closes that window: by the
        // time the readiness line is on the wire, the handler is already
        // armed.
        //
        // TcrBar always launches this process with `--headless` and stops it
        // with `process.terminate()` — a SIGTERM, not Ctrl-C
        // (`apps/macos/.../ServerController.swift`). Mirror the TUI branch's
        // SIGTERM handling (below) so a supervised stop falls through to the
        // same shared cleanup instead of a hard kill.
        //
        // This cannot turn into an unkillable process: `handle.shutdown()`
        // is `shutdown_within(DEFAULT_SHUTDOWN_GRACE)` (5s, see
        // `server::DEFAULT_SHUTDOWN_GRACE`), documented as bounded — tasks
        // that miss the grace are aborted and counted in
        // `report.tasks_aborted`, warned about just below.
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        if sigterm.is_none() {
            tracing::warn!("could not install SIGTERM handler; a supervised stop will kill this process without draining");
        }
        tracing::info!("teamclaude-rs listening on http://{bound} (headless)");
        // Block until Ctrl-C, SIGTERM, or the server task exits.
        let trigger = tokio::select! {
            _ = tokio::signal::ctrl_c() => ShutdownTrigger::CtrlC,
            _ = async {
                match sigterm.as_mut() {
                    Some(sig) => {
                        sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => ShutdownTrigger::Sigterm,
            () = handle.serving_stopped() => ShutdownTrigger::ServingStopped,
        };
        tracing::info!("{}", trigger.shutdown_line());
    } else {
        // The TUI owns the foreground. Under raw mode Ctrl-C arrives as a keystroke,
        // so the loop (not a signal) handles it. But an EXTERNAL SIGTERM — e.g. the
        // singleton replacing us on the port — would otherwise terminate the process
        // with the terminal still in raw + alternate-screen mode, wrecking the
        // caller's shell. Race the TUI against SIGTERM: on the signal the `select`
        // drops the TUI future, and dropping it runs `TerminalGuard`'s destructor,
        // which restores the terminal (the same restore path as a clean quit or a
        // panic) BEFORE the shutdown/flush below and any SIGKILL fallback.
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        if sigterm.is_none() {
            tracing::warn!("could not install SIGTERM handler; terminal may not restore if killed");
        }
        let tui_fut = tui::run(handle.manager().clone());
        tokio::pin!(tui_fut);
        tokio::select! {
            res = &mut tui_fut => {
                if let Err(err) = res {
                    tracing::error!(error = %err, "tui error");
                }
            }
            _ = async {
                match sigterm.as_mut() {
                    Some(sig) => {
                        sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("SIGTERM received; restoring terminal and shutting down");
            }
        }
    }

    // Stop serving, then flush the config and the affinity pins. The whole
    // sequence — including the final pin write a clean shutdown owes the next
    // boot — lives in `ServerHandle::shutdown`, so an embedder gets it too.
    // Bounded there, so a wedged filesystem cannot turn quitting the TUI into a
    // hang with the terminal already restored and nothing left serving.
    let report = handle.shutdown().await;
    if report.tasks_aborted > 0 {
        tracing::warn!(
            aborted = report.tasks_aborted,
            "background task(s) did not stop within the shutdown grace and were aborted"
        );
    }
    Ok(())
}

/// Which event broke the headless `select!` in [`run_server`] out of its
/// wait, so it can fall through to the shared `handle.shutdown()` below.
/// Kept as a plain enum the `select!` arms produce, rather than matching
/// inline in each arm, so the mapping from event to log message lives in
/// one `match`.
///
/// The coverage claim — that SIGTERM is actually one of these triggers —
/// is NOT proven by a list living beside this enum: a hand-written list
/// can drift from the `select!` arms with nothing to notice, which is
/// exactly the shape of gate that cannot fail for the defect it exists to
/// catch. It is proven by `tests/headless_sigterm.rs`, which spawns the
/// real binary, sends it a real SIGTERM, and asserts on the externally
/// observable effects of a graceful shutdown (the log line and the
/// port-owner claim being withdrawn) — deleting the SIGTERM arm here makes
/// that test fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownTrigger {
    CtrlC,
    Sigterm,
    ServingStopped,
}

impl ShutdownTrigger {
    /// The line this trigger logs on the way out. Every variant returns one.
    ///
    /// `ServingStopped` used to return nothing: its match arm was `=> {}`, and
    /// the only thing after the match is a `warn!` that fires solely when tasks
    /// had to be aborted. So a shutdown down that path left the durable log
    /// unable to say the process had stopped, let alone why. This repo counts
    /// boots with `rg 'server started' ~/.cache/teamclaude/logs/*` precisely
    /// because a restart is the most expensive event in the system; a stop with
    /// no matching line makes that count unreadable.
    ///
    /// Returned rather than logged inline so every variant is forced to have
    /// one by the type system, and so `every_shutdown_trigger_logs_a_line` can
    /// check them without a running server.
    fn shutdown_line(self) -> &'static str {
        match self {
            Self::CtrlC => "shutdown signal received",
            Self::Sigterm => "SIGTERM received; shutting down",
            Self::ServingStopped => "serving stopped on its own; shutting down",
        }
    }
}

/// How to recover from a WEDGED incumbent — the half of the not-answering warning
/// that depends on which proxy is holding the port.
///
/// `--replace` is the recovery for a proxy this process may signal. It is not one
/// for a [`singleton::ProxyKind::TcrEmbedded`] incumbent, and offering it there is
/// worse than offering nothing: `takeover_decision` refuses that kind on every
/// path, so the operator runs the suggested command, sees the same stand-down, and
/// the advice that DOES work — quitting the host application — was never printed.
/// An instruction that cannot succeed is a bug even when the code behind it is
/// correct.
fn wedged_incumbent_recovery(kind: singleton::ProxyKind) -> &'static str {
    match kind {
        singleton::ProxyKind::TcrEmbedded => {
            "`tcr --replace` cannot take this one over: the pid belongs to the host application \
             serving the proxy in-process, and signalling it would kill the app without its \
             normal shutdown, losing the session→account pin map. Quit the host application and \
             start it again to recover a wedged embedded proxy."
        }
        singleton::ProxyKind::Tcr | singleton::ProxyKind::LegacyJs => {
            "Run `tcr --replace` to take the port over; that is the recovery for a wedged proxy, \
             and it is not being done automatically because it also wipes the pin map of a proxy \
             that was merely slow to answer."
        }
    }
}

/// Print the stand-down diagnosis and exit with the code it earned. Never returns.
///
/// This is the half of the stand-down a *library* must not do, which is why
/// [`server::serve`] hands the facts back as a [`server::StandDown`] instead.
/// The wording is a cross-language contract: TcrBar scans this stderr, and
/// `ServerController.StandDownExit` switches on the code.
fn stand_down_exit(stand_down: &server::StandDown) -> ! {
    // Standing down is cheap and correct, but silent success here would mean
    // `cargo build && tcr` exits 0 with the OLD build still serving — say which
    // build actually holds the port before we go.
    eprintln!("{}", stand_down.report.line);
    if let cli::Liveness::Silent { why } = &stand_down.probe.liveness {
        let port = stand_down.port;
        let pid = stand_down.pid;
        let recovery = wedged_incumbent_recovery(stand_down.kind);
        eprintln!(
            "[tcr] WARNING incumbent-not-answering: port={port} pid={pid} probe={why:?} — the \
             process holding :{port} did not respond, so standing down leaves NOTHING serving \
             on it. {recovery}"
        );
    }
    std::process::exit(stand_down_exit_code(
        &stand_down.probe.liveness,
        stand_down.report.verdict,
    ));
}

/// Where (if anywhere) `run_server` should persist an in-memory `throttle`
/// migration: only when `load` actually migrated something, a file path exists
/// to write it to, and no account is quarantined. Pulled out as a pure
/// function so the decision is unit-testable without booting a real server —
/// see the `migration_persist_target_*` tests below.
///
/// The quarantine gate mirrors `cli::load_for_edit`: writing back a `Config`
/// while an account is quarantined would serialize over its raw JSON
/// (`importFrom` pointer included) and drop it permanently, so this skips the
/// write and leaves the on-disk file for a human to fix — same hazard, same
/// guard.
fn migration_persist_target<'a>(
    config: &Config,
    persist_path: &'a Option<PathBuf>,
) -> Option<&'a Path> {
    if config.migrated_legacy_throttle && config.quarantined_accounts.is_empty() {
        persist_path.as_deref()
    } else {
        None
    }
}

/// Load the config, deciding what may be written back:
/// - missing file → [`config::load_or_init`] writes the defaults out before
///   returning them, so the server's own first boot leaves a real file on disk
///   (what every other verb now also sees) instead of running on a default the
///   next `tcr status` cannot find;
/// - corrupt/unreadable existing file → **refuse to start**. This used to fall
///   back to in-memory defaults (a zero-account fleet) and boot anyway — a
///   proxy that binds its port and answers every request with 429 while
///   looking alive, which is worse than refusing outright: a dead proxy that
///   won't start is obvious, and this one was not (see `config::load`'s
///   doc-comment on the migration this replaced). A missing file is a
///   legitimate first run and still boots on defaults; a file that exists and
///   fails to parse is now the operator's problem to fix, not something this
///   binary papers over.
fn load_config(path: &Path) -> anyhow::Result<(Config, Option<PathBuf>)> {
    match config::load_or_init(path) {
        Ok((config, _created)) => Ok((config, Some(path.to_path_buf()))),
        Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            // `load_or_init` only hands back NotFound when the CREATE itself
            // could not find its way (an unwritable/absent parent that
            // `create_dir_all` refused); the read's own NotFound is handled
            // there. Booting on defaults keeps that case serving, exactly as a
            // missing file used to.
            eprintln!(
                "[tcr] no config at {} and it could not be created ({err}) — starting with defaults",
                path.display()
            );
            Ok((default_config(), Some(path.to_path_buf())))
        }
        Err(err) => anyhow::bail!(
            "config at {} is unreadable/corrupt: {err} — refusing to start rather than \
             serve an empty fleet; fix the file and restart",
            path.display()
        ),
    }
}

/// A default config with every serde default applied (correct `upstream` and
/// `switchThreshold`, empty accounts) — parsing `{}` reuses the config's own
/// `#[serde(default)]` wiring instead of a hand-rolled `Default`.
fn default_config() -> Config {
    serde_json::from_str("{}").expect("an empty JSON object is always a valid default config")
}

/// Base cache directory: `$XDG_CACHE_HOME/teamclaude`, else `$HOME/.cache/teamclaude`.
///
/// Deliberately independent of [`affinity::default_path`] (same env-var
/// resolution order, duplicated rather than shared) so that a bug or a test
/// touching the log path can never brush the live session-affinity pin file's
/// neighbourhood by construction — the two are computed by different code, not
/// just used at different leaf paths under a shared helper.
fn cache_base_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("teamclaude")
}

/// The one shared, well-known, non-private directory both modes log into:
/// `~/.cache/teamclaude/logs/` (or `$XDG_CACHE_HOME/teamclaude/logs/`).
///
/// The path is fixed for **human discoverability** — so `rg 'server started'
/// ~/.cache/teamclaude/logs/*` always finds the right directory — not because
/// any code depends on the literal string. Nothing in this codebase opens this
/// path programmatically outside this module; see `open_log_appender` below and
/// the doc-comment on `the_log_directory_is_the_shared_cache_location` for the
/// census that established that.
fn log_dir_path() -> std::path::PathBuf {
    cache_base_dir().join("logs")
}

/// Open the durable, rotating log directory, injectable for tests so they never
/// touch the real `~/.cache/teamclaude/logs/` — pass a unique temp dir instead
/// of routing through [`log_dir_path`].
///
/// `tracing_appender::rolling` has no per-file `.mode()` hook (checked against
/// its 0.2.5 source: `create_writer` in its `rolling.rs` opens with
/// `OpenOptions::append(true).create(true)` only), so a rotating file cannot be
/// made owner-only the way the old single file was. Confidentiality is enforced
/// on the *directory* instead, and it is load-bearing on Linux CI targets, not
/// belt-and-braces: the parent `~/.cache/teamclaude/` is `0755` (protects its
/// existing contents by *file* mode — `session-affinity.json` is `0600`), so
/// without an owner-only `logs/` subdirectory, files landing at the crate's
/// default `0644` would be world-readable on any multi-user box. The mode is
/// requested atomically at directory-creation time via `DirBuilder::mode`, not
/// create-then-`chmod` — the latter leaves a TOCTOU window where the directory
/// is briefly world-traversable while it starts to hold sensitive log content.
///
/// The 0700 mode is applied at directory creation and re-asserted once at
/// process startup (below). It is **not** a standing invariant:
/// `tracing_appender`'s own fallback (`rolling.rs:795`) recreates the
/// directory with `create_dir_all` and no mode if it is removed externally —
/// at construction and at every rotation — so a `logs/` deleted mid-run
/// silently returns at 0755 for the life of the process. This function closes
/// the pre-existing-directory case (a stale `mkdir -p`, a `tar` restore
/// without `-p`, a baked container path); it cannot reach that crate-internal
/// recreation path, which is not this function's to fix.
///
/// The mode is checked on the **resolved** target, not the path itself:
/// `std::fs::metadata`/`set_permissions` both follow symlinks, and
/// `DirBuilder` succeeds against an existing symlink-to-directory, so a
/// symlinked `logs/` is validated by where it points, not by the link. That
/// is deliberate, not an oversight: pointing the log directory at another
/// volume is a legitimate operator setup, and refusing to start over a
/// symlink would break a working configuration to close a hole that is not a
/// privilege boundary anyway — planting such a symlink first requires write
/// access to the 0755, user-owned `~/.cache/teamclaude/` parent, i.e. the
/// user or root, who already has more reach than this would buy them.
///
/// `.recursive(true)` creates every missing path component, parents included,
/// **carrying the same `0700` mode** — this is not confined to `logs/` itself.
/// On a fresh install or in a container where `~/.cache/teamclaude/` (or even
/// `~/.cache/`) does not yet exist, this call creates it at `0700`, changing a
/// directory shared with every other application on the machine. That is
/// invisible on a dev box where both already exist at `0755`, but it is real,
/// measured behaviour of `DirBuilder::recursive`, not a hypothetical.
///
/// Rotation is `DAILY` with `max_log_files(5)`: measured against the live log
/// (2026-08-08) at ~13.5 MiB/day, this bounds this directory's steady-state
/// disk use to roughly 65-70 MiB instead of genuinely unbounded growth. That
/// is **not** a wash against the old file: the pre-upgrade
/// `$TMPDIR/teamclaude-rs.log` (~62 MiB, measured 2026-08-08) is left in
/// place deliberately (deprecate, don't delete — nothing here removes an
/// operator's existing evidence), so the true post-upgrade footprint is that
/// ~62 MiB orphan **plus** the new 65-70 MiB, until an operator cleans the
/// orphan up by hand. It is a wall-clock bound, not a byte-size bound: a single unusually
/// verbose day can still exceed the per-file average before the next
/// rotation, so this is a soft cap, not a hard one. Rotation also rolls on
/// `Rotation::DAILY`'s UTC clock (`rolling.rs` uses `OffsetDateTime::now_utc`),
/// not local midnight — for the stated goal of human discoverability, that
/// means the daily boundary lands at 03:00 in Asia/Jerusalem, not midnight.
fn open_log_appender(
    log_dir: &std::path::Path,
) -> std::io::Result<tracing_appender::rolling::RollingFileAppender> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(log_dir)?;

    // `DirBuilder::mode` only governs directories it *creates* — a directory
    // that already existed (operator `mkdir -p`, a `tar` restore without
    // `-p`, a baked container path) is untouched by the call above and could
    // be sitting at 0755 or worse. Re-assert here so every process start
    // closes that window, not just a genuinely-fresh directory.
    let mode = std::fs::metadata(log_dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(log_dir, std::fs::Permissions::from_mode(0o700))?;
        let confirmed = std::fs::metadata(log_dir)?.permissions().mode() & 0o777;
        if confirmed & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "log directory {} is not owner-only after chmod (mode {confirmed:#o}); \
                 refusing to log account emails into a world-readable directory",
                log_dir.display()
            )));
        }
    }

    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("teamclaude-rs.log")
        .max_log_files(5)
        .build(log_dir)
        .map_err(std::io::Error::other)
}

/// Build the headless subscriber: every event goes to **both** `stdout_sink` and
/// (when it could be opened) the durable log file.
///
/// Headless used to be stdout-only, which meant its logs were destroyed outright
/// whenever a supervisor discarded the child's stdout — as TcrBar does
/// (`ServerController.swift`, `standardOutput = FileHandle.nullDevice`, and older
/// builds hand it an undrained pipe). A crashing proxy then left no evidence of
/// why. Stdout is kept as well, because someone running `tcr server --headless`
/// in a terminal expects output, and launchd-style supervisors capture it.
///
/// `file` is an `Option` on purpose: a logging failure must never take the proxy
/// down, so a log that will not open degrades to stdout-only.
///
/// `RollingFileAppender` is used directly as the writer, never through
/// `tracing_appender::non_blocking()`. `non_blocking()` returns a `WorkerGuard`
/// that must live as long as logging should happen — drop it (as this
/// function's `()` return type would force, if it owned one) and the
/// background writer thread shuts down with no error and no warning: every
/// event after that silently stops reaching disk while every gate stays green.
/// `RollingFileAppender` itself implements `Write`/`MakeWriter` synchronously,
/// so it needs no guard and no lifetime plumbing.
/// `stdout_ansi` is the caller's decision, made once, because the library's
/// own default is wrong here: `tracing_subscriber::fmt::Layer::default` turns
/// colour on whenever `NO_COLOR` is unset and never asks whether the sink is a
/// terminal, so a redirected `tcr --headless > run.log` fills the file with
/// escape codes. That is a bug by itself (a log nobody can grep), and it broke
/// `tests/peer_e2e.rs` on CI, where `NO_COLOR` is unset and the children's
/// boot logs are files: every field name arrived as
/// `\e[3mpeer_listen\e[0m\e[2m=\e[0m`.
fn headless_subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    file: Option<tracing_appender::rolling::RollingFileAppender>,
    stdout_sink: W,
    stdout_ansi: bool,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt as _;
    // The file sink is non-ANSI: escape codes in a log read back as garbage.
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(stdout_ansi)
                .with_writer(stdout_sink),
        )
        .with(file_layer)
}

/// Initialise tracing. Headless logs to stdout *and* the durable log file; the
/// TUI logs only to the file, so events never corrupt the alternate screen.
/// Either way, a log file that will not open is a warning, never a failure.
fn init_tracing(headless: bool) {
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_dir = log_dir_path();
    let file = match open_log_appender(&log_dir) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!(
                "[tcr] could not open log directory {}: {err}",
                log_dir.display()
            );
            None
        }
    };
    if headless {
        // Colour only for a human at a terminal. Piped or redirected stdout is
        // something a program or a test will read back, and `NO_COLOR` is still
        // honoured on top: see `headless_subscriber`.
        let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout())
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        headless_subscriber(filter, file, std::io::stdout, ansi).init();
        return;
    }
    // No file? Already warned above. The TUI cannot fall back to stdout without
    // corrupting the alternate screen, so it runs without tracing rather than
    // refusing to start.
    if let Some(file) = file {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_info::StandDownBuild;
    use std::os::unix::fs::PermissionsExt as _;

    fn silent() -> cli::Liveness {
        cli::Liveness::Silent {
            why: "the server did not answer within 5s".to_string(),
        }
    }

    /// `TCR_CLAUDE_BIN` outranks `CLAUDE_BIN` when both are set.
    #[test]
    fn tcr_claude_bin_wins_over_the_legacy_key() {
        assert_eq!(
            resolve_harness_bin_from(
                Some("/opt/beta/claude".to_string()),
                Some("/opt/legacy/claude".to_string()),
                "claude",
            ),
            "/opt/beta/claude",
        );
    }

    /// `CLAUDE_BIN` is used when the tcr-specific key is unset.
    #[test]
    fn claude_bin_is_the_fallback_when_tcr_claude_bin_is_unset() {
        assert_eq!(
            resolve_harness_bin_from(None, Some("/opt/legacy/claude".to_string()), "claude"),
            "/opt/legacy/claude",
        );
    }

    /// An empty value counts as unset, for either key — a shell that leaves
    /// `TCR_CLAUDE_BIN=` behind must not silently win over a real `CLAUDE_BIN`.
    #[test]
    fn an_empty_value_is_treated_as_missing() {
        assert_eq!(
            resolve_harness_bin_from(
                Some(String::new()),
                Some("/opt/legacy/claude".to_string()),
                "claude",
            ),
            "/opt/legacy/claude",
            "an empty TCR_CLAUDE_BIN must fall through to CLAUDE_BIN"
        );
        assert_eq!(
            resolve_harness_bin_from(Some(String::new()), Some(String::new()), "claude"),
            "claude",
            "two empty values must fall through to the default"
        );
    }

    /// Neither key set falls back to the default (`claude` on `PATH`).
    #[test]
    fn neither_key_set_falls_back_to_the_default() {
        assert_eq!(resolve_harness_bin_from(None, None, "claude"), "claude");
    }

    /// Pins that a configured proxy key is withheld from `claude`, and says why.
    #[test]
    fn the_proxy_key_is_withheld_from_claude_with_the_reason() {
        let notice = withheld_api_key_notice(true, false).expect("a configured key is withheld");
        assert!(
            notice.contains("connector"),
            "the notice must name what exporting it costs: {notice}"
        );
        assert!(
            notice.contains("Export it yourself"),
            "the notice must name the escape hatch: {notice}"
        );

        assert_eq!(
            withheld_api_key_notice(false, false),
            None,
            "nothing is withheld when no proxy key is configured"
        );
        assert_eq!(
            withheld_api_key_notice(true, true),
            None,
            "a key the caller exported is their choice — we neither replace it nor comment"
        );
    }

    /// Neither routing mode may hand `claude` an `ANTHROPIC_API_KEY`.
    #[test]
    fn no_routing_mode_gives_claude_an_api_key() {
        for (label, apply) in [
            (
                "see-through",
                &(|cmd: &mut std::process::Command| {
                    apply_see_through_env(cmd, 3456, Path::new("/tmp/ca.pem"))
                }) as &dyn Fn(&mut std::process::Command),
            ),
            (
                "base-URL",
                &(|cmd: &mut std::process::Command| apply_base_url_env(cmd, 3456)),
            ),
        ] {
            let mut cmd = claude_command();
            apply(&mut cmd);
            assert!(
                !cmd.get_envs()
                    .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_some()),
                "{label} mode must not set ANTHROPIC_API_KEY — it outranks the claude.ai \
                 login and disables every claude.ai connector"
            );
        }
    }

    /// The re-entry marker must reach the child, and must not be named such that
    /// the wrapper it informs deletes it on the way.
    ///
    /// cmux's claude wrapper runs `for k in ${!CMUX_@}; do unset "$k"; done` before
    /// exec'ing the real binary. A marker under that prefix would be swept exactly
    /// where it is needed, and the double-wrap would come back looking like a bug
    /// in the launcher rather than in the name.
    #[test]
    fn marker_survives_the_cmux_prefix_sweep() {
        assert!(
            !RUN_ACTIVE_ENV.starts_with("CMUX_"),
            "{RUN_ACTIVE_ENV} would be erased by cmux's own CMUX_* sweep before the \
             launcher that reads it ever runs"
        );

        let mut cmd = claude_command();
        mark_run_active(&mut cmd);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == RUN_ACTIVE_ENV && v == Some("1".as_ref())),
            "every `tcr run` child must carry {RUN_ACTIVE_ENV}"
        );
    }

    /// A WEDGED INCUMBENT MUST NOT BE OFFERED A RECOVERY THAT CANNOT WORK.
    ///
    /// `--replace` is refused for an embedded incumbent on every path in
    /// `singleton::takeover_decision`, deliberately: the pid is the host
    /// application's. Printing "Run `tcr --replace`" for that kind sends the
    /// operator around a loop that ends in the same stand-down, while the
    /// instruction that does work — quit the host application — never appears.
    #[test]
    fn a_wedged_embedded_incumbent_is_not_told_to_run_replace() {
        let embedded = wedged_incumbent_recovery(singleton::ProxyKind::TcrEmbedded);
        assert!(
            !embedded.contains("Run `tcr --replace`"),
            "must not prescribe an override that this kind refuses: {embedded}"
        );
        assert!(
            embedded.contains("Quit the host application"),
            "must name the recovery that works: {embedded}"
        );
        // The control: for the kinds `--replace` CAN take over, the prescription is
        // unchanged — so the assertions above are about the kind, not about the
        // advice having been dropped for everyone.
        for kind in [singleton::ProxyKind::Tcr, singleton::ProxyKind::LegacyJs] {
            let advice = wedged_incumbent_recovery(kind);
            assert!(
                advice.contains("Run `tcr --replace`"),
                "{kind:?} is recoverable by --replace: {advice}"
            );
        }
    }

    /// The ordinary stand-down: a peer is serving this binary's commit. Exit 0,
    /// or every `tcr` in a script becomes a failure.
    #[test]
    fn a_clean_stand_down_exits_zero() {
        for verdict in [
            StandDownBuild::InSync,
            StandDownBuild::DirtyBuild,
            StandDownBuild::Unknown,
        ] {
            assert_eq!(
                stand_down_exit_code(&cli::Liveness::Answering, verdict),
                0,
                "{verdict:?} is a working incumbent"
            );
        }
    }

    /// DETECTED BUILD SKEW MUST NOT EXIT 0. The whole point of the stale-server
    /// warning is that `cargo build && tcr` no longer guarantees the new binary
    /// is serving; returning success anyway leaves the guarantee broken for every
    /// script, CI step and launchd job, which read the code and not the stderr.
    #[test]
    fn a_stale_incumbent_exits_non_zero() {
        let code = stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::Stale);
        assert_ne!(code, 0, "a detected skew must be visible to `tcr && next`");
        assert_ne!(code, 1, "1 is a genuine startup failure");
        assert_ne!(code, 2, "2 is clap's usage error");
        assert_eq!(code, EXIT_STOOD_DOWN_STALE);
    }

    /// THE WEDGED PROXY. Nothing is serving on the port, so exiting 0 tells every
    /// caller — and TcrBar — that the server is up. The code has to say
    /// otherwise, and it outranks the build verdict: a proxy that will not answer
    /// cannot report a build either, so `Unknown` is what it always comes with.
    #[test]
    fn a_silent_incumbent_exits_its_own_non_zero_code() {
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Unknown),
            EXIT_STOOD_DOWN_NOT_ANSWERING
        );
        assert_ne!(EXIT_STOOD_DOWN_NOT_ANSWERING, 0);
        assert_ne!(
            EXIT_STOOD_DOWN_NOT_ANSWERING, EXIT_STOOD_DOWN_STALE,
            "the two need different recoveries, so they need different codes"
        );
        // Liveness outranks every build verdict, including a comparable one.
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::InSync),
            EXIT_STOOD_DOWN_NOT_ANSWERING,
            "a matching sha from a process that answers nothing is not a healthy port"
        );
    }

    /// THE EXIT CODES ARE A CROSS-LANGUAGE CONTRACT, exactly like
    /// `singleton::INCUMBENT_MARKER`, and nothing but this test couples them.
    ///
    /// TcrBar switches on the numbers: `ServerController.StandDownExit` in
    /// `apps/macos/Sources/TcrBarCore/ServerController.swift` declares
    /// `stale = 3` and `notAnswering = 4`, and `classifyExit` turns them into
    /// `.incumbentIsStale` and `.incumbentNotAnswering`. Renumbering a constant
    /// here is a one-character edit that every other Rust test survives, while
    /// the menu-bar app silently falls through to a bare `.exited(5, …)` and
    /// reports a wedged proxy — one serving NOTHING — as a clean exit. That is
    /// the misreport this whole round exists to eliminate.
    ///
    /// The numbers are SPELLED OUT rather than referenced through the constants,
    /// deliberately: `assert_eq!(EXIT_STOOD_DOWN_STALE, EXIT_STOOD_DOWN_STALE)`
    /// compares a value with itself and passes for every value of it. The
    /// constant is the thing that must not drift, so the test has to hold the
    /// other copy — the one Swift carries.
    #[test]
    fn the_stand_down_exit_codes_are_the_numbers_tcrbar_switches_on() {
        // Transcribed from ServerController.StandDownExit.
        let tcrbar_stale: i32 = 3;
        let tcrbar_not_answering: i32 = 4;

        assert_eq!(
            EXIT_STOOD_DOWN_OK, 0,
            "a clean stand-down is success; anything else fails every `tcr && next`"
        );
        assert_eq!(
            EXIT_STOOD_DOWN_STALE, tcrbar_stale,
            "ServerController.StandDownExit.stale is 3 — change one, change both"
        );
        assert_eq!(
            EXIT_STOOD_DOWN_NOT_ANSWERING, tcrbar_not_answering,
            "ServerController.StandDownExit.notAnswering is 4 — change one, change both"
        );

        // The constants being right is worthless if the mapping does not emit
        // them, so the contract is asserted through the function TcrBar's input
        // actually comes from, against the same literals.
        assert_eq!(
            stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::InSync),
            0
        );
        assert_eq!(
            stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::Stale),
            3,
            "a stale incumbent must reach Swift as .incumbentIsStale"
        );
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Unknown),
            4,
            "a wedged incumbent must reach Swift as .incumbentNotAnswering"
        );

        // Liveness outranks build skew across the boundary too: a proxy that
        // answers nothing is not merely stale, and 4 must win over 3 — the
        // operator's next command differs (recover, not upgrade).
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Stale),
            4,
            "NOT SERVING outranks a stale build; reporting 3 here understates it"
        );

        // Three outcomes, three codes. Two of them collapsing would make the
        // Swift switch pick one arm for both.
        let codes = [
            EXIT_STOOD_DOWN_OK,
            EXIT_STOOD_DOWN_STALE,
            EXIT_STOOD_DOWN_NOT_ANSWERING,
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "the stand-down codes must stay distinct: {codes:?}");
            }
        }
        // And neither may take a code that already means something else.
        assert!(
            !codes[1..].contains(&1),
            "1 is a genuine startup failure (anyhow::Error out of main)"
        );
        assert!(
            !codes[1..].contains(&2),
            "2 is clap's usage error, which TcrBar maps to the unknown-argument hint"
        );
    }

    /// `--no-replace` is documented as a deprecated no-op, and used to be wired as
    /// a SILENT VETO over `--replace`: an operator whose launchd plist or alias
    /// already carried it, adding `--replace` to force a rebuilt binary onto the
    /// port, took over nothing and got exit 0. clap must reject the contradiction
    /// by name instead.
    #[test]
    fn replace_and_no_replace_together_are_a_usage_error() {
        // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which does
        // not implement Debug (nor should it — it would print the config path).
        let Err(err) = Cli::try_parse_from(["tcr", "server", "--replace", "--no-replace"]) else {
            panic!("the pair is a contradiction, not a precedence puzzle — clap accepted it");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "it must fail as a conflict, not as some other parse error: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("--no-replace") && rendered.contains("--replace"),
            "the message must name BOTH flags so the operator knows what to remove: {rendered}"
        );
    }

    /// `--token` and `--from-claude-code` are two different credentials from
    /// two different places; asking for both is a contradiction, not a
    /// precedence puzzle. clap must reject it by name rather than let one
    /// silently win.
    #[test]
    fn token_and_from_claude_code_together_are_a_usage_error() {
        // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which
        // does not implement Debug (nor should it — it would print the config
        // path).
        let Err(err) = Cli::try_parse_from(["tcr", "login", "--token", "--from-claude-code"])
        else {
            panic!("--token with --from-claude-code must not parse");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "it must fail as a conflict, not as some other parse error: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("--token") && rendered.contains("--from-claude-code"),
            "the message must name BOTH flags so the operator knows what to remove: {rendered}"
        );
    }

    /// Each alone still parses, and `--from-claude-code` combines with the
    /// flags a browser login combines with — `--account` especially, which
    /// `--token` has to refuse (an inference-only credential has no identity
    /// to confirm; this one does).
    #[test]
    fn from_claude_code_parses_alone_and_with_account_and_name() {
        for args in [
            vec!["tcr", "login", "--from-claude-code"],
            vec!["tcr", "login", "--from-claude-code", "--account", "work"],
            vec!["tcr", "login", "--from-claude-code", "--name", "work"],
            vec!["tcr", "login", "--from-claude-code", "--force"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
        }
    }

    /// Each flag alone still parses — the deprecated one is accepted, as promised
    /// to the scripts and launch agents that already pass it.
    #[test]
    fn each_replace_flag_alone_still_parses() {
        for args in [
            vec!["tcr", "server", "--replace"],
            vec!["tcr", "server", "--no-replace"],
            vec!["tcr", "--no-replace"],
            vec!["tcr", "server"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
        }
    }

    /// A unique, test-only log directory keyed by pid + nanosecond timestamp.
    /// Never the real `~/.cache/teamclaude/logs/` — tests must not touch the
    /// live proxy's cache directory (`session-affinity.json` lives one level up
    /// and is being written by a running process).
    fn unique_log_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "teamclaude-rs-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }

    /// The rotating log directory (account emails + request paths inside it)
    /// must be created owner-only. `tracing_appender` has no per-file
    /// `.mode()` hook (verified against its 0.2.5 source — `create_writer` in
    /// `rolling.rs` opens with `OpenOptions::append(true).create(true)` only),
    /// so confidentiality is enforced on the directory, not the file: `0700`
    /// blocks traversal into the directory for everyone but the owner, even
    /// though the files landing inside it carry whatever mode the process
    /// umask gives a freshly `OpenOptions::create`d file (commonly `0644` —
    /// world-readable in their *own* bits, but unreachable because nothing
    /// outside the owner can resolve a path through a `0700` parent).
    #[test]
    fn log_directory_is_created_owner_only() {
        let dir = unique_log_dir("dirmode");
        // Goes through the production opener: re-implementing the mode here
        // would assert 0o700 == 0o700 and pass however `open_log_appender`
        // drifts.
        let _appender = open_log_appender(&dir).expect("open temp log dir");
        let dir_mode = std::fs::metadata(&dir)
            .expect("stat temp log dir")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(dir_mode, 0o700, "log directory must be owner-only (0700)");
    }

    /// `DirBuilder::mode` only sets the mode of directories it *creates* — a
    /// directory that already exists (operator `mkdir -p`, a `tar` restore
    /// without `-p`, a baked container path) is left exactly as it was found.
    /// `open_log_appender` must re-assert `0700` on an already-existing
    /// directory, not merely request it at creation time, or a pre-existing
    /// `0755` `logs/` silently ships every rotated file world-readable.
    #[test]
    fn log_directory_is_re_asserted_owner_only_when_it_already_exists() {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = unique_log_dir("dirmode-preexisting");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .recursive(true)
            .create(&dir)
            .expect("pre-create the dir at 0755, simulating an external mkdir/restore");
        let preexisting_mode = std::fs::metadata(&dir)
            .expect("stat pre-created dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            preexisting_mode, 0o755,
            "test setup sanity: the pre-created dir must actually be 0755"
        );

        let _appender = open_log_appender(&dir).expect("open pre-existing log dir");
        let dir_mode = std::fs::metadata(&dir)
            .expect("stat temp log dir")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            dir_mode, 0o700,
            "a pre-existing, wrongly-permissioned log directory must be re-asserted to 0700"
        );
    }

    /// A `MakeWriter` that keeps what was written, so a test can inspect the
    /// stdout sink without capturing the process's real stdout.
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            let bytes = self.0.lock().expect("shared buffer poisoned").clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("shared buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Reads back whatever `open_log_appender` actually wrote inside `dir` —
    /// there is exactly one matching file after a single, unrotated write.
    /// This is the "it writes", not merely "it initialised", proof: a dropped
    /// `WorkerGuard` (see `headless_subscriber`'s doc-comment) would make
    /// `init_tracing`-style construction succeed while nothing ever lands here.
    fn read_the_one_log_file(dir: &std::path::Path) -> String {
        let mut matches: Vec<_> = std::fs::read_dir(dir)
            .expect("read temp log dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("teamclaude-rs.log")
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one log file in {dir:?}, found {matches:?}"
        );
        std::fs::read_to_string(matches.pop().unwrap().path()).expect("read the log file")
    }

    /// HEADLESS MUST REACH DISK. TcrBar spawns `tcr server --headless` and throws
    /// the child's stdout away, so a stdout-only headless subscriber destroys
    /// 100% of the proxy's logs — which is exactly why a restart loop could not
    /// be diagnosed. Both sinks are asserted: dropping either one is a
    /// regression, and asserting only the file would let stdout silently die for
    /// everyone running it in a terminal.
    #[test]
    fn headless_logging_reaches_both_the_file_and_stdout() {
        let dir = unique_log_dir("headless-test");
        let appender = open_log_appender(&dir).expect("open temp log dir");
        let stdout = SharedBuf::default();
        let marker = format!("headless-sink-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            Some(appender),
            stdout.clone(),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{marker}");
        });

        let on_disk = read_the_one_log_file(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            on_disk.contains(&marker),
            "headless event never reached the log file; file held: {on_disk:?}"
        );
        assert!(
            !on_disk.contains('\u{1b}'),
            "the log file must be non-ANSI; escape codes read back as garbage"
        );
        assert!(
            stdout.contents().contains(&marker),
            "headless event never reached stdout; stdout held: {:?}",
            stdout.contents()
        );
    }

    /// Redirected stdout carries no escape codes.
    ///
    /// The library default would: `Layer::default` enables colour whenever
    /// `NO_COLOR` is unset, whatever the sink is, so a `tcr --headless >
    /// run.log` on a machine without `NO_COLOR` wrote a log that no `rg` of a
    /// field name could match. Asserted on both sinks, and against the same
    /// sink with colour ON, so a `stdout_ansi` argument that is quietly
    /// ignored fails here rather than on somebody's CI runner.
    #[test]
    fn headless_stdout_is_plain_when_it_is_not_a_terminal() {
        let plain = SharedBuf::default();
        let marker = format!("headless-ansi-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            plain.clone(),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(peer_listen = %"127.0.0.1:34865", "{marker}");
        });

        let written = plain.contents();
        assert!(
            !written.contains('\u{1b}'),
            "non-terminal stdout must hold no escape byte; held: {written:?}"
        );
        assert!(
            written.contains("peer_listen=127.0.0.1:34865"),
            "a reader must find the plain `key=value`; held: {written:?}"
        );

        // The control: the same call with colour on must differ, which is what
        // proves the assertion above measures the argument.
        let coloured = SharedBuf::default();
        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            coloured.clone(),
            true,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(peer_listen = %"127.0.0.1:34865", "{marker}");
        });
        assert!(
            coloured.contents().contains('\u{1b}'),
            "with colour on the same event carries escapes, so the plain case above is a real result"
        );
    }

    /// A log file that will not open must degrade to stdout-only, never take the
    /// proxy down: logging is not worth the traffic it is observing.
    #[test]
    fn headless_logging_survives_an_unopenable_log_file() {
        let stdout = SharedBuf::default();
        let marker = format!("headless-degraded-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            stdout.clone(),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{marker}");
        });

        assert!(
            stdout.contents().contains(&marker),
            "with no log file, stdout must still receive every event"
        );
    }

    /// `init_tracing` must target the one shared, well-known cache directory,
    /// not a private/generated path — so a human running the documented
    /// `rg 'server started' ~/.cache/teamclaude/logs/*` recipe can always find
    /// it. This is **not** because any code reads it back (a census —
    /// `rg -n 'teamclaude-rs\.log|log_file_path|log_dir_path' src/ scripts/
    /// apps/`, repo-relevant scope, not `src/` alone: a `src/`-only census once
    /// missed `scripts/validate-cache.sh`'s own copy of the old fixed path —
    /// found no programmatic reader anywhere outside this module; the previous
    /// version of this test justified itself with "this is what `tcr status`
    /// and every diagnostic already read", which was false and is not
    /// repeated here).
    #[test]
    fn the_log_directory_is_the_shared_cache_location() {
        assert_eq!(log_dir_path(), cache_base_dir().join("logs"));
        assert!(
            log_dir_path().ends_with(std::path::Path::new("teamclaude").join("logs")),
            "log directory must live under the shared teamclaude cache dir: {:?}",
            log_dir_path()
        );
    }

    /// `max_log_files` pruning is real, not merely configured, AND it is the
    /// production `open_log_appender`'s own hardcoded `max_log_files(5)`
    /// being exercised — not a hand-rolled duplicate of the config, so a
    /// regression that drops or weakens the production call is what this test
    /// is sensitive to. Pre-creates 5 dated files (the `prefix.date` shape
    /// `join_date()` produces for any non-`NEVER` rotation), each with a
    /// distinct creation time, then calls the real production opener —
    /// `prune_old_logs()` runs at *construction*, before the first new file is
    /// created (verified against 0.2.5 source, `rolling.rs:615-617`), which is
    /// what makes this deterministic: no wall-clock rotation boundary needs to
    /// be crossed.
    ///
    /// This is portable across the two CI targets, not merely convenient on
    /// one. `prune_old_logs()` sorts by `metadata.created()` where the
    /// platform supports it, but explicitly falls back to parsing the date out
    /// of the filename itself when it does not (`rolling.rs:689-696`,
    /// `parse_date_from_filename`) — and this test's embedded dates
    /// (`2020-02-01` .. `2020-02-05`) sort in the same order as their real
    /// creation timestamps, so the assertions hold identically whichever path
    /// the pruner takes. Verified directly against this repo's own targets:
    /// macOS (APFS) supports `created()`; the crate's own filename-parsing
    /// fallback exists specifically because ext4/most Linux filesystems (this
    /// repo's `ci` job runs `ubuntu-latest`) commonly return
    /// `ErrorKind::Unsupported` for it — an attempt to break this test via
    /// that path does not succeed, because the fallback is exercised, not
    /// skipped.
    #[test]
    fn old_log_files_are_pruned_at_max_log_files() {
        let dir = unique_log_dir("pruning");
        std::fs::create_dir_all(&dir).expect("create temp log dir");

        // Five pre-existing dated files, oldest first, each with a distinct
        // creation time (the pruner sorts by `metadata.created()`).
        let oldest = "2020-02-01";
        let survivors = ["2020-02-02", "2020-02-03", "2020-02-04", "2020-02-05"];
        for date in std::iter::once(&oldest).chain(survivors.iter()) {
            std::fs::write(dir.join(format!("teamclaude-rs.log.{date}")), b"old\n")
                .expect("write stale log file");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Production max_log_files(5): prune keeps (5 - 1) = 4 of the 5
        // existing files, then the appender creates one new file for today —
        // 5 total, with only the single oldest pre-existing file removed.
        let _appender = open_log_appender(&dir).expect("open temp log dir");

        let remaining: std::collections::BTreeSet<String> = std::fs::read_dir(&dir)
            .expect("read temp log dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            remaining.len() >= 2,
            "at least 2 files must survive pruning: {remaining:?}"
        );
        assert!(
            !remaining.contains(&format!("teamclaude-rs.log.{oldest}")),
            "the single oldest file must have been pruned: {remaining:?}"
        );
        for date in survivors {
            assert!(
                remaining.contains(&format!("teamclaude-rs.log.{date}")),
                "pre-existing file {date} must survive pruning: {remaining:?}"
            );
        }
    }

    // ---- `--group` header composition (`compose_group_header`) ----------------

    #[test]
    fn group_header_with_no_inherited_value_is_just_ours() {
        assert_eq!(
            compose_group_header(None, "codereview"),
            "x-tcr-group: codereview"
        );
        // Empty is treated the same as absent.
        assert_eq!(
            compose_group_header(Some(""), "codereview"),
            "x-tcr-group: codereview"
        );
    }

    #[test]
    fn group_header_appends_to_unrelated_inherited_headers() {
        let composed = compose_group_header(Some("X-Custom: yes"), "codereview");
        assert_eq!(composed, "X-Custom: yes\nx-tcr-group: codereview");
    }

    #[test]
    fn group_header_replaces_rather_than_duplicates_an_existing_group_line() {
        let composed = compose_group_header(
            Some("X-Custom: yes\nx-tcr-group: stale\nX-Other: also-kept"),
            "codereview",
        );
        assert_eq!(
            composed, "X-Custom: yes\nX-Other: also-kept\nx-tcr-group: codereview",
            "the stale line is replaced in place at the END, never duplicated, \
             and every unrelated line's text and order survive"
        );
        assert_eq!(
            composed.matches("x-tcr-group").count(),
            1,
            "must never carry two group headers"
        );
    }

    #[test]
    fn group_header_replacement_matches_the_name_case_insensitively() {
        let composed = compose_group_header(Some("X-TCR-Group: stale"), "codereview");
        assert_eq!(composed, "x-tcr-group: codereview");
    }

    // ---- `--group` claude version gate (`classify_claude_version_output`) -----

    #[test]
    fn claude_version_parses_the_real_output_shape() {
        assert_eq!(
            parse_claude_version("2.1.237 (Claude Code)"),
            Some((2, 1, 237))
        );
        assert_eq!(parse_claude_version("2.1.237"), Some((2, 1, 237)));
    }

    #[test]
    fn claude_version_parse_fails_closed_on_garbage() {
        for garbage in ["", "not a version", "v2.1.237", "2.1", "2.1.abc"] {
            assert_eq!(
                parse_claude_version(garbage),
                None,
                "{garbage:?} must not parse as a version"
            );
        }
    }

    #[test]
    fn claude_version_too_old_refuses() {
        assert_eq!(
            classify_claude_version_output("2.1.226 (Claude Code)"),
            ClaudeVersionCheck::TooOld("2.1.226".to_string())
        );
        assert_eq!(
            classify_claude_version_output("1.9.999 (Claude Code)"),
            ClaudeVersionCheck::TooOld("1.9.999".to_string())
        );
    }

    /// The comparison must be NUMERIC per-component, not lexicographic on the
    /// version string. `"2.1.9"` sorts AFTER `"2.1.227"` as a plain string
    /// (`'9' > '2'`), which would wrongly classify it `Ok` against the
    /// 2.1.227 minimum; numerically `9 < 227`, so it must be `TooOld`. Every
    /// other version-gate test here (`2.1.226`/`1.9.999` vs `2.1.227`) would
    /// pass under EITHER comparison and so does not guard this property —
    /// this is the one case where the two implementations disagree.
    #[test]
    fn claude_version_compares_patch_numerically_not_lexicographically() {
        assert_eq!(
            classify_claude_version_output("2.1.9 (Claude Code)"),
            ClaudeVersionCheck::TooOld("2.1.9".to_string()),
            "2.1.9 must be TooOld against a 2.1.227 minimum — a lexicographic \
             compare would wrongly say Ok because '9' > '2' as characters"
        );
    }

    #[test]
    fn claude_version_at_or_above_minimum_is_ok() {
        assert_eq!(
            classify_claude_version_output("2.1.227 (Claude Code)"),
            ClaudeVersionCheck::Ok
        );
        assert_eq!(
            classify_claude_version_output("2.1.237 (Claude Code)"),
            ClaudeVersionCheck::Ok
        );
        assert_eq!(
            classify_claude_version_output("3.0.0 (Claude Code)"),
            ClaudeVersionCheck::Ok
        );
    }

    #[test]
    fn claude_version_unparseable_output_warns_and_proceeds() {
        assert_eq!(
            classify_claude_version_output("garbage"),
            ClaudeVersionCheck::Unknown
        );
        assert_eq!(
            classify_claude_version_output(""),
            ClaudeVersionCheck::Unknown
        );
    }

    // ---- `--group` name validation (`validate_group`) --------------------------

    fn account_with_groups(name: &str, groups: Option<&[&str]>) -> config::Account {
        config::Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: None,
            expires_at: None,
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: groups.map(|gs| gs.iter().map(|g| g.to_string()).collect()),
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: crate::config::Egress::Local,
            egress_strict: false,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn validate_group_accepts_a_configured_group() {
        let mut config = default_config();
        config.accounts = vec![
            account_with_groups("alice", Some(&["codereview"])),
            account_with_groups("bob", None),
        ];
        assert!(validate_group(&config, "codereview").is_ok());
    }

    #[test]
    fn validate_group_rejects_an_unknown_name_and_lists_the_configured_groups() {
        let mut config = default_config();
        config.accounts = vec![
            account_with_groups("alice", Some(&["codereview", "burst"])),
            account_with_groups("bob", Some(&["burst"])),
        ];
        let err = validate_group(&config, "typo-group")
            .expect_err("an unconfigured group name must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("burst") && msg.contains("codereview"),
            "the error must name every configured group so the operator can fix the typo: {msg}"
        );
    }

    #[test]
    fn validate_group_rejects_when_no_account_has_any_group() {
        let mut config = default_config();
        config.accounts = vec![account_with_groups("alice", None)];
        assert!(
            validate_group(&config, "codereview").is_err(),
            "a typo must never silently resolve to the empty set — that routes everywhere"
        );
    }

    // ---- `--group` label character validation (`validate_group_label_chars`) --

    #[test]
    fn group_label_chars_accepts_an_ordinary_ascii_label() {
        assert_eq!(validate_group_label_chars("codereview"), Ok(()));
        assert_eq!(validate_group_label_chars("code-review_2"), Ok(()));
    }

    #[test]
    fn group_label_chars_rejects_empty_and_whitespace_only() {
        assert_eq!(
            validate_group_label_chars(""),
            Err("empty or whitespace-only")
        );
        assert_eq!(
            validate_group_label_chars("   "),
            Err("empty or whitespace-only")
        );
    }

    #[test]
    fn group_label_chars_rejects_a_newline() {
        assert_eq!(
            validate_group_label_chars("code\nreview"),
            Err("contains a newline")
        );
        assert_eq!(
            validate_group_label_chars("code\rreview"),
            Err("contains a newline")
        );
    }

    #[test]
    fn group_label_chars_rejects_other_control_characters() {
        assert_eq!(
            validate_group_label_chars("code\0review"),
            Err("contains a control character")
        );
        assert_eq!(
            validate_group_label_chars("code\treview"),
            Err("contains a control character")
        );
    }

    #[test]
    fn group_label_chars_rejects_codepoints_above_u00ff() {
        assert_eq!(
            validate_group_label_chars("codereview\u{1F600}"), // an emoji
            Err("contains a codepoint above U+00FF")
        );
    }

    #[test]
    fn validate_group_rejects_a_group_argument_with_an_embedded_newline() {
        let mut config = default_config();
        config.accounts = vec![account_with_groups("alice", Some(&["codereview"]))];
        let err = validate_group(&config, "code\nreview")
            .expect_err("a newline in the --group argument must be refused");
        assert!(
            err.to_string().contains("newline"),
            "the error must name the character class at fault: {err}"
        );
    }

    #[test]
    fn validate_group_rejects_a_config_declared_label_with_a_control_character() {
        let mut config = default_config();
        config.accounts = vec![account_with_groups("alice", Some(&["code\0review"]))];
        let err = validate_group(&config, "codereview")
            .expect_err("a control character in a CONFIG-declared label must also be refused");
        assert!(
            err.to_string().contains("control character"),
            "the error must name the character class at fault: {err}"
        );
    }

    // --- legacy `throttle` migration: boot-time behaviour ---------------------

    fn unique_config_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tcr-main-{tag}-{}-{seq}.json", std::process::id()))
    }

    /// `migration_persist_target` gates the write `run_server` performs after a
    /// migration: an active migration with nowhere quarantined and a real path
    /// on disk gets rewritten.
    #[test]
    fn migration_persist_target_writes_when_clear() {
        let path = unique_config_path("persist-target-clear");
        let mut config = default_config();
        config.migrated_legacy_throttle = true;
        assert_eq!(
            migration_persist_target(&config, &Some(path.clone())),
            Some(path.as_path())
        );
    }

    /// The quarantine gate is not optional (mirrors `cli::load_for_edit`):
    /// writing back a `Config` while an account is quarantined would serialize
    /// over that account's raw JSON (its `importFrom` pointer included) and
    /// drop it permanently. A pending migration must stay in-memory-only until
    /// a human clears the quarantine.
    #[test]
    fn migration_persist_target_is_none_when_an_account_is_quarantined() {
        let path = unique_config_path("persist-target-quarantined");
        let mut config = default_config();
        config.migrated_legacy_throttle = true;
        config.quarantined_accounts = vec!["acct-import".to_string()];
        assert_eq!(migration_persist_target(&config, &Some(path)), None);
    }

    /// Nothing to persist when `load` never migrated anything.
    #[test]
    fn migration_persist_target_is_none_when_nothing_migrated() {
        let path = unique_config_path("persist-target-unmigrated");
        let config = default_config();
        assert_eq!(migration_persist_target(&config, &Some(path)), None);
    }

    /// Nothing to persist without a file path (e.g. the corrupt-config fallback
    /// used to drop the persist path — see `load_config`).
    #[test]
    fn migration_persist_target_is_none_without_a_persist_path() {
        let mut config = default_config();
        config.migrated_legacy_throttle = true;
        assert_eq!(migration_persist_target(&config, &None), None);
    }

    /// A missing config file is a legitimate first run: `load_config` must
    /// still boot on in-memory defaults, keeping the persist path so the first
    /// refresh creates the file.
    #[test]
    fn load_config_creates_the_file_when_it_is_missing() {
        let path = unique_config_path("missing");
        let (config, persist_path) =
            load_config(&path).expect("a missing config file must not refuse to boot");
        assert!(config.accounts.is_empty());
        assert_eq!(persist_path, Some(path.clone()));
        // The server's first boot now LEAVES the file behind, so the `tcr
        // status` a user runs next finds the same config the server booted from
        // rather than nothing at all (`config::load_or_init`).
        assert!(
            path.exists(),
            "booting with no config must create {}",
            path.display()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The behaviour this task changed: a config that exists and fails to
    /// parse (malformed JSON, NOT the legacy `throttle` key — that key is
    /// migrated, never an error, per `config::load`) must make the server
    /// REFUSE to boot rather than silently serve a zero-account fleet that
    /// answers every request with 429 while looking alive.
    #[test]
    fn load_config_refuses_to_boot_on_a_corrupt_config() {
        let path = unique_config_path("corrupt");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let err = load_config(&path).expect_err("a corrupt config must refuse to boot");
        assert!(
            err.to_string().contains("unreadable/corrupt"),
            "the refusal must say why: {err}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Every shutdown trigger must log a distinct, non-empty line.
    ///
    /// `ServingStopped` previously logged NOTHING: its arm was `=> {}` and the
    /// only statement after the match warns solely when tasks were aborted. A
    /// process that stopped down that path left no trace of having stopped.
    ///
    /// Distinctness matters as much as presence. `tests/headless_sigterm.rs`
    /// asserts on the SIGTERM text specifically, so two triggers sharing a line
    /// would make that assertion pass for the wrong trigger, which is a green
    /// test for a broken reason.
    #[test]
    fn every_shutdown_trigger_logs_a_line() {
        let triggers = [
            ShutdownTrigger::CtrlC,
            ShutdownTrigger::Sigterm,
            ShutdownTrigger::ServingStopped,
        ];
        let mut seen: Vec<&str> = Vec::new();
        for trigger in triggers {
            let line = trigger.shutdown_line();
            assert!(
                !line.trim().is_empty(),
                "{trigger:?} logs nothing on the way out"
            );
            assert!(
                !seen.contains(&line),
                "{trigger:?} shares its line with another trigger ({line:?}); \
                 the SIGTERM assertion in tests/headless_sigterm.rs would then \
                 pass for the wrong reason"
            );
            seen.push(line);
        }
        assert_eq!(
            ShutdownTrigger::Sigterm.shutdown_line(),
            "SIGTERM received; shutting down",
            "tests/headless_sigterm.rs matches this exact text; changing it here \
             without changing it there turns that test red for no real reason"
        );
    }
}
