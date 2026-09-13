//! Hand a bound listening socket to a successor process, so a restart never
//! unbinds the port.
//!
//! A normal bounce closes the listener and the successor binds a fresh one.
//! Between those two moments nothing is listening and every new connection is
//! refused. Measured 2026-09-13 that window was about a second per restart; see
//! `docs/design/zero-downtime-restart.md` for the numbers and the wider design.
//!
//! Passing the *same* socket closes the window entirely rather than shrinking
//! it. The successor receives a duplicate of the predecessor's listening file
//! descriptor, so the socket's refcount never reaches zero, the port is never
//! unbound, and a connection arriving mid-swap waits in the kernel's accept
//! queue instead of being refused.
//!
//! # What this module does NOT do
//!
//! It moves a file descriptor and nothing else. It does not decide *when* a
//! handoff is allowed, and in particular it does not release mutation
//! ownership — that is `Manager::release_mutation_ownership` in the main crate,
//! and the ordering between the two is the part of the design that can cost
//! accounts if it is wrong. Keeping the transport ignorant of the policy is
//! deliberate: this crate is testable in isolation precisely because it knows
//! none of it.
//!
//! # Why this is a separate crate
//!
//! Adopting a descriptor the kernel just handed you means turning an integer
//! into an owning handle, and there is no safe way to express that: every route
//! bottoms out in `FromRawFd`, which is `unsafe` by construction. The main
//! crate is `#![forbid(unsafe_code)]` (`src/lib.rs`), and `forbid` cannot be
//! lifted by an inner `allow` — that is the entire difference between `forbid`
//! and `deny`, and the reason to reach for it in the first place.
//!
//! Rather than downgrade a crate-wide invariant for one function, the unsafe
//! lives here, in a crate small enough to read in one sitting. The proxy keeps
//! its guarantee intact and the audit surface is this file.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};

/// The single payload byte that rides along with the descriptor.
///
/// **Not decoration.** `SCM_RIGHTS` is ancillary data attached to a normal
/// message, and a `sendmsg` carrying no ordinary data at all is permitted to
/// transfer nothing: the receiver can see a zero-length read and no control
/// message. One byte guarantees there is a message for the descriptor to ride
/// on. Its VALUE is also checked on the far side, so a stray byte from anything
/// else on the socket cannot be mistaken for a handoff.
const HANDOFF_BYTE: u8 = 0xF0;

/// Send `listener`'s file descriptor over an already-connected `stream`.
///
/// The caller keeps its own listener open and usable. `SCM_RIGHTS` duplicates
/// the descriptor into the receiving process rather than moving it, so both
/// sides hold a reference until each drops its own — which is exactly the
/// property that keeps the port bound across the swap.
pub fn send_listener(stream: &UnixStream, listener: &std::net::TcpListener) -> io::Result<()> {
    let fds = [listener.as_raw_fd()];
    let cmsgs = [ControlMessage::ScmRights(&fds)];
    let payload = [HANDOFF_BYTE];
    let iov = [io::IoSlice::new(&payload)];
    sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .map_err(io::Error::from)?;
    Ok(())
}

