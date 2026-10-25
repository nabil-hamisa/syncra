//! Rendezvous protocol between host, client and the signal server (plain
//! UDP, sent from the same sockets that later carry the stream, so the server
//! sees each side's NAT-mapped public address and the mappings stay open).
//!
//! Nothing here is secret or trusted: the Noise handshake that follows
//! authenticates both ends, and the client checks the host key against the
//! ID the user typed.
//!
//! Flow: host REGISTERs (every few seconds) → client CONNECTs with an ID →
//! server forwards INCOMING to the host → host ANSWERs (after the user
//! approves) → server sends ACCEPTED with the host's addresses to the client
//! → both PUNCH each other's candidates; if that fails they RELAY through
//! the server.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

/// First byte of every rendezvous datagram.
pub const RENDEZVOUS: u8 = 0xC0;
/// First byte of a relayed datagram: `[RELAY][session u32][inner datagram]`.
pub const RELAY: u8 = 0xC7;
pub const RELAY_HEADER: usize = 5;
pub const MAX_NAME: usize = 64;
pub const MAX_LOCALS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// Host → server.
    Register {
        pubkey: [u8; 32],
        locals: Vec<SocketAddrV4>,
    },
    /// Server → host.
    Registered {
        id: u32,
        public: SocketAddrV4,
    },
    /// Client → server.
    Connect {
        host_id: u32,
        pubkey: [u8; 32],
        name: String,
        locals: Vec<SocketAddrV4>,
    },
    /// Server → host: a client wants in.
    Incoming {
        session: u32,
        pubkey: [u8; 32],
        name: String,
        public: SocketAddrV4,
        locals: Vec<SocketAddrV4>,
    },
    /// Host → server: the user's decision.
    Answer {
        session: u32,
        accept: bool,
    },
    /// Server → client.
    Accepted {
        session: u32,
        pubkey: [u8; 32],
        public: SocketAddrV4,
        locals: Vec<SocketAddrV4>,
    },
    Denied {
        session: u32,
    },
    NotFound {
        host_id: u32,
    },
    /// Peer ↔ peer, to open NAT mappings and find a working path.
    Punch {
        session: u32,
    },
    PunchAck {
        session: u32,
    },
}

const REGISTER: u8 = 1;
const REGISTERED: u8 = 2;
const CONNECT: u8 = 3;
const INCOMING: u8 = 4;
const ANSWER: u8 = 5;
const ACCEPTED: u8 = 6;
const DENIED: u8 = 7;
const NOT_FOUND: u8 = 8;
const PUNCH: u8 = 9;
const PUNCH_ACK: u8 = 10;

struct W(Vec<u8>);

impl W {
    fn new(kind: u8) -> Self {
        W(vec![RENDEZVOUS, kind])
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    fn addr(mut self, a: &SocketAddrV4) -> Self {
        self.0.extend_from_slice(&a.ip().octets());
        self.0.extend_from_slice(&a.port().to_be_bytes());
        self
    }
    fn addrs(mut self, v: &[SocketAddrV4]) -> Self {
        let v = &v[..v.len().min(MAX_LOCALS)];
        self.0.push(v.len() as u8);
        for a in v {
            self = self.addr(a);
        }
        self
    }
    fn name(mut self, n: &str) -> Self {
        let mut end = n.len().min(MAX_NAME);
        while !n.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push(end as u8);
        self.0.extend_from_slice(&n.as_bytes()[..end]);
        self
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn key(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }
    fn addr(&mut self) -> Option<SocketAddrV4> {
        let b = self.take(6)?;
        Some(SocketAddrV4::new(
            Ipv4Addr::new(b[0], b[1], b[2], b[3]),
            u16::from_be_bytes([b[4], b[5]]),
        ))
    }
    fn addrs(&mut self) -> Option<Vec<SocketAddrV4>> {
        let n = self.u8()? as usize;
        if n > MAX_LOCALS {
            return None;
        }
        (0..n).map(|_| self.addr()).collect()
    }
    fn name(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n.min(MAX_NAME))?.to_vec()).ok()
    }
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Msg::Register { pubkey, locals } => W::new(REGISTER).bytes(pubkey).addrs(locals).0,
            Msg::Registered { id, public } => W::new(REGISTERED).u32(*id).addr(public).0,
            Msg::Connect {
                host_id,
                pubkey,
                name,
                locals,
            } => {
                W::new(CONNECT)
                    .u32(*host_id)
                    .bytes(pubkey)
                    .name(name)
                    .addrs(locals)
                    .0
            }
            Msg::Incoming {
                session,
                pubkey,
                name,
                public,
                locals,
            } => {
                W::new(INCOMING)
                    .u32(*session)
                    .bytes(pubkey)
                    .name(name)
                    .addr(public)
                    .addrs(locals)
                    .0
            }
            Msg::Answer { session, accept } => {
                W::new(ANSWER).u32(*session).bytes(&[*accept as u8]).0
            }
            Msg::Accepted {
                session,
                pubkey,
                public,
                locals,
            } => {
                W::new(ACCEPTED)
                    .u32(*session)
                    .bytes(pubkey)
                    .addr(public)
                    .addrs(locals)
                    .0
            }
            Msg::Denied { session } => W::new(DENIED).u32(*session).0,
            Msg::NotFound { host_id } => W::new(NOT_FOUND).u32(*host_id).0,
            Msg::Punch { session } => W::new(PUNCH).u32(*session).0,
            Msg::PunchAck { session } => W::new(PUNCH_ACK).u32(*session).0,
        }
    }

    pub fn decode(b: &[u8]) -> Option<Msg> {
        if b.len() < 2 || b[0] != RENDEZVOUS {
            return None;
        }
        let mut r = R(&b[2..]);
        Some(match b[1] {
            REGISTER => Msg::Register {
                pubkey: r.key()?,
                locals: r.addrs()?,
            },
            REGISTERED => Msg::Registered {
                id: r.u32()?,
                public: r.addr()?,
            },
            CONNECT => Msg::Connect {
                host_id: r.u32()?,
                pubkey: r.key()?,
                name: r.name()?,
                locals: r.addrs()?,
            },
            INCOMING => Msg::Incoming {
                session: r.u32()?,
                pubkey: r.key()?,
                name: r.name()?,
                public: r.addr()?,
                locals: r.addrs()?,
            },
            ANSWER => Msg::Answer {
                session: r.u32()?,
                accept: r.u8()? != 0,
            },
            ACCEPTED => Msg::Accepted {
                session: r.u32()?,
                pubkey: r.key()?,
                public: r.addr()?,
                locals: r.addrs()?,
            },
            DENIED => Msg::Denied { session: r.u32()? },
            NOT_FOUND => Msg::NotFound { host_id: r.u32()? },
            PUNCH => Msg::Punch { session: r.u32()? },
            PUNCH_ACK => Msg::PunchAck { session: r.u32()? },
            _ => return None,
        })
    }
}

