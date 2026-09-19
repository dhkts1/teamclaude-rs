//! Enrolment: two paths, both ending in the same pinned row.
//!
//! # Headless, and it is the default
//!
//! On the accepting node, `tcr peer invite --label laptop-2 --ttl 10m --uses 1`
//! prints one token; on the joiner, `tcr peer join <token>` consumes it. The
//! token carries the registrar's address, its static public key and a 32-byte
//! secret, so the joiner can run `IKpsk1` and prove the secret in message 1.
//! **One paste, no second screen**, which is the only reason a machine with no
//! display can ever join.
//!
//! # Interactive, kept because it is better when two screens exist
//!
//! `tcr peer pair <host:port>` runs `XX`, both panels show the same six digits
//! derived from the handshake hash, and `tcr peer confirm <peer> <code>` on
//! both sides pins it. This is the shape a person already recognises from
//! pairing headphones, and it is the simple surface's default when a peer is
//! discovered rather than typed.
//!
//! # What a token really is, stated rather than softened
//!
//! A bearer secret that passes through a paste buffer and a shell history, and
//! **an outstanding invite is join-capable by anything that can read the peers
//! file.** The registrar must hold PSK material to complete the handshake, so a
//! stored value an attacker could not use is a value the registrar could not
//! use either. The mitigations that actually work are the boring ones: single
//! use, a short TTL, mode 0600, the row deleted on use,
//! [`MAX_OUTSTANDING_INVITES`], and an explicit revoke.
//!
//! # Where the writes are
//!
//! [`crate::peer::config::PeerStore`] reads; it has no write path and no view
//! of the invite table. So the four mutating verbs here read the file through
//! [`crate::peer::config::read_or_default`], which is also the one place the
//! 0600 check lives, change one row, and write it back through
//! [`crate::peer::config::save`]. There was once a second reader and a second
//! writer in this file because those two were `todo!()`; both are gone.
//!
//! # The pairing window
//!
//! `XX` is the one pattern with no prior key, so an `XX` message 1 from a
//! stranger is 32 bytes that make this node disclose its static key in message
//! 2 with no operator anywhere in the loop. A later decision moved the gate this
//! module used to provide (a node-wide open/closed window) to one keyed to a
//! single accepted instance id and address
//! ([`crate::peer::state::PeerState::accepted_window`],
//! [`crate::peer::listener::accept_pairing_or_return`]); outside that window
//! the listener answers an `XX` message 1 with zero bytes, the same silence a
//! failed pin check gets. [`PairingWindow`] and [`open_pairing_window`] below
//! remain the node-wide TYPE and MINTER (nothing in production reads them),
//! kept for [`pairing_window`]'s own test coverage rather than deleted, which
//! is out of scope here.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tcr_peer_wire::{
    decode_key32, encode_key32, sanitize_label, Control, Enroll, PeerId, StreamHeader, StreamKind,
};

use crate::peer::config::{
    read_or_default, save, Endpoint, EndpointSource, PeerFile, PeerRow, PeerStore, PendingInvite,
};
use crate::peer::id::{default_config_dir, NodeKey};
use crate::peer::noise::{self, Handshake, KEY_BYTES};

/// How many invites may be outstanding at once.
///
/// Not a style limit: every outstanding invite is one more PSK the registrar
/// must trial-decrypt message 1 against, which is the same cost this design
/// refuses `KK` for. A cap is what keeps that bounded, and a fleet-sized N
/// never occurring is what makes the cost cheap rather than merely bounded.
pub const MAX_OUTSTANDING_INVITES: usize = 8;

/// The default life of an invite. Short on purpose: the row is join-capable
/// while it exists.
pub const INVITE_DEFAULT_TTL_SECS: u32 = 600;

/// The one-line token an operator pastes.
///
/// Shape: `tcr-join:v1:<host:port>:<b32 static-pub>:<b32 secret32>`. A version
/// field because this string is the one thing an operator moves between two
/// builds by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinToken {
    /// Where to dial. An address, never a name to resolve: discovery is a
    /// separate, optional mechanism and a token must work without it.
    pub addr: SocketAddr,
    /// The registrar's static public key, the `IK` half.
    pub registrar: PeerId,
    /// The 32-byte join secret, the `psk1` half.
    pub secret: [u8; 32],
}

/// What every token starts with. The version is checked, never guessed at.
pub const TOKEN_PREFIX: &str = "tcr-join:v1:";

impl JoinToken {
    /// Render the token for pasting.
    ///
    /// The address stays plain text, an operator who has to read a token out
    /// loud can at least see which machine it points at, and base32ing
    /// `127.0.0.1:9600` hides nothing that the two 52-character fields after it
    /// do not already reveal is there. The two 32-byte fields go through the one
    /// base32 codec this tree has.
    pub fn to_token(&self) -> String {
        format!(
            "{TOKEN_PREFIX}{}:{}:{}",
            self.addr,
            self.registrar.to_wire(),
            encode_key32(&self.secret)
        )
    }