/// Receive a listening socket sent by [`send_listener`].
///
/// The returned listener is a real `std::net::TcpListener` owning the received
/// descriptor. Converting it for tokio needs `set_nonblocking(true)` first, as
/// with any adopted socket; this function deliberately does not do it, so the
/// caller states which runtime it is adopting into.
pub fn recv_listener(stream: &UnixStream) -> io::Result<std::net::TcpListener> {
    let mut byte = [0u8; 1];
    let mut space = nix::cmsg_space!([RawFd; 1]);

    // Everything the message borrows is confined to this block, so `byte` is
    // readable afterwards. Descriptors are wrapped in `OwnedFd` the moment they
    // are seen, which is what makes every early return below leak-free: a
    // rejected handoff closes what it was sent instead of stranding it in this
    // process for the rest of its life.
    let (bytes, mut fds) = {
        let mut iov = [io::IoSliceMut::new(&mut byte)];
        let msg = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut space),
            MsgFlags::empty(),
        )
        .map_err(io::Error::from)?;

        let mut fds: Vec<OwnedFd> = Vec::new();
        for cmsg in msg.cmsgs().map_err(io::Error::from)? {
            if let ControlMessageOwned::ScmRights(raw) = cmsg {
                for fd in raw {
                    // SAFETY: each fd came out of an SCM_RIGHTS control message,
                    // so the kernel installed it in this process and nothing else
                    // owns it. Taking ownership here is what guarantees it is
                    // closed on every path out of this function.
                    fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
        (msg.bytes, fds)
    };

    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "handoff peer closed the socket without sending a listener",
        ));
    }
    if byte[0] != HANDOFF_BYTE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff payload byte was {:#04x}, expected {HANDOFF_BYTE:#04x}",
                byte[0]
            ),
        ));
    }
    if fds.len() != 1 {
        // More than one is a protocol mismatch, not something to paper over by
        // taking the first; none means the control message never arrived.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff carried {} descriptors, expected exactly 1",
                fds.len()
            ),
        ));
    }
    let Some(fd) = fds.pop() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff message carried no SCM_RIGHTS descriptor",
        ));
    };
    Ok(std::net::TcpListener::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// The property the whole crate exists for: the received descriptor is a
    /// working listener on the same port.
    ///
    /// The original is dropped BEFORE accepting, deliberately. With both ends
    /// open this test would pass even if `recv_listener` returned something
    /// useless, because the sender's listener would still be holding the port.
    /// Dropping it first means the received descriptor is the only thing
    /// keeping the socket bound, which is exactly the claim being made: the
    /// port never unbinds across a handoff.
    #[test]
    fn a_received_listener_still_serves_after_the_sender_drops_its_own() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
        let addr = listener.local_addr().expect("reading the bound address");
        let (send_side, recv_side) = UnixStream::pair().expect("socketpair");

        send_listener(&send_side, &listener).expect("sending the listener");
        let received = recv_listener(&recv_side).expect("receiving the listener");
        assert_eq!(
            received.local_addr().expect("reading the received address"),
            addr,
            "the received descriptor names a different port than the one sent"
        );

        drop(listener);

        let client = std::thread::spawn(move || std::net::TcpStream::connect(addr));
        let (_conn, _peer) = received
            .accept()
            .expect("the received listener must accept a connection after the original is gone");
        client
            .join()
            .expect("client thread")
            .expect("connecting to the handed-over port");
    }

    /// A plain byte with no ancillary data must be refused rather than taken as
    /// a successful handoff of nothing.
    #[test]
    fn a_message_carrying_no_descriptor_is_refused() {
        let (send_side, recv_side) = UnixStream::pair().expect("socketpair");
        (&send_side)
            .write_all(&[HANDOFF_BYTE])
            .expect("writing a bare payload byte");

        let err =
            recv_listener(&recv_side).expect_err("a descriptor-less message must not succeed");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "got: {err}");
    }

    /// A peer that closes without sending is reported as EOF, not as an empty
    /// success. A failed handoff has to be distinguishable from a completed
    /// one, because the caller's fallback depends on telling them apart.
    #[test]
    fn a_closed_peer_is_reported_as_eof() {
        let (send_side, recv_side) = UnixStream::pair().expect("socketpair");
        drop(send_side);

        let err = recv_listener(&recv_side).expect_err("a closed peer must not succeed");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "got: {err}");
    }

    /// The payload byte is checked, not just counted, so traffic from anything
    /// else on the socket cannot be mistaken for a handoff.
    #[test]
    fn a_wrong_payload_byte_is_refused() {
        let (send_side, recv_side) = UnixStream::pair().expect("socketpair");
        (&send_side)
            .write_all(&[HANDOFF_BYTE ^ 0xFF])
            .expect("writing a foreign byte");

        let err = recv_listener(&recv_side).expect_err("a foreign byte must not succeed");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "got: {err}");
    }
}
