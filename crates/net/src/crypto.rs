//! End-to-end encryption: Noise_XX handshake, then AES-256-GCM per datagram.
//!
//! UDP reorders and drops, so the transport uses explicit nonces
//! (`StatelessTransportState`): every sealed datagram carries its 64-bit
//! nonce, and the receiver rejects replays with a sliding window.
//!
//! Wire format of a sealed datagram: `[SEALED][nonce u64 LE][ciphertext][16-byte tag]`.
//! Handshake datagrams: `[HANDSHAKE][msg index u8][noise message]`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use snow::{Builder, HandshakeState, StatelessTransportState};

pub const NOISE_PARAMS: &str = "Noise_XX_25519_AESGCM_SHA256";
/// First byte of a sealed (encrypted) datagram.
pub const SEALED: u8 = 0xB0;
/// First byte of a Noise handshake datagram.
pub const HANDSHAKE: u8 = 0xA0;
/// Bytes a sealed datagram adds: marker + nonce + AEAD tag.
pub const SEAL_OVERHEAD: usize = 1 + 8 + 16;
const REPLAY_WINDOW: u64 = 1024;

/// A long-term X25519 identity, stored on disk so the host ID stays stable.
#[derive(Clone)]
pub struct Identity {
    pub private: Vec<u8>,
    pub public: [u8; 32],
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let kp = Builder::new(NOISE_PARAMS.parse()?).generate_keypair()?;
        let public: [u8; 32] = kp.public.as_slice().try_into().context("public key size")?;
        Ok(Identity {
            private: kp.private,
            public,
        })
    }

    /// Load `<data_dir>/<name>.key`, creating it on first use.
    pub fn load_or_create(name: &str) -> Result<Self> {
        let dir = data_dir()?;
        let path = dir.join(format!("{name}.key"));
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 64 {
                return Ok(Identity {
                    private: bytes[..32].to_vec(),
                    public: bytes[32..].try_into().unwrap(),
                });
            }
            bail!("{} is corrupt (expected 64 bytes)", path.display());
        }
        std::fs::create_dir_all(&dir)?;
        let id = Self::generate()?;
        let mut bytes = id.private.clone();
        bytes.extend_from_slice(&id.public);
        std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
        tracing::info!("created identity {}", path.display());
        Ok(id)
    }

    /// The 9-digit ID users type to reach this host.
    pub fn host_id(&self) -> u32 {
        host_id(&self.public)
    }
}

pub use proto::rendezvous::{format_id, host_id};

