//! This node's identity: one Noise static X25519 keypair, on disk, minted once.
//!
//! **Node identity is the keypair and nothing else.** There is no registry, no
//! serial, no name that carries authority: a peer's authorization is the public
//! half of this pair appearing in its own `tcr-peers.json`, and revoking
//! it is one deleted line there.
//!
//! # Never re-minted automatically
//!
//! A re-mint evicts every peer that pinned this node, their next handshake
//! fails the pin check, correctly, because the key really did change. So
//! [`NodeKey::load_or_mint`] mints on FIRST use only, and a re-mint is an
//! explicit operator act with its own confirmation, never a repair a boot
//! sequence performs on its own.
//!
//! # A separate file pair from the MITM CA, deliberately
//!
//! The CA lives at its own paths (`src/mitm.rs:221-228`) and regenerating it
//! invalidates every leaf signed by the boot-time CA, which breaks `claude`
//! processes running right now, a recorded incident. Keeping the peer keypair
//! in its own files means a peer rotation can never invalidate a leaf a running
//! `claude` depends on, and the reverse.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tcr_peer_wire::PeerId;

/// The mode both key files are created with. The private half is readable by
/// this user and nobody else; a wider mode is a refusal, not a warning,
/// because the file IS the identity.
pub const PRIVATE_KEY_MODE: u32 = 0o600;

/// The public half is world-readable on purpose: it is what an operator pastes
/// into an invite and reads off a panel row.
pub const PUBLIC_KEY_MODE: u32 = 0o644;

/// This node's static keypair.
///
/// The secret never leaves this type and never leaves this machine. It is
/// handed to `snow`'s builder ([`NodeKey::secret_bytes`]) and to nothing else:
/// no log line, no `tcr status --json`, no `Hello`, no fixture.
pub struct NodeKey {
    secret: [u8; 32],
    public: [u8; 32],
}

impl NodeKey {
    /// Read the keypair from `config_dir`, minting it on first use.
    ///
    /// Refuses rather than repairs when the private key exists with a mode
    /// wider than [`PRIVATE_KEY_MODE`], or when the public half disagrees with
    /// the private one: both mean something other than this program wrote the
    /// files, and silently re-deriving would erase the evidence.
    pub fn load_or_mint(config_dir: &Path) -> Result<Self> {
        let priv_path = private_key_path(config_dir);
        let pub_path = public_key_path(config_dir);

        if priv_path.exists() {
            let mode = fs::metadata(&priv_path)
                .with_context(|| format!("reading permissions of {}", priv_path.display()))?
                .permissions()
                .mode()
                & 0o777;
            if mode != PRIVATE_KEY_MODE {
                anyhow::bail!(
                    "{} has mode {mode:o}, expected {PRIVATE_KEY_MODE:o}: refusing to trust a \
                     key file this program did not write",
                    priv_path.display()
                );
            }

            let secret_raw =
                fs::read(&priv_path).with_context(|| format!("reading {}", priv_path.display()))?;
            let secret: [u8; 32] = secret_raw.try_into().map_err(|raw: Vec<u8>| {
                anyhow::anyhow!(
                    "{} holds {} bytes, expected exactly 32",
                    priv_path.display(),
                    raw.len()
                )
            })?;

            let derived_public = public_from_secret(&secret)?;

            let public = if pub_path.exists() {
                let public_raw = fs::read(&pub_path)
                    .with_context(|| format!("reading {}", pub_path.display()))?;
                if public_raw != derived_public {
                    anyhow::bail!(
                        "{} disagrees with the key derived from {}: something other than this \
                         program wrote these files",
                        pub_path.display(),
                        priv_path.display()
                    );
                }
                derived_public
            } else {
                // The public half is regenerable from the private one and is not
                // the identity, so a missing (but not disagreeing) public file
                // is repaired rather than refused.
                write_key_file(&pub_path, &derived_public, PUBLIC_KEY_MODE)?;
                derived_public
            };

            return Ok(Self { secret, public });
        }

        // First use: mint once.
        fs::create_dir_all(config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let (secret, public) = generate_keypair()?;
        write_key_file(&priv_path, &secret, PRIVATE_KEY_MODE)?;
        write_key_file(&pub_path, &public, PUBLIC_KEY_MODE)?;

        Ok(Self { secret, public })
    }

    /// This node's own id, the public half, as every peer sees it.
    pub fn id(&self) -> PeerId {
        PeerId(self.public)
    }

    /// The private static, for `snow`'s builder and no other caller.
    pub fn secret_bytes(&self) -> &[u8; 32] {
        &self.secret
    }
}

/// `<config_dir>/tcr-node.key`, the private static, mode
/// [`PRIVATE_KEY_MODE`].
pub fn private_key_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tcr-node.key")
}

/// `<config_dir>/tcr-node.pub`, the public static, mode [`PUBLIC_KEY_MODE`].
pub fn public_key_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tcr-node.pub")
}

