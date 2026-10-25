//! Rendezvous + relay server. Stateless across restarts, no accounts, no
//! database: hosts register their ID (derived from their public key) every
//! few seconds, clients ask for an ID, and the server introduces them. When
//! direct UDP hole punching fails, it relays the (end-to-end encrypted)
//! datagrams between the two NAT-mapped endpoints of a session.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use proto::rendezvous::{format_id, host_id, relay_unwrap, v4, Msg, RELAY};

#[derive(Parser)]
struct Args {
    /// UDP address to serve on.
    #[arg(long, default_value = "0.0.0.0:7000")]
    listen: SocketAddr,
}

/// A host that hasn't re-registered for this long is forgotten.
const HOST_TTL: Duration = Duration::from_secs(30);
/// Sessions (and their relay) expire after this long without traffic.
const SESSION_TTL: Duration = Duration::from_secs(120);

struct Host {
    pubkey: [u8; 32],
    addr: SocketAddr,
    locals: Vec<std::net::SocketAddrV4>,
    seen: Instant,
}

struct Session {
    host_id: u32,
    host: SocketAddr,
    client: SocketAddr,
    pubkey: [u8; 32],
    name: String,
    locals: Vec<std::net::SocketAddrV4>,
    answered: Option<bool>,
    active: Instant,
    relayed: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let socket = bind(args.listen)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    tracing::info!("signal server on {}", socket.local_addr()?);

    let mut hosts: HashMap<u32, Host> = HashMap::new();
    let mut sessions: HashMap<u32, Session> = HashMap::new();
    let mut next_session = rand_u32();
    let mut buf = vec![0u8; 65536];
    let mut last_sweep = Instant::now();

    loop {
        if last_sweep.elapsed() > Duration::from_secs(5) {
            hosts.retain(|_, h| h.seen.elapsed() < HOST_TTL);
            sessions.retain(|id, s| {
                let keep = s.active.elapsed() < SESSION_TTL;
                if !keep {
                    tracing::info!("session {id:08x} expired ({} datagrams relayed)", s.relayed);
                }
                keep
            });
            last_sweep = Instant::now();
        }
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        };
        let d = &buf[..n];

        if d.first() == Some(&RELAY) {
            // Forward to the other end of the session, prefix intact.
            let Some((sid, _)) = relay_unwrap(d) else {
                continue;
            };
            let Some(s) = sessions.get_mut(&sid) else {
                continue;
            };
            if s.answered != Some(true) {
                continue;
            }
            let to = if from == s.host {
                s.client
            } else if from == s.client {
                s.host
            } else {
                continue;
            };
            s.active = Instant::now();
            s.relayed += 1;
            let _ = socket.send_to(d, to);
            continue;
        }

        let Some(msg) = Msg::decode(d) else { continue };
        let reply = |m: Msg, to: SocketAddr| {
            let _ = socket.send_to(&m.encode(), to);
        };
        match msg {
            Msg::Register { pubkey, locals } => {
                let id = host_id(&pubkey);
                // An ID belongs to whoever holds its key; first come keeps it.
                if let Some(h) = hosts.get(&id) {
                    if h.pubkey != pubkey && h.seen.elapsed() < HOST_TTL {
                        tracing::warn!("ID {id} collision from {from}; ignoring");
                        continue;
                    }
                }
                let new = hosts.get(&id).is_none_or(|h| h.addr != from);
                hosts.insert(
                    id,
                    Host {
                        pubkey,
                        addr: from,
                        locals,
                        seen: Instant::now(),
                    },
                );
                if new {
                    tracing::info!("host {} registered from {from}", format_id(id));
                }
                if let Some(public) = v4(from) {
                    reply(Msg::Registered { id, public }, from);
                }
                // Keep a host's sessions pointing at its current address.
                for s in sessions.values_mut().filter(|s| s.host_id == id) {
                    s.host = from;
                }
            }
            Msg::Connect {
                host_id,
                pubkey,
                name,
                locals,
            } => {
                let Some(h) = hosts.get(&host_id) else {
                    reply(Msg::NotFound { host_id }, from);
                    continue;
                };
                // Retries of the same request reuse the session.
                let sid = match sessions
                    .iter()
                    .find(|(_, s)| s.client == from && s.host_id == host_id)
                {
                    Some((sid, _)) => *sid,
                    None => {
                        next_session = next_session.wrapping_add(0x9e37_79b9);
                        tracing::info!(
                            "session {next_session:08x}: {name:?} at {from} -> host {}",
                            format_id(host_id)
                        );
                        sessions.insert(
                            next_session,
                            Session {
                                host_id,
                                host: h.addr,
                                client: from,
                                pubkey,
                                name: name.clone(),
                                locals: locals.clone(),
                                answered: None,
                                active: Instant::now(),
                                relayed: 0,
                            },
                        );
                        next_session
                    }
                };
                let s = &sessions[&sid];
                match s.answered {
                    None => {
                        if let Some(public) = v4(s.client) {
                            reply(
                                Msg::Incoming {
                                    session: sid,
                                    pubkey: s.pubkey,
                                    name: s.name.clone(),
                                    public,
                                    locals: s.locals.clone(),
                                },
                                h.addr,
                            );
                        }
                    }
                    Some(true) => {
                        if let Some(public) = v4(h.addr) {
                            reply(
                                Msg::Accepted {
                                    session: sid,
                                    pubkey: h.pubkey,
                                    public,
                                    locals: h.locals.clone(),
                                },
                                from,
                            );
                        }
                    }
                    Some(false) => reply(Msg::Denied { session: sid }, from),
                }
            }
            Msg::Answer { session, accept } => {
                let Some(s) = sessions.get_mut(&session) else {
                    continue;
                };
                let Some(h) = hosts.get(&s.host_id) else {
                    continue;
                };
                if from != h.addr {
                    continue; // only the registered host may answer
                }
                s.answered = Some(accept);
                s.active = Instant::now();
                tracing::info!(
                    "session {session:08x} {}",
                    if accept { "accepted" } else { "denied" }
                );
                let m = if accept {
                    match v4(h.addr) {
                        Some(public) => Msg::Accepted {
                            session,
                            pubkey: h.pubkey,
                            public,
                            locals: h.locals.clone(),
                        },
                        None => continue,
                    }
                } else {
                    Msg::Denied { session }
                };
                reply(m, s.client);
            }
            _ => {}
        }
    }
}

/// Big kernel buffers: relayed IDR bursts arrive faster than one thread
/// forwards them one datagram at a time.
fn bind(addr: SocketAddr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    s.set_recv_buffer_size(8 << 20)?;
    s.set_send_buffer_size(8 << 20)?;
    s.bind(&addr.into())?;
    Ok(s.into())
}

fn rand_u32() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id().rotate_left(16)
}
