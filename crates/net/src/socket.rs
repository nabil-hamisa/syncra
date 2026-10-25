//! UDP socket with large kernel buffers and, on Windows, UDP Segmentation
//! Offload (USO), which sends a whole run of equal-size datagrams with one
//! syscall (see `socket_win`). Other platforms (the Linux probe and signal
//! tools) send one datagram at a time.
//!
//! The socket also owns end-to-end encryption and relaying (see [`Link`]):
//! once a Noise session is set, every application datagram is sealed on send
//! and opened on receive, and datagrams to the rendezvous server can be
//! wrapped for its relay. Everything above (video, input, FEC, NACK) is
//! unaware of either.

use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use proto::rendezvous::{RELAY, RELAY_HEADER, RENDEZVOUS};

use crate::crypto::{Session, HANDSHAKE, SEALED, SEAL_OVERHEAD};
use crate::sim::{SimConfig, SimLink};

#[cfg(windows)]
#[path = "socket_win.rs"]
mod os;
#[cfg(not(windows))]
#[path = "socket_unix.rs"]
mod os;

/// A single USO send is limited to a 64 KB IP payload.
const USO_MAX_BYTES: usize = 60_000;

pub struct Socket {
    inner: UdpSocket,
    /// Shared by clones: USO turned out to be unsupported at send time.
    uso: Arc<AtomicBool>,
    /// Simulated outgoing link (tests and benchmarks), shared by clones.
    sim: Option<Arc<SimLink>>,
    /// Encryption and relay state, shared by clones. Read on every datagram,
    /// written only when a connection starts or ends: lock-free reads.
    link: Arc<ArcSwap<Link>>,
}

/// How datagrams are transformed between the application and the wire.
/// Rendezvous datagrams (`0xC0`) always pass untouched; handshake datagrams
/// (`0xA0`) are relayed but never sealed.
#[derive(Clone, Default)]
pub struct Link {
    /// The rendezvous server. Relayed datagrams arriving from it are unwrapped.
    pub server: Option<SocketAddr>,
    /// Relay session: datagrams sent to `server` get the relay prefix.
    pub relay: Option<u32>,
    /// Seal every application datagram and open received ones.
    pub session: Option<Arc<Session>>,
    /// Drop unsealed application datagrams even without a session (a host
    /// reached through the rendezvous server talks only to authenticated
    /// clients).
    pub require_session: bool,
}

impl Link {
    fn is_plain(&self) -> bool {
        self.server.is_none() && self.session.is_none() && !self.require_session
    }

    fn relay_to(&self, d: &[u8], to: SocketAddr) -> Option<u32> {
        self.relay
            .filter(|_| Some(to) == self.server && d.first() != Some(&RENDEZVOUS))
    }

    fn seal_with(&self, d: &[u8]) -> Option<&Session> {
        self.session
            .as_deref()
            .filter(|_| !matches!(d.first(), Some(&RENDEZVOUS) | Some(&HANDSHAKE)))
    }
}

/// Bytes the link adds to datagram `d` sent to `to`.
fn wrap_len(link: &Link, d: &[u8], to: SocketAddr) -> usize {
    link.relay_to(d, to).map_or(0, |_| RELAY_HEADER)
        + link.seal_with(d).map_or(0, |_| SEAL_OVERHEAD)
}

/// Write `d` as it goes on the wire into `out` (sized by `wrap_len`).
fn wrap(link: &Link, d: &[u8], to: SocketAddr, out: &mut [u8]) -> io::Result<usize> {
    let mut at = 0;
    if let Some(id) = link.relay_to(d, to) {
        out[0] = RELAY;
        out[1..RELAY_HEADER].copy_from_slice(&id.to_le_bytes());
        at = RELAY_HEADER;
    }
    match link.seal_with(d) {
        Some(s) => Ok(at + s.seal(d, &mut out[at..]).map_err(io::Error::other)?),
        None => {
            out[at..at + d.len()].copy_from_slice(d);
            Ok(at + d.len())
        }
    }
}

/// Turn a wire datagram into what the application sees; None drops it.
fn unwrap(link: &Link, mut d: &[u8], from: SocketAddr, out: &mut [u8]) -> Option<usize> {
    if Some(from) == link.server && d.first() == Some(&RELAY) {
        d = d.get(RELAY_HEADER..)?;
    }
    match *d.first()? {
        SEALED => link.session.as_ref()?.open(d, out),
        RENDEZVOUS | HANDSHAKE => copy(d, out),
        _ if link.session.is_some() || link.require_session => None,
        _ => copy(d, out),
    }
}

/// `buf[..len]`, growing (never shrinking) the buffer: no re-zeroing per call.
fn grow(buf: &mut Vec<u8>, len: usize) -> &mut [u8] {
    if buf.len() < len {
        buf.resize(len, 0);
    }
    &mut buf[..len]
}