    /// Parse a pasted token. Refuses an unknown version rather than guessing,
    /// because the alternative is a silent wrong-key dial.
    ///
    /// Split from the RIGHT: an IPv6 address carries colons of its own, and the
    /// two fields after it do not, so the two rightmost separators are the only
    /// ones whose position is known.
    pub fn parse(token: &str) -> Result<Self> {
        let token = token.trim();
        let Some(body) = token.strip_prefix(TOKEN_PREFIX) else {
            bail!(
                "peer join: this is not a v1 join key (it must start with {TOKEN_PREFIX:?}); \
                 an unknown version is refused rather than guessed, because guessing it \
                 would be a silent dial to the wrong key"
            );
        };
        let mut fields = body.rsplitn(3, ':');
        let (Some(secret), Some(registrar), Some(addr)) =
            (fields.next(), fields.next(), fields.next())
        else {
            bail!(
                "peer join: a v1 join key is `{TOKEN_PREFIX}<host:port>:<registrar>:<secret>`, \
                 and this one has fewer than three fields after the version"
            );
        };
        let addr: SocketAddr = addr
            .parse()
            .with_context(|| format!("peer join: {addr:?} is not the host:port to dial"))?;
        let registrar = PeerId::parse(registrar).map_err(|refusal| {
            anyhow!("peer join: the registrar field is unreadable: {refusal}")
        })?;
        // The secret is decoded, never echoed: every refusal below names the
        // FIELD and its shape, and no arm of this function prints the value.
        // A pasted token that failed to parse is still a live bearer secret,
        // and the place operators paste one is a terminal that keeps scrollback.
        let secret = decode_key32(secret)
            .map_err(|refusal| anyhow!("{refusal}"))
            .context("peer join: the secret field is not 32 base32-encoded bytes")?;
        Ok(Self {
            addr,
            registrar,
            secret,
        })
    }
}

/// Where `tcr peer join` takes the token from.
///
/// A typed pair rather than an `Option<&str>` plus a `bool`, because the two
/// are mutually exclusive and a caller that passes both has not decided
/// anything, and the argv arm is the one that leaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource<'a> {
    /// Typed on the command line. **The whole token, including its 32-byte
    /// secret, is then visible in `ps` output to every process on this Mac and
    /// in the operator's shell history.** Kept because it is the one-paste
    /// headless path the design is built around, and a machine with no screen
    /// has no better channel.
    Argv(&'a str),
    /// Read from standard input: one line, consumed by this process and never
    /// placed in an argument vector. What the panel's Paste-a-key sheet writes
    /// to (`tcr peer join --stdin`), and the only source that keeps the secret
    /// off `ps`.
    Stdin,
}

/// Read the join token from wherever the operator put it.
///
/// **Nothing here logs or prints the token.** A refusal names the field and
/// its shape (see [`JoinToken::parse`]), because a terminal keeps scrollback
/// and a failed paste is still a live bearer secret.
pub fn read_join_token(source: TokenSource<'_>) -> Result<JoinToken> {
    match source {
        TokenSource::Argv(token) => JoinToken::parse(token),
        TokenSource::Stdin => token_from_reader(std::io::stdin().lock()),
    }
}

/// [`read_join_token`]'s stdin arm, against any reader, so a test can drive it
/// without a terminal.
///
/// One line: a token is one line by construction, and reading to end of file
/// instead would make a trailing newline plus anything after it part of the
/// secret field.
pub fn token_from_reader<R: std::io::BufRead>(mut reader: R) -> Result<JoinToken> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("peer join: could not read the join key from standard input")?;
    if line.trim().is_empty() {
        bail!(
            "peer join: standard input carried no join key (`--stdin` expects the \
             `{TOKEN_PREFIX}…` line on stdin, so the key never enters this process's argv)"
        );
    }
    JoinToken::parse(&line)
}

/// Mint an invite: generate the secret, store the row, return the token.
///
/// The label is sanitized here, at the point it is accepted, by the one shared
/// sanitizer, not at the beacon, and not at display time. A label reaches
/// every pinned peer through a `Hello` whether discovery ever ships or not.
pub fn mint_invite(
    store: &PeerStore,
    label: &str,
    ttl_secs: u32,
    uses: u8,
) -> Result<(PendingInvite, JoinToken)> {
    let node = NodeKey::load_or_mint(&default_config_dir())
        .context("peer invite: this node has no keypair to invite anyone to")?;
    mint_invite_as(store, &node, label, ttl_secs, uses)
}

/// [`mint_invite`], against an explicit [`NodeKey`].
///
/// The skeleton's signature carries no key, and a token cannot be built without
/// one: its middle field IS the registrar's static public half, which is what
/// lets the joiner run `IK` at all. [`mint_invite`] loads this node's own key
/// and delegates, so nothing calls a `PeerId` it did not prove.
pub fn mint_invite_as(
    store: &PeerStore,
    node: &NodeKey,
    label: &str,
    ttl_secs: u32,
    uses: u8,
) -> Result<(PendingInvite, JoinToken)> {
    if uses == 0 {
        bail!("peer invite: an invite with zero uses admits nobody");
    }
    let label = sanitize_label(label).map_err(|refusal| anyhow!("peer invite: {refusal}"))?;

    // Locked like every other read-modify-write of this file (see
    // `accept_enrolment`'s doc comment): unlocked, this mint's own save can
    // clobber a concurrent `revoke_invite`'s save (or the reverse), a lost
    // update that either resurrects a revoked invite or drops a row
    // `accept_enrolment` just pinned under its own lock.
    let _lock = crate::peer::config::FileLock::acquire(store.path())?;
    let mut file = read_or_default(store.path())?;
    let Some(addr) = file.listen else {
        bail!(
            "peer invite: this node has no peer listener, so a token would carry no address \
             to dial (`tcr peer find on` opens one)"
        );
    };

    let now = now_ms();
    // An expired row is not an outstanding invite, so it does not spend the
    // cap. Dropped here rather than filtered at read time, because the file is
    // being rewritten anyway and a dead PSK on disk is still a PSK on disk.
    file.pending_invites
        .retain(|invite| invite.uses_left > 0 && invite.expires_at_ms > now);
    if file.pending_invites.len() >= MAX_OUTSTANDING_INVITES {
        bail!(
            "peer invite: {} invites are already outstanding, which is the cap \
             (MAX_OUTSTANDING_INVITES); every one of them is a PSK the registrar must try \
             against message 1, so `tcr peer invite --revoke <id>` first",
            file.pending_invites.len()
        );
    }

    let secret = noise::random_secret()?;
    let invite = PendingInvite {
        id: random_id()?,
        label,
        secret,
        expires_at_ms: now.saturating_add(i64::from(ttl_secs).saturating_mul(1000)),
        uses_left: uses,
    };
    file.pending_invites.push(invite.clone());
    save(store.path(), &file)?;

    let token = JoinToken {
        addr,
        registrar: node.id(),
        secret,
    };
    Ok((invite, token))
}

