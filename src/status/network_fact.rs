//! Whether this Mac is on a network at all: the fact the Peers tab needs to
//! say "No network" instead of "Looking" when there is nothing to look on.
//!
//! # The rule
//!
//! At least one non-loopback interface is up with an address, IPv4 or IPv6,
//! that is not link-local-only. A private address (`192.168.0.0/16`,
//! `10.0.0.0/8`) or a unique-local IPv6 one (`fc00::/7`) counts as up: both
//! are addresses a real interface holds and mDNS can announce over, and the
//! panel's question is "is there a LAN to find peers on", not "is there a
//! route to the internet". A link-local-only address (`169.254.0.0/16`,
//! `fe80::/10`) does not count: it is the address a NIC gives itself before
//! DHCP answers, and a Mac holding only one of those is not usefully on a
//! network yet.
//!
//! # Why a route lookup, not an interface list
//!
//! The real answer is "walk every interface", which needs `getifaddrs`. That
//! is C and this crate forbids `unsafe` (`#![forbid(unsafe_code)]`,
//! `src/lib.rs:8`). The crate that wraps it, `if-addrs`, is already resolved
//! in `Cargo.lock` through `mdns-sd`, but pulling it into this crate directly
//! would still add a dependency EDGE this unit's brief asks to record rather
//! than take (see [`crate::peer::reach::global_v6_addresses`], which faced
//! the identical choice for the v6 address this Mac would send from).
//!
//! What stands in is the same trick that function and
//! [`crate::peer::reach::Client::internal_address`] already use: `connect` on
//! a UDP socket touches no network, it only runs the kernel's route lookup
//! and records a destination, and the source address the kernel picks back is
//! the address a real interface would send from. No route at all (no
//! interface up) leaves the socket at its unspecified bind address, and that
//! is exactly the case this rule exists to catch.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// Destinations used only for their route lookup, one per family. Both are
/// off-link documentation addresses (RFC 5737 for v4, RFC 3849 for v6) on
/// purpose: nothing is ever sent to them, and a reserved prefix can never be
/// the answer this rule is trying to detect.
const PROBES: [&str; 2] = ["192.0.2.1:9", "[2001:db8::1]:9"];

/// Is `addr` the kind of address a real, up, non-loopback interface holds?
/// The one rule [`network_present`] runs, named so it can be driven directly
/// from a hand-built list in a test.
pub fn is_usable(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_usable_v4(v4),
        IpAddr::V6(v6) => is_usable_v6(v6),
    }
}

fn is_usable_v4(addr: Ipv4Addr) -> bool {
    if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() {
        return false;
    }
    // 169.254.0.0/16 link-local: a NIC's self-assigned address before DHCP
    // answers, not a network this Mac has actually joined.
    let octets = addr.octets();
    if octets[0] == 169 && octets[1] == 254 {
        return false;
    }
    true
}

fn is_usable_v6(addr: Ipv6Addr) -> bool {
    if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() {
        return false;
    }
    // fe80::/10 link-local, the same self-assigned case as v4's 169.254/16.
    if addr.segments()[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    true
}

/// The rule over a whole list at once: does any address in `addrs` count as
/// this Mac being on a network? Kept apart from the live probe below so a
/// test can hand it addresses directly instead of depending on this
/// machine's real interfaces.
pub fn any_usable(addrs: &[IpAddr]) -> bool {
    addrs.iter().copied().any(is_usable)
}

/// The address the kernel would send `destination` from, or `None` when
/// there is no route to try (no interface up at all, or the bind itself
/// failed).
fn route_source(destination: SocketAddr) -> Option<IpAddr> {
    let bind = match destination {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(destination).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Every address the kernel's route lookup reveals right now, one probe per
/// family. A machine with no interface up returns an empty list.
fn route_addresses() -> Vec<IpAddr> {
    PROBES
        .iter()
        .filter_map(|probe| probe.parse::<SocketAddr>().ok())
        .filter_map(route_source)
        .collect()
}

/// Is this Mac on a network at all: does it have at least one non-loopback
/// interface up with a usable address? See the module doc comment for the
/// rule and why a route lookup stands in for an interface list.
pub fn network_present() -> bool {
    any_usable(&route_addresses())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_does_not_count() {
        assert!(!is_usable(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_usable(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn unspecified_does_not_count() {
        assert!(!is_usable(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_usable(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn link_local_only_does_not_count() {
        assert!(!is_usable("169.254.1.1".parse().expect("a v4 address")));
        assert!(!is_usable("fe80::1".parse().expect("a v6 address")));
    }

    #[test]
    fn a_private_lan_address_counts() {
        assert!(is_usable("192.168.1.20".parse().expect("a v4 address")));
        assert!(is_usable("10.0.0.5".parse().expect("a v4 address")));
    }

    #[test]
    fn a_unique_local_v6_address_counts() {
        assert!(is_usable("fd12:3456::1".parse().expect("a v6 address")));
    }

    #[test]
    fn a_global_address_counts() {
        assert!(is_usable("203.0.113.9".parse().expect("a v4 address")));
    }

    #[test]
    fn no_addresses_at_all_is_no_network() {
        assert!(!any_usable(&[]));
    }

    #[test]
    fn only_link_local_addresses_is_no_network() {
        let addrs = [
            "169.254.1.1".parse().expect("a v4 address"),
            "fe80::1".parse().expect("a v6 address"),
        ];
        assert!(!any_usable(&addrs));
    }

    #[test]
    fn one_usable_address_among_link_local_ones_is_a_network() {
        let addrs = [
            "fe80::1".parse().expect("a v6 address"),
            "192.168.1.20".parse().expect("a v4 address"),
        ];
        assert!(any_usable(&addrs));
    }
}