fn copy(d: &[u8], out: &mut [u8]) -> Option<usize> {
    out.get_mut(..d.len())?.copy_from_slice(d);
    Some(d.len())
}

thread_local! {
    /// Per-thread wire buffers, so sealing never allocates per datagram.
    static SEND_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static RECV_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl Socket {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let inner = UdpSocket::bind(addr)?;
        let s = Socket {
            inner,
            uso: Arc::new(AtomicBool::new(false)),
            sim: None,
            link: Arc::default(),
        };
        os::set_buffers(&s.inner, 4 << 20, 8 << 20)?;
        Ok(s)
    }

    /// Enable USO if the OS supports it (probed with the socket option, which
    /// is then reset to 0 so it never applies to plain sends). Returns whether
    /// it is on; without it [`Socket::send_segments`] sends one datagram at a time.
    pub fn enable_uso(&mut self) -> io::Result<bool> {
        let on = os::probe_uso(&self.inner)?;
        self.uso.store(on, Ordering::Relaxed);
        Ok(on)
    }

    pub fn uso_enabled(&self) -> bool {
        self.uso.load(Ordering::Relaxed)
    }

    /// A second handle to the same socket (e.g. one thread sends, another receives).
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Socket {
            inner: self.inner.try_clone()?,
            uso: self.uso.clone(),
            sim: self.sim.clone(),
            link: self.link.clone(),
        })
    }

    pub fn link(&self) -> Arc<Link> {
        self.link.load_full()
    }

    /// Replace the encryption/relay state for this socket and all clones.
    pub fn set_link(&self, link: Link) {
        self.link.store(Arc::new(link));
    }

    /// Route every datagram sent through this socket (and later clones) over
    /// a simulated link.
    pub fn set_sim(&mut self, cfg: SimConfig) -> io::Result<()> {
        self.sim = Some(SimLink::new(cfg, self.inner.try_clone()?));
        Ok(())
    }

    pub fn sim(&self) -> Option<&SimLink> {
        self.sim.as_deref()
    }

    pub fn set_read_timeout(&self, t: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(t)
    }

    pub fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nb)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub fn send_to(&self, buf: &[u8], to: SocketAddr) -> io::Result<usize> {
        let link = self.link.load();
        let extra = wrap_len(&link, buf, to);
        if extra == 0 {
            return self.send_raw(buf, to);
        }
        SEND_BUF.with_borrow_mut(|out| {
            let out = grow(out, buf.len() + extra);
            let n = wrap(&link, buf, to, out)?;
            self.send_raw(&out[..n], to)
        })?;
        Ok(buf.len())
    }

    fn send_raw(&self, buf: &[u8], to: SocketAddr) -> io::Result<usize> {
        if let Some(sim) = &self.sim {
            sim.send(buf, to);
            return Ok(buf.len());
        }
        self.inner.send_to(buf, to)
    }

    /// Receive one application datagram (opened, if the link is sealed), or
    /// a rendezvous / handshake datagram as is. Relayed datagrams come back
    /// with the server as their source.
    ///
    /// A datagram that fails authentication (or is unsealed while the link
    /// requires a session) is dropped and reported as `WouldBlock`, so a
    /// flood of junk can't keep the caller from its periodic work.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        if self.link.load().is_plain() {
            return self.inner.recv_from(buf);
        }
        RECV_BUF.with_borrow_mut(|raw| {
            let raw = grow(raw, 65536);
            let (n, from) = self.inner.recv_from(raw)?;
            // Loaded after the (possibly long) wait, so a link set meanwhile
            // applies to this datagram.
            let link = self.link.load();
            unwrap(&link, &raw[..n], from, buf)
                .map(|n| (n, from))
                .ok_or_else(|| io::ErrorKind::WouldBlock.into())
        })
    }

    /// Send `buf` as consecutive datagrams of `segment` bytes (the last may be
    /// shorter). With USO each batch of up to ~60 KB is a single syscall.
    pub fn send_segments(&self, buf: &[u8], segment: usize, to: SocketAddr) -> io::Result<()> {
        let link = self.link.load();
        let extra = wrap_len(&link, buf, to);
        if extra == 0 {
            return self.send_segments_raw(buf, segment, to);
        }
        // Wrap each datagram; they stay equal-sized, so USO still applies.
        // One USO batch at a time: the first datagrams leave while the rest
        // of a big (IDR) slice is still unsealed.
        let wire_segment = segment + extra;
        let per_batch = (USO_MAX_BYTES / wire_segment).max(1);
        SEND_BUF.with_borrow_mut(|out| {
            let out = grow(out, per_batch * wire_segment);
            for batch in buf.chunks(per_batch * segment) {
                let mut at = 0;
                for d in batch.chunks(segment) {
                    // Every datagram must grow by the same amount, or USO
                    // would cut the wire buffer in the wrong places.
                    if wrap_len(&link, d, to) != extra {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "send_segments: mixed datagram kinds in one buffer",
                        ));
                    }
                    at += wrap(&link, d, to, &mut out[at..])?;
                }
                self.send_segments_raw(&out[..at], wire_segment, to)?;
            }
            Ok(())
        })
    }

    fn send_segments_raw(&self, buf: &[u8], segment: usize, to: SocketAddr) -> io::Result<()> {
        if let Some(sim) = &self.sim {
            for d in buf.chunks(segment) {
                sim.send(d, to);
            }
            return Ok(());
        }
        if self.uso_enabled() && buf.len() > segment {
            let batch = (USO_MAX_BYTES / segment).max(1) * segment;
            for b in buf.chunks(batch) {
                if b.len() <= segment {
                    self.inner.send_to(b, to)?;
                    continue;
                }
                match os::send_uso(&self.inner, b, segment, to) {
                    Ok(()) => {}
                    Err(e) if os::uso_rejected(&e) => {
                        tracing::warn!(
                            "per-message USO rejected ({e}); falling back to per-datagram sends"
                        );
                        self.uso.store(false, Ordering::Relaxed);
                        for d in b.chunks(segment) {
                            self.inner.send_to(d, to)?;
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            return Ok(());
        }
        for d in buf.chunks(segment) {
            self.inner.send_to(d, to)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn segments_arrive_as_separate_datagrams() {
        let rx = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut tx = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let uso = tx.enable_uso().unwrap();
        rx.inner
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        // Two different segment sizes back to back, plus a plain datagram
        // bigger than the first segment size: per-message USO must not leak.
        for (segment, n, tail) in [(1200usize, 4usize, 300usize), (344, 3, 344)] {
            let mut buf = vec![0u8; segment * n + tail];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (i / segment) as u8;
            }
            tx.send_segments(&buf, segment, rx.local_addr().unwrap())
                .unwrap();
            let mut d = [0u8; 65536];
            let mut sizes = Vec::new();
            for _ in 0..n + 1 {
                let (len, _) = rx.recv_from(&mut d).unwrap();
                assert!(d[..len].iter().all(|&b| b == d[0]), "segment mixed data");
                sizes.push(len);
            }
            eprintln!("USO {uso}: segment {segment} -> {sizes:?}");
            let mut want = vec![segment; n];
            want.push(tail);
            assert_eq!(sizes, want);
        }
        tx.send_to(&[7u8; 1000], rx.local_addr().unwrap()).unwrap();
        let mut d = [0u8; 65536];
        assert_eq!(
            rx.recv_from(&mut d).unwrap().0,
            1000,
            "plain send unaffected"
        );
    }

    fn sessions() -> (Session, Session) {
        use crate::crypto::{Handshake, Identity, Step};
        let (c, h) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let mut ini = Handshake::initiator(&c, b"").unwrap();
        let mut resp = Handshake::responder(&h).unwrap();
        let m0 = ini.start().unwrap();
        let Step::Send(m1) = resp.on_message(&m0).unwrap() else {
            panic!()
        };
        let Step::Done(Some(m2)) = ini.on_message(&m1).unwrap() else {
            panic!()
        };
        resp.on_message(&m2).unwrap();
        (ini.into_session().unwrap(), resp.into_session().unwrap())
    }

    #[test]
    fn sealed_segments_and_relay() {
        let rx = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut tx = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        tx.enable_uso().unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (to, from) = (rx.local_addr().unwrap(), tx.local_addr().unwrap());
        let (c, h) = sessions();
        // The sender relays through "the server", which here is the receiver.
        tx.set_link(Link {
            server: Some(to),
            relay: Some(7),
            session: Some(Arc::new(c)),
            require_session: false,
        });
        rx.set_link(Link {
            server: Some(from),
            relay: None,
            session: Some(Arc::new(h)),
            require_session: true,
        });

        let mut buf = vec![proto::VERSION; 1200 * 3 + 100];
        for (i, b) in buf.iter_mut().enumerate().skip(1) {
            *b = (i / 1200) as u8 + 10;
        }
        for i in 0..4 {
            buf[i * 1200] = proto::VERSION;
        }
        tx.send_segments(&buf, 1200, to).unwrap();
        let mut d = [0u8; 1200];
        for (i, want) in buf.chunks(1200).enumerate() {
            let (n, src) = rx.recv_from(&mut d).unwrap();
            assert_eq!(src, from);
            assert_eq!(&d[..n], want, "segment {i}");
        }
        // Unsealed application datagrams are dropped; rendezvous passes as is.
        let plain = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        plain.send_to(&[proto::VERSION; 50], to).unwrap();
        plain.send_to(&[RENDEZVOUS, 1, 2], to).unwrap();
        let e = rx.recv_from(&mut d).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "dropped, not hidden");
        let (n, _) = rx.recv_from(&mut d).unwrap();
        assert_eq!(&d[..n], &[RENDEZVOUS, 1, 2]);
    }
}
