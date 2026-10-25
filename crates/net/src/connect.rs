//! Client side of the ID connect flow: ask the signal server for a host ID,
//! wait for the host's user to approve, find a path (hole punch first, the
//! server's relay if that fails) and authenticate with a Noise handshake.
//!
//! Returns the address to stream with: the host itself, or the server when
//! relayed. The socket is sealed from then on.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::crypto::{format_id, host_id, Handshake, Identity, Step, HANDSHAKE, SEALED};
use crate::socket::Link;
use crate::Socket;
use anyhow::{bail, Context, Result};
use proto::rendezvous::{Msg, RENDEZVOUS};
use proto::Control;

const CONNECT_EVERY: Duration = Duration::from_millis(500);
/// How long the host's user has to answer.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const PUNCH_EVERY: Duration = Duration::from_millis(50);
/// Give up on a direct path and relay after this long.
const PUNCH_TIMEOUT: Duration = Duration::from_millis(1500);
const HANDSHAKE_RESEND: Duration = Duration::from_millis(250);
/// A direct path that doesn't complete the handshake falls back to the relay.
const DIRECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn connect(
    socket: &Socket,
    server: SocketAddr,
    id: u32,
    force_relay: bool,
) -> Result<SocketAddr> {
    let identity = Identity::load_or_create("client")?;
    let name = device_name();
    let locals = crate::addr::local_candidates(socket.local_addr()?.port());
    socket.set_link(Link {
        server: Some(server),
        ..Link::default()
    });
    let mut buf = [0u8; 2048];

    // 1. Introduction and approval.
    tracing::info!("asking {server} for host {}", format_id(id));
    let connect = Msg::Connect {
        host_id: id,
        pubkey: identity.public,
        name: name.clone(),
        locals,
    }
    .encode();
    let started = Instant::now();
    let mut sent_at: Option<Instant> = None;
    let mut waiting_logged = false;
    let (session, host_key, mut candidates) = loop {
        if started.elapsed() > APPROVAL_TIMEOUT {
            bail!("no answer from the host within {APPROVAL_TIMEOUT:?}");
        }
        if sent_at.is_none_or(|t| t.elapsed() >= CONNECT_EVERY) {
            socket.send_to(&connect, server)?;
            sent_at = Some(Instant::now());
        }
        let Some((n, from)) = recv(socket, &mut buf)? else {
            continue;
        };
        if from != server {
            continue;
        }
        match Msg::decode(&buf[..n]) {
            Some(Msg::Incoming { .. }) | None => {}
            Some(Msg::NotFound { .. }) => bail!("no host with ID {} is online", format_id(id)),
            Some(Msg::Denied { .. }) => bail!("the host denied the connection"),
            Some(Msg::Accepted {
                session,
                pubkey,
                public,
                locals,
            }) => {
                // The ID is derived from the key, so a server can't swap hosts.
                if host_id(&pubkey) != id {
                    bail!(
                        "signal server returned a key that doesn't match ID {}",
                        format_id(id)
                    );
                }
                let mut c: Vec<SocketAddr> = vec![public.into()];
                c.extend(locals.into_iter().map(SocketAddr::from));
                c.dedup();
                break (session, pubkey, c);
            }
            Some(_) => {}
        }
        if !waiting_logged && started.elapsed() > Duration::from_secs(1) {
            tracing::info!("waiting for the host to approve…");
            waiting_logged = true;
        }
    };
    tracing::info!("approved; host candidates {candidates:?}");
    socket.set_link(Link {
        server: Some(server),
        relay: Some(session),
        ..Link::default()
    });

    // 2. Path: punch every candidate, take the first that answers.
    let punch = Msg::Punch { session }.encode();
    let started = Instant::now();
    let mut punched_at: Option<Instant> = None;
    let mut path = force_relay.then_some(server);
    while path.is_none() {
        if started.elapsed() > PUNCH_TIMEOUT {
            tracing::info!("no direct path; relaying through {server}");
            path = Some(server);
            break;
        }
        if punched_at.is_none_or(|t| t.elapsed() >= PUNCH_EVERY) {
            for &c in &candidates {
                let _ = socket.send_to(&punch, c);
            }
            punched_at = Some(Instant::now());
        }
        let Some((n, from)) = recv(socket, &mut buf)? else {
            continue;
        };
        match Msg::decode(&buf[..n]) {
            Some(Msg::Punch { session: s }) | Some(Msg::PunchAck { session: s })
                if s == session && from != server =>
            {
                let _ = socket.send_to(&Msg::PunchAck { session }.encode(), from);
                tracing::info!("direct path to {from} after {:?}", started.elapsed());
                path = Some(from);
            }
            _ => {}
        }
    }
    let mut path = path.unwrap();
    candidates.retain(|&c| c != path);

    // 3. Noise_XX: authenticates the host key (bound to the ID) and ours.
    let started = Instant::now();
    let mut path_since = Instant::now();
    let mut ini = Handshake::initiator(&identity, name.as_bytes())?;
    let mut last = ini.start()?;
    let mut sent_at = Instant::now();
    socket.send_to(&last, path)?;
    let session_keys = loop {
        if started.elapsed() > HANDSHAKE_TIMEOUT {
            bail!("handshake with the host timed out");
        }
        if path != server && path_since.elapsed() > DIRECT_HANDSHAKE_TIMEOUT {
            tracing::warn!("direct path {path} stopped answering; relaying");
            path = server;
            path_since = Instant::now();
            ini = Handshake::initiator(&identity, name.as_bytes())?;
            last = ini.start()?;
            sent_at = Instant::now() - HANDSHAKE_RESEND;
        }
        if sent_at.elapsed() >= HANDSHAKE_RESEND {
            socket.send_to(&last, path)?;
            sent_at = Instant::now();
        }
        let Some((n, from)) = recv(socket, &mut buf)? else {
            continue;
        };
        let d = &buf[..n];
        if d[0] == RENDEZVOUS {
            if let Some(Msg::Punch { session: s }) = Msg::decode(d) {
                if s == session {
                    let _ = socket.send_to(&Msg::PunchAck { session }.encode(), from);
                }
            }
            continue;
        }
        if d[0] != HANDSHAKE || from != path {
            continue;
        }
        match ini.on_message(d)? {
            Step::Done(Some(m2)) => {
                last = m2;
                socket.send_to(&last, path)?;
                break ini.into_session()?;
            }
            Step::Send(m) => {
                last = m;
                socket.send_to(&last, path)?;
            }
            _ => {}
        }
    };
    if session_keys.remote != host_key {
        bail!("host authenticated with a different key than the server announced");
    }
    socket.set_link(Link {
        server: Some(server),
        relay: Some(session),
        session: Some(Arc::new(session_keys)),
        require_session: true,
    });

    // 4. Our last handshake message may be lost: repeat it with a sealed
    // Hello until the host answers with anything sealed.
    let started = Instant::now();
    let hello = Control::Hello.datagram(0);
    let mut sent_at = Instant::now() - HANDSHAKE_RESEND;
    loop {
        if started.elapsed() > HANDSHAKE_TIMEOUT {
            bail!("host never confirmed the session");
        }
        if sent_at.elapsed() >= HANDSHAKE_RESEND {
            socket.send_to(&last, path)?;
            socket.send_to(&hello, path)?;
            sent_at = Instant::now();
        }
        if let Some((n, from)) = recv(socket, &mut buf)? {
            // Opened by the socket: anything that isn't rendezvous or
            // handshake passed authentication.
            if from == path && n > 0 && !matches!(buf[0], RENDEZVOUS | HANDSHAKE | SEALED) {
                break;
            }
        }
    }
    tracing::info!(
        "connected to {} via {} in {:.0} ms total",
        format_id(id),
        if path == server {
            "relay"
        } else {
            "direct path"
        },
        started.elapsed().as_secs_f64() * 1e3
    );
    Ok(path)
}

/// Non-blocking receive; None when nothing is waiting (sleeps 1 ms).
fn recv(socket: &Socket, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>> {
    match socket.recv_from(buf) {
        Ok(r) if r.0 > 0 => Ok(Some(r)),
        Ok(_) => Ok(None),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            std::thread::sleep(Duration::from_millis(1));
            Ok(None)
        }
        Err(e) => Err(e).context("recv"),
    }
}

/// This machine's name, shown in the host's approval popup.
fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "client".into())
}
