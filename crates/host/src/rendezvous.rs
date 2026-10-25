//! Host side of the ID connect flow: register with the signal server, ask the
//! user to approve each new device, open a path to it (hole punch, or the
//! server's relay) and authenticate it with a Noise handshake.
//!
//! Runs on `net-rx`, which owns the socket's receive side: rendezvous and
//! handshake datagrams are handed here, the rest of the stream code only sees
//! the decrypted session once it exists.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use net::crypto::{format_id, Handshake, Identity, Step, HANDSHAKE};
use net::socket::Link;
use net::Socket;
use proto::rendezvous::{Msg, RENDEZVOUS};

const REGISTER_EVERY: Duration = Duration::from_secs(5);
const PUNCH_EVERY: Duration = Duration::from_millis(100);
/// An approved client that hasn't completed the handshake by then is dropped.
const PENDING_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Config {
    pub server: SocketAddr,
    /// Accept every device without asking (tests).
    pub auto_approve: bool,
}

/// A device the user approved, until its handshake completes.
struct Pending {
    session: u32,
    pubkey: [u8; 32],
    name: String,
    candidates: Vec<SocketAddr>,
    responder: Handshake,
    since: Instant,
    punch_at: Instant,
}

struct Decision {
    session: u32,
    accept: bool,
    always: bool,
}

/// What `net-rx` must do after a datagram or tick.
pub enum Event {
    /// A client authenticated over `path`; the socket is already sealed.
    Connected { path: SocketAddr, name: String },
}

pub struct Rendezvous {
    cfg: Config,
    identity: Identity,
    locals: Vec<SocketAddrV4>,
    registered_at: Option<Instant>,
    confirmed: bool,
    trusted: Trusted,
    /// Sessions with an approval popup open, or already decided.
    asked: HashMap<u32, Option<bool>>,
    decisions: (mpsc::Sender<Decision>, mpsc::Receiver<Decision>),
    /// Requests waiting for the user, by session.
    parked: HashMap<u32, Pending>,
    pending: Option<Pending>,
    next_tick: Instant,
    /// Last REGISTERED reply (server reachability, for the host window).
    registered_ok: Option<Instant>,
    unreachable_shown: bool,
    /// A client is streaming (set by the session, whatever path it used).
    pub connected: bool,
}

impl Rendezvous {
    pub fn new(cfg: Config, socket: &Socket) -> Result<Self> {
        let identity = Identity::load_or_create("host")?;
        let port = socket.local_addr()?.port();
        socket.set_link(Link {
            server: Some(cfg.server),
            require_session: true,
            ..Link::default()
        });
        tracing::info!(
            "host ID: {}  (signal server {})",
            format_id(identity.host_id()),
            cfg.server
        );
        Ok(Rendezvous {
            cfg,
            identity,
            locals: net::addr::local_candidates(port),
            registered_at: None,
            confirmed: false,
            trusted: Trusted::load(),
            asked: HashMap::new(),
            decisions: mpsc::channel(),
            parked: HashMap::new(),
            pending: None,
            next_tick: Instant::now(),
            registered_ok: None,
            unreachable_shown: false,
            connected: false,
        })
    }

    /// True for datagrams this module handles.
    pub fn wants(d: &[u8]) -> bool {
        matches!(d.first(), Some(&RENDEZVOUS) | Some(&HANDSHAKE))
    }

    pub fn on_datagram(&mut self, socket: &Socket, d: &[u8], from: SocketAddr) -> Option<Event> {
        if d[0] == HANDSHAKE {
            return self.handshake(socket, d, from);
        }
        let msg = Msg::decode(d)?;
        match msg {
            Msg::Registered { id, public } if from == self.cfg.server => {
                if !self.confirmed {
                    tracing::info!("registered as {} (public address {public})", format_id(id));
                    self.confirmed = true;
                }
                if (self.registered_ok.is_none() || self.unreachable_shown) && !self.connected {
                    crate::panel::STATUS.set_state("Ready: waiting for a device to connect");
                    self.unreachable_shown = false;
                }
                self.registered_ok = Some(Instant::now());
            }
            Msg::Incoming {
                session,
                pubkey,
                name,
                public,
                locals,
            } if from == self.cfg.server => {
                self.incoming(socket, session, pubkey, name, public, locals)
            }
            Msg::Punch { session }
                if self.pending.as_ref().is_some_and(|p| p.session == session) =>
            {
                let _ = socket.send_to(&Msg::PunchAck { session }.encode(), from);
            }
            _ => {}
        }
        None
    }