/// Revoke one outstanding invite by id.
///
/// Locked, the same shape as [`accept_enrolment`] and [`mint_invite_as`]:
/// unlocked, a revoke that reads before and saves after a concurrent mint or
/// enrolment silently loses the other write, either bringing the revoked
/// invite back or dropping a row that enrolment just pinned.
pub fn revoke_invite(store: &PeerStore, id: u64) -> Result<bool> {
    let _lock = crate::peer::config::FileLock::acquire(store.path())?;
    let mut file = read_or_default(store.path())?;
    let before = file.pending_invites.len();
    file.pending_invites.retain(|invite| invite.id != id);
    let removed = file.pending_invites.len() != before;
    if removed {
        save(store.path(), &file)?;
    }
    Ok(removed)
}

/// The registrar's half of enrolment: pin the joiner, and spend the invite it
/// proved.
///
/// `secret` is the PSK that decrypted message 1, handed over by
/// [`crate::peer::noise::read_message_1_matching`]. It identifies the invite
/// because the token carries no invite id, so [`Enroll::invite_id`] is not
/// consulted here and cannot be used to retire somebody else's row.
///
/// **One write, both effects.** The pin and the decrement land in a single
/// read/modify/write of the peers file: two writes could leave a spent invite
/// with no pinned row (a joiner that can never enrol and an operator with no
/// way to see why) or a pinned row with a live invite (a one-use key that
/// admits a second machine).
///
/// **And one write is not enough on its own.** "Single read/modify/write" is a
/// statement about this function's own body, and two copies of this body
/// running at once each read a file with `usesLeft: 1` and each decide they may
/// pin, measured at 40 double-accepts out of 40
/// trials, which is a one-use key admitting two machines every single time. So
/// the read, the mutate and the save are held under
/// [`crate::peer::config::FileLock`], and the invite is re-checked INSIDE the
/// lock: the loser of the race finds no outstanding invite and refuses, which
/// is the correct answer and the one an operator can act on.
///
/// The joiner's claimed label is UNTRUSTED input off the wire, so it goes
/// through the one shared sanitizer, and a label that fails is a refusal
/// rather than a substitution: this repository is public, the label reaches
/// every panel row and every later `Hello`, and quietly filing a hostile
/// joiner under the operator's own invite label would hide which machine
/// actually arrived. The invite is not spent on that path.
///
/// `from` is the socket the enrolment arrived on, recorded as the joiner's
/// first endpoint: a registrar that pinned a joiner and kept no way back to it
/// would have trusted a machine it cannot reach.
///
/// Ordering and why the gate does not require a pinned row here:
/// [`crate::peer::noise`]'s module docs.
pub fn accept_enrolment(
    peers_path: &Path,
    joiner: PeerId,
    enroll: &Enroll,
    secret: &[u8; KEY_BYTES],
    now_ms: i64,
    from: SocketAddr,
) -> Result<PeerRow> {
    let label = sanitize_label(&enroll.label)
        .map_err(|refusal| anyhow!("peer enrol: the joiner's label is refused: {refusal}"))?;

    // Held across the read, the mutate and the save. See the doc-comment: the
    // lock is what makes a one-use invite admit one machine, and the read below
    // is deliberately inside it rather than before it.
    let _lock = crate::peer::config::FileLock::acquire(peers_path)?;
    let mut file = read_or_default(peers_path)?;

    let Some(position) = file.pending_invites.iter().position(|invite| {
        &invite.secret == secret && invite.uses_left > 0 && invite.expires_at_ms > now_ms
    }) else {
        bail!(
            "peer enrol: no outstanding invite matches the secret this handshake proved \
             (it was spent or it expired between message 1 and this frame); nothing was pinned"
        );
    };

    // Spend one use, and delete the row at zero: a row with `usesLeft: 0` on
    // disk is still a PSK on disk, and the module docs say what that means.
    let invite = {
        let invite = &mut file.pending_invites[position];
        invite.uses_left = invite.uses_left.saturating_sub(1);
        invite.clone()
    };
    if invite.uses_left == 0 {
        file.pending_invites.remove(position);
    }

    let mut row = PeerRow {
        node: joiner,
        label,
        endpoints: Vec::new(),
        added_at: now_ms,
        allow: crate::peer::config::Allow::default(),
        lend: Vec::new(),
        // A pin is not a session: the secret arrives when this pair next
        // completes one, which is the same instant the register learns it.
        rendezvous_secret: None,
        sees_us_at: None,
    };
    // The socket this enrolment arrived on. Observed by this node and not
    // claimed by the joiner, which is the only kind of address worth writing:
    // a completed TCP handshake makes it real and the Noise handshake against
    // the invite secret makes the peer real.
    row.observe_endpoint(Endpoint::direct(from, now_ms, EndpointSource::Paired));
    // A re-enrolment of a key already pinned replaces the row rather than
    // adding a second one, for the reason `pin_row` documents.
    file.peers.retain(|existing| existing.node != joiner);
    file.peers.push(row.clone());
    save(peers_path, &file)?;

    tracing::info!(
        peer = %joiner.display(),
        label = %row.label,
        invite_id = invite.id,
        uses_left = invite.uses_left,
        "peer enrol: pinned a joiner that proved an outstanding invite"
    );
    Ok(row)
}