pub fn parse_id(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.len() != 9 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `%LOCALAPPDATA%\Syncra` on Windows, `$XDG_DATA_HOME/syncra` (or
/// `~/.local/share/syncra`) elsewhere. A directory from before the rename
/// (`parsec-rs`) is moved over once, so the ID and trusted devices stay.
pub fn data_dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    let base = base.ok_or_else(|| anyhow!("no data directory (LOCALAPPDATA / HOME not set)"))?;
    let dir = base.join(if cfg!(windows) { "Syncra" } else { "syncra" });
    let old = base.join("parsec-rs");
    if !dir.exists() && old.is_dir() && std::fs::rename(&old, &dir).is_err() {
        // The old app still runs (its log is open): copy instead, so the
        // keys (the ID), trusted devices and settings come along anyway.
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(entries) = std::fs::read_dir(&old) {
            for e in entries.flatten() {
                if e.path().is_file() {
                    let _ = std::fs::copy(e.path(), dir.join(e.file_name()));
                }
            }
        }
    }
    Ok(dir)
}

/// One side of a Noise_XX handshake driven datagram by datagram.
/// Initiator (client): `start` → msg 0; `on_message(msg 1)` → msg 2, done.
/// Responder (host): `on_message(msg 0)` → msg 1; `on_message(msg 2)` → done.
pub struct Handshake {
    state: Option<HandshakeState>,
    initiator: bool,
    /// Last message sent, to resend when the peer repeats (UDP loss).
    last_sent: Vec<u8>,
    /// Index of the next message we expect from the peer.
    expect: u8,
    /// Payload carried in our final message (e.g. the client's device name).
    payload: Vec<u8>,
    /// Payload the peer sent (available once done).
    pub peer_payload: Vec<u8>,
}

pub enum Step {
    /// Send this datagram (and resend it if nothing arrives).
    Send(Vec<u8>),
    /// Handshake complete; send this datagram too if present.
    Done(Option<Vec<u8>>),
    /// Not for us / duplicate already answered: nothing to do.
    Ignore,
}

impl Handshake {
    pub fn initiator(id: &Identity, payload: &[u8]) -> Result<Self> {
        let state = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(&id.private)?
            .build_initiator()?;
        Ok(Handshake {
            state: Some(state),
            initiator: true,
            last_sent: Vec::new(),
            expect: 1,
            payload: payload.to_vec(),
            peer_payload: Vec::new(),
        })
    }

    pub fn responder(id: &Identity) -> Result<Self> {
        let state = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(&id.private)?
            .build_responder()?;
        Ok(Handshake {
            state: Some(state),
            initiator: false,
            last_sent: Vec::new(),
            expect: 0,
            payload: Vec::new(),
            peer_payload: Vec::new(),
        })
    }

    /// Initiator: first message.
    pub fn start(&mut self) -> Result<Vec<u8>> {
        self.write(0, &[])
    }

    fn write(&mut self, index: u8, payload: &[u8]) -> Result<Vec<u8>> {
        let st = self.state.as_mut().context("handshake finished")?;
        let mut buf = vec![0u8; 2 + 256 + payload.len()];
        buf[0] = HANDSHAKE;
        buf[1] = index;
        let n = st.write_message(payload, &mut buf[2..])?;
        buf.truncate(2 + n);
        self.last_sent = buf.clone();
        self.expect = index + 1;
        Ok(buf)
    }

    pub fn last_sent(&self) -> &[u8] {
        &self.last_sent
    }

    /// Feed a received handshake datagram.
    pub fn on_message(&mut self, d: &[u8]) -> Result<Step> {
        if d.len() < 2 || d[0] != HANDSHAKE {
            return Ok(Step::Ignore);
        }
        let index = d[1];
        let expected = self.expect;
        if index != expected {
            // A repeat of an earlier peer message: our answer was lost, resend it.
            if !self.last_sent.is_empty() && index < expected {
                return Ok(Step::Send(self.last_sent.clone()));
            }
            return Ok(Step::Ignore);
        }
        let st = self.state.as_mut().context("handshake finished")?;
        let mut payload = vec![0u8; d.len()];
        let n = st.read_message(&d[2..], &mut payload)?;
        payload.truncate(n);
        match (self.initiator, index) {
            (false, 0) => Ok(Step::Send(self.write(1, &[])?)),
            (true, 1) => {
                let p = self.payload.clone();
                let last = self.write(2, &p)?;
                Ok(Step::Done(Some(last)))
            }
            (false, 2) => {
                self.peer_payload = payload;
                self.expect = 3;
                Ok(Step::Done(None))
            }
            _ => bail!("unexpected handshake message {index}"),
        }
    }

    pub fn remote_static(&self) -> Option<[u8; 32]> {
        self.state.as_ref()?.get_remote_static()?.try_into().ok()
    }

    pub fn into_session(mut self) -> Result<Session> {
        let st = self.state.take().context("handshake already consumed")?;
        if !st.is_handshake_finished() {
            bail!("handshake not finished");
        }
        let remote: [u8; 32] = st
            .get_remote_static()
            .context("no remote key")?
            .try_into()?;
        Ok(Session {
            transport: st.into_stateless_transport_mode()?,
            remote,
            send_nonce: AtomicU64::new(0),
            replay: Mutex::new(ReplayWindow::default()),
        })
    }
}

/// An established encrypted session, shared by all threads using the socket.
pub struct Session {
    transport: StatelessTransportState,
    pub remote: [u8; 32],
    send_nonce: AtomicU64,
    replay: Mutex<ReplayWindow>,
}

impl Session {
    /// Encrypt `plain` into `out` (needs `plain.len() + SEAL_OVERHEAD` bytes).
    pub fn seal(&self, plain: &[u8], out: &mut [u8]) -> Result<usize> {
        let nonce = self.send_nonce.fetch_add(1, Ordering::Relaxed);
        out[0] = SEALED;
        out[1..9].copy_from_slice(&nonce.to_le_bytes());
        let n = self.transport.write_message(nonce, plain, &mut out[9..])?;
        Ok(9 + n)
    }

    /// Decrypt a sealed datagram into `out`. None if forged, corrupt or replayed.
    pub fn open(&self, sealed: &[u8], out: &mut [u8]) -> Option<usize> {
        if sealed.len() < SEAL_OVERHEAD || sealed[0] != SEALED {
            return None;
        }
        let nonce = u64::from_le_bytes(sealed[1..9].try_into().ok()?);
        // Authenticate first, then check and mark under one lock, so two
        // receiving threads can't both accept the same datagram. Only one
        // thread receives in practice, so the lock is uncontended.
        let n = self.transport.read_message(nonce, &sealed[9..], out).ok()?;
        let mut w = self.replay.lock().unwrap();
        if !w.fresh(nonce) {
            return None;
        }
        w.mark(nonce);
        Some(n)
    }
}

/// Sliding replay window over the last `REPLAY_WINDOW` nonces.
#[derive(Default)]
struct ReplayWindow {
    highest: Option<u64>,
    bits: [u64; (REPLAY_WINDOW / 64) as usize],
}

impl ReplayWindow {
    fn fresh(&self, n: u64) -> bool {
        match self.highest {
            None => true,
            Some(h) if n > h => true,
            Some(h) if h - n >= REPLAY_WINDOW => false,
            Some(_) => {
                let i = (n % REPLAY_WINDOW) as usize;
                self.bits[i / 64] & (1 << (i % 64)) == 0
            }
        }
    }

    fn mark(&mut self, n: u64) {
        let h = self.highest.unwrap_or(n);
        if self.highest.is_none() || n > h {
            // Clear the slots the window slides past.
            let start = self.highest.map_or(n, |h| h + 1);
            for m in start..=n.min(start + REPLAY_WINDOW) {
                let i = (m % REPLAY_WINDOW) as usize;
                self.bits[i / 64] &= !(1 << (i % 64));
            }
            self.highest = Some(n);
        }
        let i = (n % REPLAY_WINDOW) as usize;
        self.bits[i / 64] |= 1 << (i % 64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake() -> (Session, Session, Vec<u8>) {
        let (c, h) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let mut ini = Handshake::initiator(&c, b"laptop").unwrap();
        let mut resp = Handshake::responder(&h).unwrap();
        let m0 = ini.start().unwrap();
        let Step::Send(m1) = resp.on_message(&m0).unwrap() else {
            panic!()
        };
        // A repeated msg 0 (our msg 1 was lost) gets msg 1 again.
        assert!(matches!(resp.on_message(&m0).unwrap(), Step::Send(ref x) if *x == m1));
        let Step::Done(Some(m2)) = ini.on_message(&m1).unwrap() else {
            panic!()
        };
        assert!(matches!(resp.on_message(&m2).unwrap(), Step::Done(None)));
        assert_eq!(resp.remote_static(), Some(c.public));
        assert_eq!(ini.remote_static(), Some(h.public));
        let name = resp.peer_payload.clone();
        (
            ini.into_session().unwrap(),
            resp.into_session().unwrap(),
            name,
        )
    }

    #[test]
    fn handshake_authenticates_and_encrypts_both_ways() {
        let (client, host, name) = handshake();
        assert_eq!(name, b"laptop");
        let mut sealed = [0u8; 1500];
        let mut plain = [0u8; 1500];
        let n = client.seal(b"hello host", &mut sealed).unwrap();
        assert_eq!(n, 10 + SEAL_OVERHEAD);
        let m = host.open(&sealed[..n], &mut plain).unwrap();
        assert_eq!(&plain[..m], b"hello host");
        let n = host.seal(b"hi", &mut sealed).unwrap();
        assert_eq!(
            client
                .open(&sealed[..n], &mut plain)
                .map(|m| plain[..m].to_vec()),
            Some(b"hi".to_vec())
        );
    }

    #[test]
    fn rejects_tampering_and_replay_but_allows_reordering() {
        let (client, host, _) = handshake();
        let mut out = [0u8; 256];
        let seal = |s: &Session, msg: &[u8]| {
            let mut b = vec![0u8; msg.len() + SEAL_OVERHEAD];
            let n = s.seal(msg, &mut b).unwrap();
            b.truncate(n);
            b
        };
        let (a, b, c) = (
            seal(&client, b"a"),
            seal(&client, b"b"),
            seal(&client, b"c"),
        );
        assert!(host.open(&c, &mut out).is_some(), "out of order is fine");
        assert!(host.open(&a, &mut out).is_some());
        assert!(host.open(&a, &mut out).is_none(), "replay rejected");
        let mut bad = b.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(host.open(&bad, &mut out).is_none(), "tampered rejected");
        assert!(host.open(&b, &mut out).is_some(), "original still accepted");
        // Far-old nonces are rejected once the window has moved on.
        for _ in 0..(REPLAY_WINDOW + 10) {
            let d = seal(&client, b"x");
            host.open(&d, &mut out).unwrap();
        }
        assert!(host.open(&c, &mut out).is_none());
    }

    /// `cargo test -p net --release -- --ignored seal_open_cost --nocapture`
    #[test]
    #[ignore]
    fn seal_open_cost() {
        let (client, host, _) = handshake();
        let plain = [7u8; 1200];
        let mut sealed = [0u8; 1200 + SEAL_OVERHEAD];
        let mut out = [0u8; 1200];
        let n = 200_000;
        let mut seal_ns = Vec::with_capacity(n);
        let mut open_ns = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            let len = client.seal(&plain, &mut sealed).unwrap();
            let t2 = std::time::Instant::now();
            host.open(&sealed[..len], &mut out).unwrap();
            open_ns.push(t2.elapsed().as_nanos() as u64);
            seal_ns.push((t2 - t).as_nanos() as u64);
        }
        for (what, v) in [("seal", &mut seal_ns), ("open", &mut open_ns)] {
            v.sort_unstable();
            eprintln!(
                "{what} 1200 B: p50 {} ns p95 {} ns",
                v[v.len() / 2],
                v[v.len() * 95 / 100]
            );
        }
    }

    #[test]
    fn ids() {
        let id = Identity::generate().unwrap();
        assert!(id.host_id() < 1_000_000_000);
        assert_eq!(parse_id(&format_id(482_913_077)), Some(482_913_077));
        assert_eq!(parse_id("482-913-077"), Some(482_913_077));
        assert_eq!(parse_id("48291307"), None);
        assert_eq!(format_id(7), "000 000 007");
    }
}