/// Where both files live by default: the same directory the drop-in config
/// already uses, so one `--config`-shaped override reaches all of it and a test
/// points the whole peer surface at a temp dir with one argument.
pub fn default_config_dir() -> PathBuf {
    crate::config::default_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// This process's instance id: eight random bytes, minted once at first use
/// and held for the life of the process.
///
/// # Per BOOT, and that is the whole design
///
/// An announcement carries "a per-boot random 8-byte instance id,
/// the port, the wire version" and never a node id or a key. So this value is
/// deliberately NOT persisted anywhere, it is not in `tcr-peers.json`, it is
/// not in `peer-state.json`, and a restart mints a new one. That is the
/// property that makes an announcement ephemeral: a stable id in a beacon would
/// be a name for this machine that a passive listener on the LAN could follow
/// across days, which is exactly what [`PeerId`] is kept out of the beacon to
/// prevent.
///
/// It is also why it lives in a `OnceLock` here rather than on
/// [`crate::peer::config::PeerFile`]: a field on a file is a field that gets
/// written to the file.
///
/// One value per process, so the beacon this node announces, the knock it
/// sends and the `XX` message 1 it follows up with all name the same thing,
/// which is what lets the far side's Accept open a window this node's dial can
/// actually use.
pub fn boot_instance_id() -> tcr_peer_wire::InstanceId {
    static INSTANCE: std::sync::OnceLock<tcr_peer_wire::InstanceId> = std::sync::OnceLock::new();
    *INSTANCE.get_or_init(|| {
        // The same CSPRNG every other random value in this module comes from.
        // A failure here is not recoverable into something weaker: an instance
        // id that fell back to a counter or a clock would be a stable name for
        // this machine, which is the one thing it must not be, so the fallback
        // is a fresh keypair's public half, which is random by construction and
        // discarded immediately.
        let mut bytes = [0_u8; tcr_peer_wire::INSTANCE_ID_BYTES];
        let primary = match getrandom::fill(&mut bytes) {
            Ok(()) => Some(bytes),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer instance id: the CSPRNG failed; falling back to a discarded \
                     keypair's public half, which is random by construction"
                );
                None
            }
        };
        let fallback = || generate_keypair().ok().map(|(_secret, public)| public);
        // PANICS on a double failure, and that is the whole of this change.
        // The array was left as it was found, all zeroes, and became this
        // process's stable instance id: every node in the same state announces
        // the same one, so their beacons, knocks and message 1 payloads all
        // name one machine and the windows they open for each other land on the
        // wrong dial. A machine whose CSPRNG has failed twice cannot complete a
        // Noise handshake either, both patterns need the same randomness, so
        // there is nothing this process could go on to do that would work.
        mint_instance_id(primary, fallback()).expect(
            "peer instance id: this Mac's randomness failed twice, so no peer handshake on \
             it can succeed either",
        )
    })
}

/// The instance id two random sources produced, or `None` when neither
/// answered.
///
/// Split out from [`boot_instance_id`] because neither source can be made to
/// fail on a working machine, and the branch that matters is the one where both
/// did: an id of all zeroes is the same name on every machine in that state,
/// and it is minted once and held for the life of the process.
fn mint_instance_id(
    primary: Option<[u8; tcr_peer_wire::INSTANCE_ID_BYTES]>,
    fallback: Option<[u8; 32]>,
) -> Option<tcr_peer_wire::InstanceId> {
    if let Some(bytes) = primary {
        return Some(tcr_peer_wire::InstanceId(bytes));
    }
    let public = fallback?;
    let mut bytes = [0_u8; tcr_peer_wire::INSTANCE_ID_BYTES];
    bytes.copy_from_slice(&public[..tcr_peer_wire::INSTANCE_ID_BYTES]);
    Some(tcr_peer_wire::InstanceId(bytes))
}

/// The public half of an X25519 static key, derived from its secret.
fn public_from_secret(secret: &[u8; 32]) -> Result<[u8; 32]> {
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or_else(|| anyhow::anyhow!("no curve25519 DH implementation available"))?;
    dh.set(secret);
    dh.pubkey()
        .try_into()
        .map_err(|_| anyhow::anyhow!("unexpected public key length"))
}