/// Run the joiner's half: dial, `IKpsk1`, pin the registrar, be pinned.
pub async fn join(store: &PeerStore, token: &JoinToken, label: &str) -> Result<PeerId> {
    let node = NodeKey::load_or_mint(&default_config_dir())
        .context("peer join: this node has no keypair to enrol with")?;
    join_as(store, &node, token, label).await
}

/// [`join`], against an explicit [`NodeKey`].
///
/// The same seam [`mint_invite_as`] has, for the same reason: [`join`] resolves
/// this machine's key out of [`default_config_dir`], and a test that drove it
/// would read and write the operator's real config directory. With the key as a
/// parameter, both halves of an enrolment run in one process against two temp
/// directories, which is what
/// `a_two_sided_enrolment_leaves_a_pinned_row_on_both_sides`
/// (`tests/peer_noise.rs`) does.
pub async fn join_as(
    store: &PeerStore,
    node: &NodeKey,
    token: &JoinToken,
    label: &str,
) -> Result<PeerId> {
    let label = sanitize_label(label).map_err(|refusal| anyhow!("peer join: {refusal}"))?;

    let mut stream = tokio::net::TcpStream::connect(token.addr)
        .await
        .with_context(|| format!("peer join: could not reach {}", token.addr))?;
    let mut session = noise::dial_handshake(
        &mut stream,
        node.secret_bytes(),
        Handshake::Enrol,
        Some(&token.registrar.0),
        Some(&token.secret),
    )
    .await
    .context("peer join: the enrolment handshake failed, a spent or expired key is refused")?;

    // **The stream header comes first, on this stream like every other one.**
    // An enrolment is a CONTROL stream, the wire contract says the first
    // transport message on every stream is the header, and sending the
    // `Enroll` as the first frame would make the registrar's listener parse an
    // enrolment as a header and close the connection. It also keeps the
    // registrar's enrolment path one exemption wide (no pinned row is required
    // for the first frame) instead of two (no row AND no header).
    let header = serde_json::to_vec(&StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: random_request_id()?,
    })
    .context("peer join: the stream header did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &header).await?;

    // The registrar identifies WHICH invite this is by the PSK that matched
    // message 1, because the token carries no invite id. `invite_id` is
    // therefore zero here and the field is reported to the lead rather than
    // filled with a guess.
    let enroll = serde_json::to_vec(&Control::Enroll(Enroll {
        invite_id: 0,
        label,
    }))
    .context("peer join: the enrolment message did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &enroll).await?;

    // **Wait for the registrar's answer before pinning or reporting success.**
    //
    // This is the other half of that hole: the handshake succeeding
    // proves the join KEY was good, and nothing more. The registrar can still
    // refuse what comes after it, a label that fails the sanitizer, an invite
    // spent by another machine between message 1 and this frame. This
    // function once pinned the registrar and returned `Ok` either
    // way, so `tcr peer join` printed `ok` while the other Mac recorded
    // nothing.
    //
    // The answer is a `Control::Hello`, which is what a pinned peer may always
    // receive on a CONTROL stream, so the ack needs no new wire type: receiving
    // one means the registrar reached
    // [`crate::peer::listener`]'s enrolment arm, wrote the pinned row and now
    // treats this node as a peer. Anything else, a closed stream, a different
    // message, silence, is a refusal, and a refusal pins nothing on this side
    // either. Order and rationale: [`crate::peer::noise`]'s module docs.
    let answer = tokio::time::timeout(
        ENROL_ACK_TIMEOUT,
        noise::recv_encrypted(&mut stream, &mut session.transport),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "peer join: the registrar accepted the key and then said nothing for {} seconds, \
             so this Mac cannot tell whether it was enrolled; nothing was pinned here",
            ENROL_ACK_TIMEOUT.as_secs()
        )
    })?
    .context(
        "peer join: the registrar closed the stream without answering the enrolment, which \
         is what its refusal looks like (a label it would not accept, or an invite spent \
         between the handshake and this frame); nothing was pinned here",
    )?;

    match serde_json::from_slice::<Control>(&answer) {
        Ok(Control::Hello(_)) => {}
        Ok(other) => bail!(
            "peer join: the registrar answered the enrolment with {other:?} rather than the \
             Hello that means it pinned this Mac; nothing was pinned here"
        ),
        Err(err) => {
            return Err(err).context(
                "peer join: the registrar's answer to the enrolment is not a control message",
            )
        }
    }

    pin_row(
        store,
        token.registrar,
        &registrar_label(token.addr)?,
        Some(token.addr),
    )?;
    Ok(token.registrar)
}

/// How long the joiner waits for the registrar's `Hello` before calling the
/// enrolment refused.
///
/// Generous, because the registrar writes a file in that window and an
/// operator is watching a terminal, not a latency budget; bounded, because a
/// registrar that says nothing must produce an error rather than a hang.
const ENROL_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Phase one: knock, so the operator at the other Mac sees a pairing request.
///
/// This is the only way a first pairing starts: "pressing Trust
/// SENDS a knock to that Mac", and the direct-`XX` path that used to exist is
/// gone. The knock reveals no static key on either side
/// ([`crate::peer::noise::PATTERN_KNOCK`]) and earns one ack byte, which means
/// "queued", never "accepted".
///
/// `network_key` comes from this Mac's own peers file when it has one set. A
/// knock to a Mac whose key expectation differs fails on the far side inside
/// message 1 and arrives here as "did not answer", which is the honest
/// rendering of an opt-in shared secret.
pub async fn knock(store: &PeerStore, addr: SocketAddr, proposed_name: &str) -> Result<()> {
    let file = read_or_default(store.path())?;
    let name = sanitize_label(proposed_name).ok();
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("peer pair: could not reach {addr}"))?;
    noise::send_knock(
        &mut stream,
        &tcr_peer_wire::Knock {
            instance_id: crate::peer::id::boot_instance_id(),
            proposed_name: name,
            wire_version: tcr_peer_wire::PROTO_VERSION,
        },
        file.network_key.as_ref().map(|key| key.as_bytes()),
    )
    .await
}

