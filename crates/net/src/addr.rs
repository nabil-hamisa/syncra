//! Address helpers for the rendezvous flow.

use std::net::{SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};

use anyhow::{Context, Result};

/// Resolve "host:port" (name or IP) to its first IPv4 address.
pub fn resolve_v4(s: &str) -> Result<SocketAddr> {
    s.to_socket_addrs()
        .with_context(|| format!("resolve {s}"))?
        .find(SocketAddr::is_ipv4)
        .with_context(|| format!("{s} has no IPv4 address"))
}

/// This machine's LAN candidate for a socket bound to `port`: the source
/// address the OS would use to reach the internet (no packet is sent).
pub fn local_candidates(port: u16) -> Vec<SocketAddrV4> {
    let probe = || -> Option<SocketAddrV4> {
        let s = UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.1:9").ok()?;
        match s.local_addr().ok()? {
            SocketAddr::V4(a) if !a.ip().is_unspecified() => Some(SocketAddrV4::new(*a.ip(), port)),
            _ => None,
        }
    };
    probe().into_iter().collect()
}