/// Write a 32-byte key file at `mode`, refusing to follow a pre-existing
/// symlink the way [`crate::config::write_atomic`] does for the main config.
///
/// `create_new` (`O_EXCL`) is what makes that sentence true, and it became
/// true late: the previous `create(true)` FOLLOWED a symlink on
/// a fully predictable path, so anything able to create a file in the config
/// dir could plant `tcr-node.key` as a link and have this node write its
/// private static key wherever it chose. (`load_or_mint` reaches this function
/// only when the path does not exist, so refusing an existing one costs
/// nothing.) `open(2)` applies `mode` only on creation, which is the second
/// thing `O_EXCL` buys: without a guaranteed fresh inode, the 0600 below is
/// silently ignored for a pre-existing file.
fn write_key_file(path: &Path, bytes: &[u8; 32], mode: u32) -> Result<()> {
    // STAGED beside the destination, then linked into place, because the path
    // appearing is what `load_or_mint` treats as "this node already has a key".
    // Creating the file and then writing it is two steps, and a crash, a full
    // disk or a SIGKILL between them leaves a 0-byte `tcr-node.key`: a file
    // `load_or_mint` refuses for the rest of that machine's life ("holds 0
    // bytes, expected exactly 32") and that nothing will ever overwrite,
    // because minting refuses an existing path by design. This node then has no
    // identity and no way to mint one until an operator deletes a file nobody
    // told them about.
    //
    // In the destination's own directory, never the system temp dir: `link(2)`
    // across filesystems fails with `EXDEV`, which would cost exactly the
    // atomicity this exists for.
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = match path.file_name() {
        Some(name) => format!(".{}.tcr-", name.to_string_lossy()),
        None => ".tcr-".to_string(),
    };
    let mut staged = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(mode))
        .prefix(prefix.as_str())
        .suffix(".tmp")
        .tempfile_in(dir)
        .with_context(|| format!("staging {}", path.display()))?;
    staged
        .write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    // Durable BEFORE it is published: a rename or a link that outlives its own
    // data is the same 0-byte file by a slower route.
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("flushing {}", path.display()))?;

    // `link(2)` and never `rename(2)`. A rename REPLACES the destination, and
    // this writer must refuse one that already exists: that refusal is the
    // symlink defence (`create_new`'s `O_EXCL` used to be what made it true)
    // and it is what stops a second mint overwriting the identity this Mac is
    // already pinned by on every other Mac. `link` fails with `AlreadyExists`
    // for a regular file and for a planted symlink alike, and it does not
    // follow one.
    //
    // The staged copy is dropped either way: on success the destination holds
    // the same inode and one link is enough, on failure an orphan beside a key
    // file is a file holding this node's private key.
    let staged = staged.into_temp_path();
    std::fs::hard_link(&staged, path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Key generation, deliberately not through `snow::Builder`
// ---------------------------------------------------------------------------
//
// `src/peer/noise.rs` documents itself as the only file that may name
// `snow::` for the HANDSHAKE. Key GENERATION is a different concern, no
// pattern, no session, and the root `Cargo.toml` is not changed here, so no
// independent x25519 crate can be added.
// The two imports below, `snow::params::DHChoice` and
// `snow::resolvers::{CryptoResolver, DefaultResolver}`, are the whole of the
// exemption, and both are public API already reachable through the `snow`
// dependency this tree already pins. `snow::types::Dh` is not among them: the DH
// object arrives as the `Box<dyn Dh>` that `resolve_dh` returns, so the trait
// is used without being named. This calls only the raw DH primitive (generate
// / set / pubkey), never a `HandshakeState` or a Noise pattern.

use snow::params::DHChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};

/// A fresh random X25519 keypair: `(secret, public)`.
fn generate_keypair() -> Result<([u8; 32], [u8; 32])> {
    let mut rng = DefaultResolver
        .resolve_rng()
        .ok_or_else(|| anyhow::anyhow!("no RNG implementation available"))?;
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or_else(|| anyhow::anyhow!("no curve25519 DH implementation available"))?;
    dh.generate(&mut *rng)
        .context("generating the node's X25519 keypair")?;
    let secret: [u8; 32] = dh
        .privkey()
        .try_into()
        .map_err(|_| anyhow::anyhow!("unexpected private key length"))?;
    let public: [u8; 32] = dh
        .pubkey()
        .try_into()
        .map_err(|_| anyhow::anyhow!("unexpected public key length"))?;
    Ok((secret, public))
}

#[cfg(test)]
mod instance_id_tests {
    use super::mint_instance_id;

    /// **Two failed random sources produce no instance id at all.**
    ///
    /// The second failure used to leave the array as it was found, all zeroes,
    /// and that value became this process's stable instance id: every machine
    /// in the same state announces the same one.
    ///
    /// Watch it fail by returning `Some(InstanceId([0; _]))` from the last
    /// branch instead of `None`.
    #[test]
    fn a_double_failure_is_not_an_id_of_zeroes() {
        assert!(
            mint_instance_id(None, None).is_none(),
            "a machine whose randomness failed twice has no instance id to announce"
        );

        // The two ways there IS one, so the refusal above is the double failure
        // and not a function that refuses everything.
        assert_eq!(
            mint_instance_id(Some([3_u8; tcr_peer_wire::INSTANCE_ID_BYTES]), None)
                .expect("the CSPRNG answered")
                .0,
            [3_u8; tcr_peer_wire::INSTANCE_ID_BYTES],
        );
        assert_eq!(
            mint_instance_id(None, Some([5_u8; 32]))
                .expect("the fallback keypair answered")
                .0,
            [5_u8; tcr_peer_wire::INSTANCE_ID_BYTES],
        );
    }
}