    fn incoming(
        &mut self,
        socket: &Socket,
        session: u32,
        pubkey: [u8; 32],
        name: String,
        public: SocketAddrV4,
        locals: Vec<SocketAddrV4>,
    ) {
        match self.asked.get(&session) {
            // The server repeats Incoming until it gets an answer.
            Some(Some(accept)) => {
                let _ = socket.send_to(
                    &Msg::Answer {
                        session,
                        accept: *accept,
                    }
                    .encode(),
                    self.cfg.server,
                );
                return;
            }
            Some(None) => return, // popup still open
            None => {}
        }
        // The name is unauthenticated (it comes with the rendezvous request):
        // no control characters, so it can't fake lines in the popup.
        let name: String = name
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        let trusted = self.trusted.contains(&pubkey);
        if !trusted && !self.cfg.auto_approve && !self.parked.is_empty() {
            // One popup at a time; the server repeats the request, so this
            // one is asked once the open popup is answered.
            return;
        }
        let mut candidates: Vec<SocketAddr> = vec![public.into()];
        candidates.extend(locals.iter().map(|&a| SocketAddr::from(a)));
        candidates.dedup();
        tracing::info!(
            "connection request from {name:?} (key {}) at {public}",
            fingerprint(&pubkey)
        );
        crate::panel::STATUS.set_state(format!("\"{name}\" wants to connect…"));
        // Remember the request so the decision can be applied when it comes.
        self.asked.insert(session, None);
        let pending = Pending {
            session,
            pubkey,
            name: name.clone(),
            candidates,
            responder: match Handshake::responder(&self.identity) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("handshake setup failed: {e}");
                    return;
                }
            },
            since: Instant::now(),
            punch_at: Instant::now(),
        };
        if trusted || self.cfg.auto_approve {
            if trusted {
                tracing::info!("{name:?} is a trusted device: accepted");
            }
            self.decide(socket, pending, true, false);
            return;
        }
        // The popup blocks, so it runs on its own thread; the decision comes
        // back through the channel and is applied in `tick`.
        let tx = self.decisions.0.clone();
        let fp = fingerprint(&pubkey);
        self.parked.insert(session, pending);
        let _ = std::thread::Builder::new()
            .name("approve".into())
            .spawn(move || {
                let (accept, always) = ask_user(&name, &fp);
                let _ = tx.send(Decision {
                    session,
                    accept,
                    always,
                });
            });
    }

    fn decide(&mut self, socket: &Socket, p: Pending, accept: bool, always: bool) {
        let session = p.session;
        self.asked.insert(session, Some(accept));
        let _ = socket.send_to(&Msg::Answer { session, accept }.encode(), self.cfg.server);
        if !accept {
            tracing::info!("denied {:?}", p.name);
            crate::panel::STATUS.set_state("Ready: waiting for a device to connect");
            return;
        }
        if always {
            self.trusted.add(&p.pubkey, &p.name);
        }
        // Relay replies through this session if the client comes in that way.
        let link = socket.link();
        socket.set_link(Link {
            relay: Some(session),
            ..(*link).clone()
        });
        tracing::info!("accepted {:?}; opening a path", p.name);
        self.pending = Some(p);
    }

    fn handshake(&mut self, socket: &Socket, d: &[u8], from: SocketAddr) -> Option<Event> {
        let p = self.pending.as_mut()?;
        match p.responder.on_message(d) {
            Ok(Step::Send(reply)) => {
                let _ = socket.send_to(&reply, from);
                None
            }
            Ok(Step::Done(_)) => {
                let p = self.pending.take()?;
                if p.responder.remote_static() != Some(p.pubkey) {
                    tracing::warn!(
                        "handshake from {from} used a different key than approved; dropped"
                    );
                    return None;
                }
                let session = match p.responder.into_session() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("handshake: {e}");
                        return None;
                    }
                };
                let via = if from == self.cfg.server {
                    "relay"
                } else {
                    "direct"
                };
                tracing::info!("{:?} authenticated, {via} path {from}", p.name);
                self.connected = true;
                crate::panel::STATUS.set_state(format!(
                    "Connected: \"{}\" ({})",
                    p.name,
                    if via == "relay" {
                        "relayed via server"
                    } else {
                        "direct"
                    }
                ));
                socket.set_link(Link {
                    server: Some(self.cfg.server),
                    relay: Some(p.session),
                    session: Some(std::sync::Arc::new(session)),
                    require_session: true,
                });
                Some(Event::Connected {
                    path: from,
                    name: p.name,
                })
            }
            Ok(Step::Ignore) => None,
            Err(e) => {
                // Garbage or a stale handshake: start over for this client.
                tracing::warn!("handshake from {from} failed: {e}");
                if let Ok(r) = Handshake::responder(&self.identity) {
                    p.responder = r;
                }
                None
            }
        }
    }

    /// The session ended: back to accepting only handshakes.
    pub fn on_disconnect(&mut self, socket: &Socket) {
        self.connected = false;
        crate::panel::STATUS.set_state("Ready: waiting for a device to connect");
        crate::panel::STATUS.set_detail("");
        socket.set_link(Link {
            server: Some(self.cfg.server),
            // Keep relaying for a client that is mid-handshake.
            relay: self.pending.as_ref().map(|p| p.session),
            require_session: true,
            ..Link::default()
        });
    }

    /// Registration, approval results and hole punching. Called every
    /// receive timeout (≤ 100 ms).
    pub fn tick(&mut self, socket: &Socket) {
        let now = Instant::now();
        if now < self.next_tick {
            return;
        }
        self.next_tick = now + Duration::from_millis(10);
        // No REGISTERED reply for two rounds: tell the user (once).
        let unreachable = self
            .registered_ok
            .is_none_or(|t| now - t > REGISTER_EVERY * 2 + Duration::from_secs(1))
            && self.registered_at.is_some();
        if unreachable != self.unreachable_shown && !self.connected {
            self.unreachable_shown = unreachable;
            if unreachable {
                crate::panel::STATUS.set_state(format!(
                    "Can't reach the server {} (retrying)",
                    self.cfg.server
                ));
            }
        }
        if self.registered_at.is_none_or(|t| now - t >= REGISTER_EVERY) {
            self.registered_at = Some(now);
            let reg = Msg::Register {
                pubkey: self.identity.public,
                locals: self.locals.clone(),
            };
            let _ = socket.send_to(&reg.encode(), self.cfg.server);
        }
        while let Ok(d) = self.decisions.1.try_recv() {
            if let Some(p) = self.parked.remove(&d.session) {
                self.decide(socket, p, d.accept, d.always);
            }
        }
        if let Some(p) = self.pending.as_mut() {
            if now - p.since > PENDING_TIMEOUT {
                tracing::warn!("{:?} never completed the handshake; giving up", p.name);
                self.pending = None;
            } else if now >= p.punch_at {
                // Opens our NAT toward the client so its punches get through.
                p.punch_at = now + PUNCH_EVERY;
                let punch = Msg::Punch { session: p.session }.encode();
                for &c in &p.candidates {
                    let _ = socket.send_to(&punch, c);
                }
            }
        }
    }
}

