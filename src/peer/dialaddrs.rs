//! One codec for a list of dial addresses, used nowhere else.
//!
//! `JoinToken::to_token` spells a list of addresses as comma-separated text
//! (`src/peer/pair.rs`) and `MovedRecord.eps` spells the same shape as JSON
//! (`src/peer/moved.rs`). A v3 key and a sealed reply both need a third,
//! compact spelling, and writing that twice is the one shape that always
//! drifts. So it lives here once: a count byte, then per address one family
//! byte, the address bytes, and two port bytes big-endian.
//!
//! This module knows nothing about keys, seals or versions, and nothing about
//! [`crate::peer::pair::DialAddressKind`]: a decoded address never carries a
//! kind, the same honest answer [`crate::peer::pair::DialAddress::from_key`]
//! already gives for one read out of a pasted key.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::peer::drop::MAX_RECORD_ENDPOINTS;

/// The IPv4 family byte on the wire.
const FAMILY_V4: u8 = 4;

/// The IPv6 family byte on the wire.
const FAMILY_V6: u8 = 6;

/// Encode a list of addresses, best first, into the compact form.
///
/// The caller is responsible for the list already fitting
/// [`MAX_RECORD_ENDPOINTS`]; this function does not truncate, because
/// silently dropping an address the caller asked to include would be a
/// different bug from the one [`decode`] refuses.
pub fn encode(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + addrs.len() * 19);
    out.push(addrs.len() as u8);
    for addr in addrs {
        match addr.ip() {
            IpAddr::V4(v4) => {
                out.push(FAMILY_V4);
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.push(FAMILY_V6);
                out.extend_from_slice(&v6.octets());
            }
        }
        out.extend_from_slice(&addr.port().to_be_bytes());
    }
    out
}

/// Why a byte run could not be read back as a list of addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeRefusal {
    /// The count byte named more addresses than [`MAX_RECORD_ENDPOINTS`]
    /// allows. Checked before anything is reserved, so a stranger's count byte
    /// never drives an allocation.
    TooMany { found: usize },
    /// The bytes ran out mid-address: a chat client's truncation, not a
    /// forgery.
    CutShort,
    /// A family byte this build does not know. Refused rather than skipped:
    /// half of what the sender meant is worse than none of it, the rule
    /// [`crate::peer::moved`]'s `MovedRefusal::UnknownField` states.
    UnknownFamily { byte: u8 },
}

impl std::fmt::Display for DecodeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany { found } => write!(
                f,
                "this address list names {found} addresses, more than the {MAX_RECORD_ENDPOINTS} \
                 this build will read"
            ),
            Self::CutShort => write!(
                f,
                "this address list ends mid-address, which is what a paste cut short looks like"
            ),
            Self::UnknownFamily { byte } => {
                write!(
                    f,
                    "this address list names an address family this build does not know ({byte})"
                )
            }
        }
    }
}

impl std::error::Error for DecodeRefusal {}

/// The inverse of [`encode`]: the same list, in the same order, or a refusal
/// naming what went wrong.
pub fn decode(bytes: &[u8]) -> Result<Vec<SocketAddr>, DecodeRefusal> {
    let (addrs, rest) = decode_prefix(bytes)?;
    if !rest.is_empty() {
        return Err(DecodeRefusal::CutShort);
    }
    Ok(addrs)
}

/// [`decode`], but for a caller that has more bytes after the list (a v3 key's
/// registrar and secret, a sealed reply's own one-time public key): the list
/// is self-delimiting on its own count byte, so this returns the list and
/// whatever bytes came after it, unconsumed.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Vec<SocketAddr>, &[u8]), DecodeRefusal> {
    let (&count_byte, mut rest) = bytes.split_first().ok_or(DecodeRefusal::CutShort)?;
    let count = count_byte as usize;
    if count > MAX_RECORD_ENDPOINTS {
        return Err(DecodeRefusal::TooMany { found: count });
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let (&family, after_family) = rest.split_first().ok_or(DecodeRefusal::CutShort)?;
        let (ip, after_ip): (IpAddr, &[u8]) = match family {
            FAMILY_V4 => {
                if after_family.len() < 4 {
                    return Err(DecodeRefusal::CutShort);
                }
                let (octets, tail) = after_family.split_at(4);
                let octets: [u8; 4] = octets.try_into().expect("checked length");
                (IpAddr::V4(Ipv4Addr::from(octets)), tail)
            }
            FAMILY_V6 => {
                if after_family.len() < 16 {
                    return Err(DecodeRefusal::CutShort);
                }
                let (octets, tail) = after_family.split_at(16);
                let octets: [u8; 16] = octets.try_into().expect("checked length");
                (IpAddr::V6(Ipv6Addr::from(octets)), tail)
            }
            byte => return Err(DecodeRefusal::UnknownFamily { byte }),
        };
        if after_ip.len() < 2 {
            return Err(DecodeRefusal::CutShort);
        }
        let (port_bytes, tail) = after_ip.split_at(2);
        let port = u16::from_be_bytes([port_bytes[0], port_bytes[1]]);
        out.push(SocketAddr::new(ip, port));
        rest = tail;
    }
    Ok((out, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_address_family() {
        let addrs: Vec<SocketAddr> = vec![
            "192.0.2.10:7755".parse().expect("a test address"),
            "[2001:db8::1]:7755".parse().expect("a test address"),
            "198.51.100.20:1".parse().expect("a test address"),
        ];
        let encoded = encode(&addrs);
        assert_eq!(decode(&encoded).expect("it decodes"), addrs);
    }

    #[test]
    fn refuses_a_count_over_the_ceiling() {
        let mut bytes = vec![(MAX_RECORD_ENDPOINTS + 1) as u8];
        bytes.extend(std::iter::repeat_n(0, 64));
        assert_eq!(
            decode(&bytes),
            Err(DecodeRefusal::TooMany {
                found: MAX_RECORD_ENDPOINTS + 1
            })
        );
    }

    #[test]
    fn refuses_a_truncated_run_as_cut_short_not_a_forgery() {
        let addrs: Vec<SocketAddr> = vec!["192.0.2.10:7755".parse().expect("a test address")];
        let mut encoded = encode(&addrs);
        encoded.truncate(encoded.len() - 1);
        assert_eq!(decode(&encoded), Err(DecodeRefusal::CutShort));
    }

    #[test]
    fn refuses_an_unknown_family_byte() {
        assert_eq!(
            decode(&[1, 9, 0, 0, 0, 0, 0, 0]),
            Err(DecodeRefusal::UnknownFamily { byte: 9 })
        );
    }

    #[test]
    fn empty_list_round_trips() {
        assert_eq!(decode(&encode(&[])).expect("it decodes"), Vec::new());
    }

    #[test]
    fn decode_prefix_leaves_the_trailing_bytes_for_its_caller() {
        let addrs: Vec<SocketAddr> = vec!["192.0.2.10:7755".parse().expect("a test address")];
        let mut bytes = encode(&addrs);
        bytes.extend_from_slice(&[9, 9, 9]);
        let (decoded, rest) = decode_prefix(&bytes).expect("it decodes");
        assert_eq!(decoded, addrs);
        assert_eq!(rest, &[9, 9, 9]);
    }
}