/// How long [`pair`] keeps retrying the `XX` dial while it waits for the
/// operator at the other Mac to press Accept.
///
/// Ten minutes, which is a knock's own life
/// ([`crate::peer::state::KNOCK_TTL_MS`]) and the honest bound: approval
/// "can come during the handshake (Accept, then compare) or after
/// (the knock sits in the list until the operator comes back)", so this command
/// has to be able to sit through someone walking to the other room. It is a
/// deadline, not a spin: [`PAIR_RETRY_INTERVAL`] is the gap.
pub const PAIR_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// How long [`pair`] waits between `XX` attempts. One second: an operator
/// pressing Accept should see the digits appear, and a dial per second against
/// one Mac the same operator is standing at costs nothing.
pub const PAIR_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Run phase two: `XX` under the window the other Mac's Accept opened, then
/// hand back the six digits for the operator to compare.
///
/// Retries until the far side accepts, up to [`PAIR_WAIT`], because that is
/// what "approval can come during the handshake or after" means for a caller:
/// before Accept, an `XX` message 1 gets zero bytes and the dial fails, and the
/// command has to keep offering rather than tell the operator to run it again
/// at exactly the right moment.
///
/// The message-1 payload is this Mac's own
/// [`tcr_peer_wire::InstanceId`], the same one the knock carried, because
/// that is the value the far side's window is keyed to
/// ([`crate::peer::state::PeerState::accepted_window`]).
///
/// The store is the skeleton's parameter and phase two has nothing to ask it: a
/// first pairing is by definition a key nothing has pinned, so there is no row
/// to read, and the write is [`confirm`]'s once the operator has compared the
/// digits. Kept rather than re-signed, and named `_store` rather than discarded
/// with a `let _`. Reported to the lead.
pub async fn pair(_store: &PeerStore, addr: SocketAddr) -> Result<PendingPair> {
    let node = NodeKey::load_or_mint(&default_config_dir())
        .context("peer pair: this node has no keypair to pair with")?;
    let instance = crate::peer::id::boot_instance_id();
    let deadline = std::time::Instant::now() + PAIR_WAIT;

    loop {
        let refusal = match dial_first_pairing(addr, node.secret_bytes(), &instance).await {
            Ok(session) => {
                return Ok(PendingPair {
                    peer: session.peer,
                    code: session.code,
                })
            }
            Err(err) => err,
        };
        if std::time::Instant::now() >= deadline {
            return Err(refusal.context(format!(
                "peer pair: {addr} did not accept a first pairing within {} seconds. That is \
                 what it looks like before somebody presses Accept over there: until then an \
                 `XX` message 1 gets zero bytes. Ask them to run `tcr peer pending` and \
                 `tcr peer accept {instance}`",
                PAIR_WAIT.as_secs()
            )));
        }
        tokio::time::sleep(PAIR_RETRY_INTERVAL).await;
    }
}

/// One `XX` attempt. Split out so [`pair`]'s retry loop reads as a loop and not
/// as a handshake.
async fn dial_first_pairing(
    addr: SocketAddr,
    secret: &[u8; KEY_BYTES],
    instance: &tcr_peer_wire::InstanceId,
) -> Result<noise::PeerSession> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("peer pair: could not reach {addr}"))?;
    noise::dial_handshake_with_payload(
        &mut stream,
        secret,
        Handshake::Pair,
        None,
        None,
        &noise::pair_message_1_payload(instance),
    )
    .await
    .context("peer pair: the first-pairing handshake was not answered")
}

/// An `XX` handshake waiting on a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPair {
    /// Who answered, as proved by the handshake, not as claimed by a beacon.
    pub peer: PeerId,
    /// The six digits to compare. Both sides must show the same string.
    pub code: String,
}

impl PendingPair {
    /// Whether the digits the operator read off the OTHER screen are this
    /// handshake's digits.
    ///
    /// A constant-time comparison is not the point here and would be theatre:
    /// the code is a channel binding both ends already hold, not a secret.
    /// What matters is that a mismatch is a refusal and never a retry prompt.
    pub fn matches(&self, code: &str) -> bool {
        self.code == code.trim()
    }
}

/// Confirm a compared code and write the pin, recording the address the
/// pairing ran over.
///
/// Refuses on a mismatch and says so plainly: a mismatch is the one signal this
/// path exists to produce, so it is never a retry prompt.
///
/// **The comparison is [`PendingPair::matches`], in the process that holds the
/// handshake.** A `snow::HandshakeState` cannot be persisted and the skeleton
/// has no session registry, so a second `tcr` invocation has nothing to
/// recompute the code from: this function writes the pin its caller has already
/// decided on, and the CLI does the compare while the session is still open.
/// Reported to the lead as a signature gap.
///
/// # Why `addr` is a parameter and not `None`
///
/// It WAS `None`, and that is the whole of the bug this parameter closes: a
/// Mac paired with six digits was pinned, trusted, and had no way back, so it
/// was filtered out of every candidate set that asks whether a row can be
/// reached. The address is what the handshake this code came out of actually
/// ran over, which is why it is safe to record and why only this caller can
/// supply it. `None` stays reachable for a caller that genuinely has no
/// address, a code compared out of band, rather than being faked.
pub fn confirm(
    store: &PeerStore,
    peer: &PeerId,
    code: &str,
    addr: Option<SocketAddr>,
) -> Result<()> {
    if code.trim().len() != 6 || !code.trim().chars().all(|c| c.is_ascii_digit()) {
        bail!("peer pair: {code:?} is not six digits, so it is not a pairing code");
    }
    pin_row(store, *peer, &format!("peer-{}", code.trim()), addr)?;
    Ok(())
}