/// 9-digit ID derived from a public key (keys are uniformly random, so the
/// low bits are too). Clients check that the key they authenticate maps to
/// the ID they typed, so the rendezvous server cannot substitute a host.
pub fn host_id(public: &[u8; 32]) -> u32 {
    (u64::from_le_bytes(public[..8].try_into().unwrap()) % 1_000_000_000) as u32
}

/// "482 913 077" display form.
pub fn format_id(id: u32) -> String {
    let s = format!("{id:09}");
    format!("{} {} {}", &s[..3], &s[3..6], &s[6..])
}

/// Wrap a datagram for relaying through the server.
pub fn relay_wrap(session: u32, inner: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.push(RELAY);
    out.extend_from_slice(&session.to_le_bytes());
    out.extend_from_slice(inner);
}

/// (session, inner datagram) of a relayed datagram.
pub fn relay_unwrap(b: &[u8]) -> Option<(u32, &[u8])> {
    if b.len() < RELAY_HEADER || b[0] != RELAY {
        return None;
    }
    Some((
        u32::from_le_bytes(b[1..5].try_into().ok()?),
        &b[RELAY_HEADER..],
    ))
}

pub fn v4(a: SocketAddr) -> Option<SocketAddrV4> {
    match a {
        SocketAddr::V4(v) => Some(v),
        SocketAddr::V6(v) => v
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddrV4::new(ip, v.port())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_messages() {
        let a: SocketAddrV4 = "203.0.113.7:40000".parse().unwrap();
        let l: Vec<SocketAddrV4> = vec![
            "192.168.1.20:5000".parse().unwrap(),
            "10.0.0.5:5000".parse().unwrap(),
        ];
        let msgs = [
            Msg::Register {
                pubkey: [1; 32],
                locals: l.clone(),
            },
            Msg::Registered {
                id: 482_913_077,
                public: a,
            },
            Msg::Connect {
                host_id: 7,
                pubkey: [2; 32],
                name: "laptop ü".into(),
                locals: l.clone(),
            },
            Msg::Incoming {
                session: 9,
                pubkey: [3; 32],
                name: "pc".into(),
                public: a,
                locals: vec![],
            },
            Msg::Answer {
                session: 9,
                accept: true,
            },
            Msg::Accepted {
                session: 9,
                pubkey: [4; 32],
                public: a,
                locals: l,
            },
            Msg::Denied { session: 9 },
            Msg::NotFound { host_id: 5 },
            Msg::Punch { session: 1 },
            Msg::PunchAck { session: 1 },
        ];
        for m in msgs {
            assert_eq!(Msg::decode(&m.encode()), Some(m.clone()), "{m:?}");
        }
        assert_eq!(Msg::decode(&[RENDEZVOUS, 99]), None);
        assert_eq!(
            Msg::decode(&Msg::Punch { session: 1 }.encode()[..4]),
            None,
            "truncated"
        );
    }

    #[test]
    fn relay_framing() {
        let mut out = Vec::new();
        relay_wrap(0xdead_beef, b"inner", &mut out);
        assert_eq!(relay_unwrap(&out), Some((0xdead_beef, &b"inner"[..])));
        assert_eq!(relay_unwrap(&[RELAY, 1]), None);
    }
}
