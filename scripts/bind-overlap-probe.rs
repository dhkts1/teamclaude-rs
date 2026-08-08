//! Can a wildcard-bound listener coexist with tcr's loopback bind on one port?
//!
//! This calls `std::net::TcpListener::bind` — the exact constructor at
//! `server.rs:784` — so it measures the socket options std actually applies
//! rather than a hand-configured socket. That distinction is the whole point:
//! a Python probe that deliberately set no options reported REFUSED for every
//! pairing on both platforms, because std sets `SO_REUSEADDR` on Unix and BSD
//! honours it for wildcard/specific overlap. Reconstructing the call is not the
//! same measurement as making it.
//!
//! Row 1 (same address twice) is the positive control. Note what it cannot
//! catch: same-address refuses with *or* without `SO_REUSEADDR`, so a green
//! control says nothing about whether the socket is configured like tcr's.
//! That is exactly how the first probe passed its control while measuring the
//! wrong thing. The `reuseaddr=` field below is the check that does catch it.
//!
//! Build and run: `rustc bind-overlap-probe.rs -o /tmp/bp && /tmp/bp`

use std::net::TcpListener;
use std::os::unix::io::AsRawFd;

const PORT: u16 = 47832;

#[cfg(target_os = "macos")]
const SOL_SOCKET: i32 = 0xffff;
#[cfg(target_os = "macos")]
const SO_REUSEADDR: i32 = 0x0004;
#[cfg(target_os = "macos")]
const SO_REUSEPORT: i32 = 0x0200;

#[cfg(target_os = "linux")]
const SOL_SOCKET: i32 = 1;
#[cfg(target_os = "linux")]
const SO_REUSEADDR: i32 = 2;
#[cfg(target_os = "linux")]
const SO_REUSEPORT: i32 = 15;

extern "C" {
    fn getsockopt(s: i32, level: i32, name: i32, val: *mut i32, len: *mut u32) -> i32;
}

fn opt(l: &TcpListener, name: i32) -> String {
    let mut v: i32 = -1;
    let mut len: u32 = 4;
    let rc = unsafe { getsockopt(l.as_raw_fd(), SOL_SOCKET, name, &mut v, &mut len) };
    if rc == 0 {
        v.to_string()
    } else {
        "ERR".to_string()
    }
}

fn main() {
    println!("os={}", std::env::consts::OS);

    // What does std actually configure? This is the claim the first probe got wrong.
    match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(l) => println!(
            "std TcpListener::bind -> SO_REUSEADDR={} SO_REUSEPORT={}",
            opt(&l, SO_REUSEADDR),
            opt(&l, SO_REUSEPORT)
        ),
        Err(e) => println!("std TcpListener::bind -> probe bind failed: {e}"),
    }

    let cases = [
        ("127.0.0.1", "127.0.0.1", "CONTROL same-address"),
        ("0.0.0.0", "127.0.0.1", "wildcard-then-specific"),
        ("[::]", "127.0.0.1", "v6-wildcard-then-v4-specific"),
        ("127.0.0.1", "0.0.0.0", "specific-then-wildcard"),
    ];

    for (first, second, label) in cases {
        let a = match TcpListener::bind(format!("{first}:{PORT}").as_str()) {
            Ok(l) => l,
            Err(e) => {
                println!("{first:>9} -> {second:<9} : FIRST-BIND-FAILED {e}  ({label})");
                continue;
            }
        };
        match TcpListener::bind(format!("{second}:{PORT}").as_str()) {
            Ok(b) => {
                println!("{first:>9} -> {second:<9} : SECOND-BIND-SUCCEEDED  ({label})");
                drop(b);
            }
            Err(e) => {
                println!("{first:>9} -> {second:<9} : REFUSED {:?}  ({label})", e.kind());
            }
        }
        drop(a);
    }
}