/// Delete one pinned row. Immediate and per-peer for streams that TERMINATE at
/// this node: the next handshake from that peer fails the pin check and is
/// dropped before message 2. No CRL, no rotation for anyone else, no restart.
///
/// **It does not revoke egress reachable through a still-trusted relay**. See
/// [`crate::peer::config::Allow::relay`]. Every surface that offers this has to
/// say so when any peer still holds `relay`, because a revocation a reader
/// believes is mesh-wide and is not is worse than no revocation at all.
///
/// A LIVE session with that peer dies within one frame, not at the next
/// handshake: [`crate::peer::listener`] re-reads this file and re-checks the
/// row before every frame it serves.
///
/// **Locked**, like every other read-modify-write of this file. Unlocked, a
/// revocation was the write most easily lost: an inbound enrolment
/// ([`accept_enrolment`], which holds the lock) or a concurrent `tcr peer
/// allow` reads the file, and if it saves after this one, it saves a snapshot
/// taken while the forgotten row was still in it, so the operator is told
/// `peer forget: ok`, the row comes back, and the next handshake from that peer
/// passes the pin check. That is the one direction this verb must never fail
/// in.
pub fn forget(store: &PeerStore, peer: &PeerId) -> Result<bool> {
    let _lock = crate::peer::config::FileLock::acquire(store.path())?;
    let mut file = read_or_default(store.path())?;
    let before = file.peers.len();
    file.peers.retain(|row| row.node != *peer);
    let removed = file.peers.len() != before;
    if removed {
        save(store.path(), &file)?;
        // The row is gone from the file; the process may still hold this pair's
        // rendezvous secret, and a secret nobody cleared is a way of reaching a
        // Mac the operator just said to stop reaching. The row's own copy goes
        // with the row.
        let cleared = crate::peer::reach::forget_port_secret(peer);
        tracing::debug!(
            peer = %peer.display(),
            cleared,
            "peer forget: dropped this pair's rendezvous secret"
        );
    }
    Ok(removed)
}

/// Every peer that still holds `relay`, which is what `forget` does NOT
/// revoke. A surface that offers `forget` has to say this when the list is not
/// empty.
pub fn peers_still_holding_relay(file: &PeerFile) -> Vec<PeerId> {
    file.peers
        .iter()
        .filter(|row| row.allow.relay)
        .map(|row| row.node)
        .collect()
}

/// Write or replace one pinned row, leaving every grant at its default: a bare
/// pin can do nothing but say hello.
///
/// Locked for the reason [`forget`] gives, and safe to lock here because both
/// callers ([`confirm`] and the joiner's half of an enrolment) hold no lock:
/// the registrar's side writes its row inside [`accept_enrolment`]'s own lock
/// and never through this function.
fn pin_row(
    store: &PeerStore,
    peer: PeerId,
    label: &str,
    addr: Option<SocketAddr>,
) -> Result<PeerRow> {
    let label = sanitize_label(label).map_err(|refusal| anyhow!("peer pin: {refusal}"))?;
    let _lock = crate::peer::config::FileLock::acquire(store.path())?;
    let mut file = read_or_default(store.path())?;
    let endpoints = addr
        .map(|addr| vec![Endpoint::direct(addr, now_ms(), EndpointSource::Paired)])
        .unwrap_or_default();
    let row = PeerRow {
        node: peer,
        label,
        endpoints,
        added_at: now_ms(),
        allow: crate::peer::config::Allow::default(),
        lend: Vec::new(),
        // As above: pinning records who, and a session records where and on
        // what port.
        rendezvous_secret: None,
        sees_us_at: None,
    };
    // A re-pin of the same key replaces the row rather than adding a second
    // one: two rows for one key would make "is this peer pinned" answerable
    // two ways.
    file.peers.retain(|existing| existing.node != peer);
    file.peers.push(row.clone());
    save(store.path(), &file)?;
    Ok(row)
}

/// The label a joiner files its registrar under. The joiner has not been told
/// the registrar's name yet, a `Hello` carries it, and that is after the pin,
/// so the address it dialled is the honest placeholder.
fn registrar_label(addr: SocketAddr) -> Result<String> {
    sanitize_label(&format!("peer-{}", addr.port()))
        .map_err(|refusal| anyhow!("peer join: {refusal}"))
}

/// How long `tcr peer pair` is willing to accept a FIRST pairing, in seconds.
///
/// Two minutes is the shape of the act: an operator who just typed
/// `tcr peer pair` is standing at the other machine typing the same thing.
/// Every second of it is a second in which any host that can reach this port
/// can make this node disclose its static key in message 2, so the number is
/// short on purpose and is not configurable.
pub const PAIRING_WINDOW_SECS: i64 = 120;

/// Whether this node is currently willing to answer a first pairing under the
/// retired NODE-WIDE window (see the module doc comment for what replaced
/// it). Nothing in production reads this type today; [`pairing_window`],
/// [`open_pairing_window`] and [`close_pairing_window`] below are kept for
/// their own test coverage.
///
/// Originally a typed pair rather than a `bool` argument, because the
/// listener seam that took it alongside three other slices needed a caller
/// that passed the wrong `bool` to be impossible, not just unlikely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingWindow {
    /// An operator asked for a pairing within the last [`PAIRING_WINDOW_SECS`].
    Open,
    /// Nobody did. An `XX` message 1 gets zero bytes.
    Closed,
}