/// Short, human-comparable key fingerprint.
fn fingerprint(key: &[u8; 32]) -> String {
    let mut s = String::new();
    for (i, b) in key[..6].iter().enumerate() {
        if i > 0 && i % 2 == 0 {
            s.push(':');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Approve / deny popup, then "always allow?". Returns (accept, always).
fn ask_user(name: &str, fp: &str) -> (bool, bool) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::*;
    let style = MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND;
    let text = HSTRING::from(format!(
        "A device wants to connect and control this PC.\n\nKey: {fp}\nName (unverified): \"{name}\"\n\nAllow?"
    ));
    // SAFETY: plain modal dialog with valid, NUL-terminated strings that
    // outlive the call.
    let accept = unsafe {
        MessageBoxW(
            crate::panel::hwnd(),
            &text,
            &HSTRING::from("Syncra: connection request"),
            style,
        )
    } == IDYES;
    if !accept {
        return (false, false);
    }
    let text = HSTRING::from(format!("Always allow \"{name}\" without asking?"));
    // SAFETY: as above.
    let always =
        unsafe { MessageBoxW(crate::panel::hwnd(), &text, &HSTRING::from("Syncra"), style) }
            == IDYES;
    (true, always)
}

/// Devices the user chose to always allow, one per line: hex key, then name.
struct Trusted {
    path: Option<PathBuf>,
    keys: Vec<[u8; 32]>,
}

impl Trusted {
    fn load() -> Self {
        let path = net::crypto::data_dir()
            .ok()
            .map(|d| d.join("trusted_clients.txt"));
        let keys = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().filter_map(parse_key).collect())
            .unwrap_or_default();
        Trusted { path, keys }
    }

    fn contains(&self, key: &[u8; 32]) -> bool {
        self.keys.contains(key)
    }

    fn add(&mut self, key: &[u8; 32], name: &str) {
        if self.contains(key) {
            return;
        }
        self.keys.push(*key);
        let Some(path) = &self.path else { return };
        let mut line = String::new();
        for b in key {
            let _ = write!(line, "{b:02x}");
        }
        let _ = writeln!(line, " {}", name.replace(['\r', '\n'], " "));
        let r = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?
                .write_all(line.as_bytes())
        });
        if let Err(e) = r {
            tracing::warn!("could not save trusted device: {e}");
        }
    }
}

fn parse_key(line: &str) -> Option<[u8; 32]> {
    let hex = line.split_whitespace().next()?;
    if hex.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(k)
}
