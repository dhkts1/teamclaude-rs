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
//! # Why a real interface list, not a route lookup
//!
//! An earlier version of this module asked the kernel's route lookup instead:
//! `connect` a UDP socket to an off-link documentation address and read back
//! the source address it picked, the same trick
//! [`crate::peer::reach::global_v6_addresses`] and
//! [`crate::peer::reach::Client::internal_address`] use. That answers "is
//! there a route to the internet", not "is there a LAN to look on": a Mac
//! with an interface up and addressed but no default gateway (a hotspot with
//! no uplink, an isolated office LAN, two Macs on a link with no router) has
//! no route to an off-link address at all, and `connect` fails with
//! `ENETUNREACH` (confirmed on this machine with `SO_DONTROUTE`, which
//! restricts a socket to directly-connected networks the same way a missing
//! gateway does: the documentation-range probe failed with "Network is
//! unreachable" while a plain probe on the same machine succeeded). That Mac
//! still finds peers over mDNS on the LAN it has, so the route trick read a
//! real "yes" as "no".
//!
//! Walking every interface directly needs `getifaddrs`, which is C and this
//! crate forbids `unsafe` (`#![forbid(unsafe_code)]`, `src/lib.rs:8`).
//! `if-addrs` wraps it and is already resolved in `Cargo.lock` through
//! `mdns-sd`, so taking it as a direct dependency here adds one dependency
//! EDGE and no new package (`Cargo.toml`, next to `mdns-sd`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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

/// Every address a real, operationally-up interface holds right now, as
/// `if-addrs` reports it. Loopback is excluded here rather than left to
/// [`is_usable`], because [`if_addrs::Interface::is_oper_up`] is a fact about
/// the INTERFACE and [`is_usable`] is a rule about the ADDRESS; keeping them
/// apart is what lets a test hand [`any_usable`] addresses directly without
/// building a fake interface. An interface whose operational status this
/// platform cannot report reads as down rather than up, so an unreadable
/// state can only ever make this Mac look OFFLINE, never falsely online.
fn interface_addresses() -> Vec<IpAddr> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    interfaces
        .into_iter()
        .filter(|interface| interface.is_oper_up() && !interface.is_loopback())
        .map(|interface| interface.ip())
        .collect()
}

/// Is this Mac on a network at all: does it have at least one non-loopback
/// interface up with a usable address? See the module doc comment for the
/// rule and why a real interface list is what answers it.
pub fn network_present() -> bool {
    any_usable(&interface_addresses())
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