/// Open the pairing window, and report the deadline it now carries.
///
/// Called by `tcr peer pair` before it dials, so the two Macs an operator is
/// standing between are both willing while they are both being typed at.
///
/// The deadline is a TYPED FIELD of [`crate::peer::state::PeerState`], read and
/// written through that file's own loader and writer. A local document was
/// kept here with a flattened remainder, because an additive key had to
/// survive a read/modify/write; the
/// field is now where it belongs, and `PeerState` carries the flattened
/// remainder instead, so a key a NEWER build wrote still survives this write.
pub fn open_pairing_window(state_path: &Path, now_ms: i64) -> Result<i64> {
    let mut state = crate::peer::state::load(state_path, now_ms)?;
    let until = state.open_pairing_window(now_ms, PAIRING_WINDOW_SECS);
    crate::peer::state::save(state_path, &state).with_context(|| {
        format!(
            "peer pair: could not open a pairing window in {}",
            state_path.display()
        )
    })?;
    Ok(until)
}

/// Close the pairing window, so a completed or abandoned pairing does not leave
/// the remainder of its two minutes standing.
pub fn close_pairing_window(state_path: &Path) -> Result<()> {
    let mut state = crate::peer::state::load(state_path, now_ms())?;
    if !state.close_pairing_window() {
        return Ok(());
    }
    crate::peer::state::save(state_path, &state)
}

/// Whether a first pairing may be answered right now.
///
/// An unreadable state file is not a silent `Open`: [`crate::peer::state::load`]
/// renames a file it cannot trust aside and answers with an empty state, which
/// reads as [`PairingWindow::Closed`] here, the same silence a failed pin
/// check gets. The decision itself is
/// [`crate::peer::state::PeerState::pairing_window`], including the rule that a
/// clock jump in either direction closes the window.
pub fn pairing_window(state_path: &Path, now_ms: i64) -> Result<PairingWindow> {
    let state = crate::peer::state::load(state_path, now_ms)?;
    match state.pairing_window(now_ms) {
        crate::peer::state::PairingDeadline::Open { .. } => Ok(PairingWindow::Open),
        crate::peer::state::PairingDeadline::Closed => Ok(PairingWindow::Closed),
    }
}

/// A random request id for the one stream this file opens.
///
/// Random rather than counted, for the reason [`tcr_peer_wire::StreamHeader`]'s
/// `request_id` exists at all: it is the key a terminal dedups on, so two
/// enrolments from the same machine must not collide, and a counter would
/// restart at the same value on every process.
fn random_request_id() -> Result<u128> {
    let bytes = noise::random_secret()?;
    let mut head = [0_u8; 16];
    head.copy_from_slice(&bytes[..16]);
    Ok(u128::from_be_bytes(head))
}

/// A random invite id. Random rather than sequential so an id does not leak how
/// many invites this node has ever minted.
fn random_id() -> Result<u64> {
    let bytes = noise::random_secret()?;
    let mut head = [0_u8; 8];
    head.copy_from_slice(&bytes[..8]);
    Ok(u64::from_be_bytes(head))
}

/// Unix milliseconds.
///
/// Public because `tcr peer pair` opens the pairing window and the listener
/// compares that deadline: both have to read the same clock, and a second
/// spelling of "now" between the writer and the reader of one deadline is the
/// kind of drift that shows up as a window that is open on one side only.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// A 32-byte join secret is the same width as a static key, which is why both
/// are [`KEY_BYTES`].
const _: () = assert!(KEY_BYTES == 32);

// ---------------------------------------------------------------------------
// The share link: one string a person pastes into Slack or iMessage
// ---------------------------------------------------------------------------

/// The scheme and path every share link starts with.
///
/// `tcr://peer/join?…` rather than `tcr:join?…` because TcrBar registers this
/// as a URL scheme (`CFBundleURLTypes`) and macOS hands the whole URL to the
/// app: a host and a path leave room for a second verb later without a second
/// scheme, and `peer/join` says what the link does in the one place a person
/// reads before clicking.
pub const LINK_PREFIX: &str = "tcr://peer/join?";

/// The link version, checked and never guessed at, same rule as
/// [`TOKEN_PREFIX`].
pub const LINK_VERSION: u32 = 1;

/// A share link: a network key, and optionally a join key riding along.
///
/// The rule is: "one link a person can paste in Slack or iMessage that brings a
/// Mac onto the office mesh". Opening it sets the network key and, when a join
/// key rides along, enrols with the Mac that minted it. **Nothing else**, it
/// grants nothing, pins nothing by itself and turns no switch on.
///
/// # What each half really is, stated rather than softened
///
/// The `nk` half is a shared secret as durable as the key itself: anyone who
/// can read the message can join the beacon layer of that office mesh until the
/// key is rotated. It is an admission ticket, not identity, and it authorizes
/// nothing beyond being seen and heard, every pairing still needs the operator
/// to press Accept and compare six digits.
///
/// The `jk` half is a live bearer secret with one use and ten minutes on it
/// ([`INVITE_DEFAULT_TTL_SECS`]), exactly like the [`JoinToken`] it carries,
/// **and it enrols without a second screen**. A link with one in it is the
/// thing in this design closest to a password in a chat window, which is why
/// `tcr peer link` mints it only on an explicit `--invite`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareLink {
    /// The network key the receiving Mac should paste in.
    pub network_key: crate::peer::config::NetworkKey,
    /// The join key, when this link was minted with `--invite`.
    pub join: Option<JoinToken>,
}

impl ShareLink {
    /// Render the link.
    ///
    /// Percent-encoding is deliberately not applied and deliberately not
    /// needed: every value here is Crockford base32, a colon, a dot or a digit
    /// ([`JoinToken::to_token`]), and none of those is reserved in a query
    /// string. A hand-rolled encoder over an alphabet that cannot contain a
    /// reserved character would be a second thing to get wrong.
    pub fn to_link(&self) -> String {
        let mut out = format!(
            "{LINK_PREFIX}v={LINK_VERSION}&nk={}",
            self.network_key.to_paste_string()
        );
        if let Some(join) = &self.join {
            out.push_str("&jk=");
            out.push_str(&join.to_token());
        }
        out
    }

    /// Parse a pasted link. Refuses an unknown version rather than guessing.
    ///
    /// **Nothing here prints a field value.** Both halves are secrets and the
    /// place a person pastes one is a terminal that keeps scrollback, the same
    /// rule [`JoinToken::parse`] follows, for the same reason.
    pub fn parse(link: &str) -> Result<Self> {
        let link = link.trim();
        let Some(query) = link.strip_prefix(LINK_PREFIX) else {
            bail!(
                "peer link: this is not a share link (it must start with {LINK_PREFIX:?}); \
                 an unknown shape is refused rather than guessed at"
            );
        };

        // **Collected first, decoded second, and the version decided in
        // between.** Decoding a field as it is read makes the refusal depend on
        // the order the sender happened to write the query in: a v2 link whose
        // `nk` field this build cannot read was refused for the `nk` field
        // rather than for its version, which sends the operator to fix the one
        // thing that is not wrong.
        let mut version = None;
        let mut network_key_field = None;
        let mut join_field = None;
        for field in query.split('&') {
            let Some((key, value)) = field.split_once('=') else {
                bail!(
                    "peer link: {:?} is not a `key=value` field; the link is \
                     `{LINK_PREFIX}v=1&nk=<network key>[&jk=<join key>]`",
                    field.chars().take(8).collect::<String>()
                );
            };
            match key {
                "v" => version = Some(value.to_string()),
                "nk" => network_key_field = Some(value.to_string()),
                "jk" => join_field = Some(value.to_string()),
                // An unknown field is refused, not ignored. Everywhere else on
                // this wire an unknown value keeps the parse alive on purpose
                // (a mixed-build LAN), and this is the one place the opposite
                // is right: a link is a SECURITY decision a person takes in one
                // click, and silently dropping a field a newer build added
                // could mean acting on half of what the sender meant.
                other => bail!(
                    "peer link: {other:?} is a field this build does not know. A link is \
                     acted on in one click, so an unrecognised field is refused rather \
                     than dropped, update this Mac, or ask for a link without it"
                ),
            }
        }

        let Some(version) = version else {
            bail!("peer link: the link carries no `v=` version field")
        };
        if version != LINK_VERSION.to_string() {
            bail!(
                "peer link: {version:?} is not v{LINK_VERSION}, the only version this build \
                 reads; an unknown version is refused rather than guessed, because guessing \
                 would mean acting on a link this Mac does not understand"
            );
        }
        let Some(network_key) = network_key_field else {
            bail!(
                "peer link: the link carries no `nk=` network key, which is the one thing a \
                 share link is for"
            )
        };
        let network_key = crate::peer::config::NetworkKey::try_from(network_key)
            .map_err(|refusal| anyhow!("{refusal}"))
            .context("peer link: the nk field is not 32 base32-encoded bytes")?;
        let join = match join_field {
            Some(field) => Some(JoinToken::parse(&field)?),
            None => None,
        };
        Ok(Self { network_key, join })
    }
}

/// What `tcr peer join` was handed: a share link, or a bare join key.
///
/// The rule is: "`tcr peer join` (positional or `--stdin`) accepts a link or a
/// bare join key". Parsed once into this, so no later call site has to ask the
/// question again with a different test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinInput {
    /// A `tcr://peer/join?…` link.
    Link(ShareLink),
    /// A bare `tcr-join:v1:…` key, as `tcr peer invite` prints it.
    Key(JoinToken),
}

impl JoinInput {
    /// Decide which of the two this string is, by its own prefix.
    ///
    /// The prefixes are disjoint and both are checked, so an input that is
    /// neither gets a refusal naming both shapes rather than whichever error
    /// the first parser happened to produce.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.starts_with(LINK_PREFIX) {
            return ShareLink::parse(raw).map(Self::Link);
        }
        if raw.starts_with(TOKEN_PREFIX) {
            return JoinToken::parse(raw).map(Self::Key);
        }
        bail!(
            "peer join: this is neither a share link ({LINK_PREFIX}…) nor a join key \
             ({TOKEN_PREFIX}…). Nothing is printed back, because a paste that failed to \
             parse is still a live secret and a terminal keeps scrollback"
        )
    }

    /// The join key to enrol with, if this input carries one.
    pub fn join_token(&self) -> Option<&JoinToken> {
        match self {
            Self::Link(link) => link.join.as_ref(),
            Self::Key(token) => Some(token),
        }
    }

    /// The network key to set, if this input carries one.
    pub fn network_key(&self) -> Option<crate::peer::config::NetworkKey> {
        match self {
            Self::Link(link) => Some(link.network_key),
            Self::Key(_) => None,
        }
    }
}

/// Read a join input from wherever the operator put it.
///
/// The [`TokenSource::Stdin`] arm is the only one that keeps the secret out of
/// `ps` and the shell history, and it is what the panel's sheet and the
/// `tcr://` URL handler both use. **Nothing here logs or prints the input.**
pub fn read_join_input(source: TokenSource<'_>) -> Result<JoinInput> {
    match source {
        TokenSource::Argv(raw) => JoinInput::parse(raw),
        TokenSource::Stdin => join_input_from_reader(std::io::stdin().lock()),
    }
}

/// [`read_join_input`]'s stdin arm, against any reader, so a test can drive it
/// without a terminal.
pub fn join_input_from_reader<R: std::io::BufRead>(mut reader: R) -> Result<JoinInput> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("peer join: could not read the join key from standard input")?;
    if line.trim().is_empty() {
        bail!(
            "peer join: standard input carried no join key or share link (`--stdin` expects \
             one line, so the secret never enters this process's argv)"
        );
    }
    JoinInput::parse(&line)
}
