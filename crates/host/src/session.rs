//! Client session: the receive thread and state shared with the encode loop.
//!
//! The receive thread blocks on the socket and injects keyboard and mouse
//! input the moment a datagram arrives, so input latency never depends on the
//! capture/encode cadence. Gamepad reports also go out from here (plugging a
//! new virtual pad runs on a worker thread). The encode loop only sends.
//!
//! A client that goes silent (no datagram, not even a keepalive, for
//! `PEER_TIMEOUT`) is dropped: everything it pressed is released, its virtual
//! pads are unplugged and streaming stops.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use net::cc::{CcConfig, CongestionController};
use net::history::SendHistory;
use net::video::frame_before;
use net::Socket;
use proto::input::{decode_input, GamepadState, InputEvent, Sequenced};
use proto::{clock, Channel, Control, Header, MAX_DATAGRAM};
use windows::Win32::Foundation::RECT;

use crate::gamepad::Gamepads;
use crate::input_inject::Injector;
use crate::rendezvous::{self, Rendezvous};
use crate::stats::Series;

const PEER_TIMEOUT: Duration = Duration::from_secs(3);

const PING_EVERY: Duration = Duration::from_millis(100);

pub struct Shared {
    pub peer: Mutex<Option<SocketAddr>>,
    pub want_idr: AtomicBool,
    pub want_cursor_shape: AtomicBool,
    pub stop: AtomicBool,
    /// Rate and FEC decisions, fed by client feedback on `net-rx`, read by
    /// the encode loop once per frame.
    pub cc: Mutex<CongestionController>,
    /// Newest frame the client reported lost and the encode loop hasn't
    /// handled yet (reference invalidation or IDR).
    pub lost_pending: Mutex<Option<u32>>,
    /// Newest frame excluded from references after a loss (u32::MAX = none
    /// yet). Sent in every video header as `lost_acked`.
    pub lost_acked: AtomicU32,
    /// Recently sent video, for NACK resends.
    pub history: SendHistory,
}

impl Shared {
    pub fn new(cc: CcConfig) -> Self {
        Shared {
            peer: Mutex::new(None),
            want_idr: AtomicBool::new(false),
            want_cursor_shape: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            cc: Mutex::new(CongestionController::new(cc, Instant::now())),
            lost_pending: Mutex::new(None),
            lost_acked: AtomicU32::new(u32::MAX),
            history: SendHistory::new(),
        }
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        *self.peer.lock().unwrap()
    }
}

pub fn spawn_receiver(
    socket: Socket,
    shared: Arc<Shared>,
    monitor: RECT,
    signal: Option<rendezvous::Config>,
) -> Result<std::thread::JoinHandle<()>> {
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let rv = signal.map(|c| Rendezvous::new(c, &socket)).transpose()?;
    Ok(std::thread::Builder::new()
        .name("net-rx".into())
        .spawn(move || {
            crate::rt::mmcss_current_thread(crate::rt::MmcssTask::Games);
            Receiver::new(socket, shared, monitor, rv).run();
        })?)
}

struct Receiver {
    socket: Socket,
    shared: Arc<Shared>,
    injector: Injector,
    gamepads: Gamepads,
    last_heard: Instant,
    latency: Series,
    report_at: Instant,
    ping_at: Instant,
    /// ID connect flow (`--signal`); None for direct `--listen` streaming.
    rv: Option<Rendezvous>,
}

impl Receiver {
    fn new(socket: Socket, shared: Arc<Shared>, monitor: RECT, rv: Option<Rendezvous>) -> Self {
        let gamepads = Gamepads::spawn(socket.try_clone().ok(), shared.clone());
        Receiver {
            socket,
            shared,
            injector: Injector::new(monitor),
            gamepads,
            last_heard: Instant::now(),
            latency: Series::default(),
            report_at: Instant::now() + Duration::from_secs(1),
            ping_at: Instant::now(),
            rv,
        }
    }

    fn run(mut self) {
        let mut events: Vec<Sequenced> = Vec::with_capacity(16);
        let mut buf = [0u8; MAX_DATAGRAM];

        while !self.shared.stop.load(Ordering::Relaxed) {
            self.housekeeping();
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue, // ICMP from a gone client
                Err(e) => {
                    tracing::error!("recv failed: {e}");
                    return;
                }
            };
            if Rendezvous::wants(&buf[..n]) {
                if let Some(rv) = self.rv.as_mut() {
                    if let Some(rendezvous::Event::Connected { path, name }) =
                        rv.on_datagram(&self.socket, &buf[..n], from)
                    {
                        // A newly authenticated client replaces the current
                        // one; it takes over with its first (sealed) Hello.
                        tracing::info!("{name:?} will stream over {path}");
                        self.forget_peer();
                    }
                }
                continue;
            }
            let Some((h, payload)) = Header::read(&buf[..n]) else {
                continue;
            };
            let from_peer = self.shared.peer() == Some(from);
            if from_peer {
                self.last_heard = Instant::now();
            }

            match h.channel() {
                Some(Channel::Input) if from_peer => {
                    events.clear();
                    if decode_input(payload, &mut events).is_some() {
                        self.input(&events, h.send_us);
                    }
                }
                Some(Channel::Control) => self.control(Control::decode(payload), from, from_peer),
                _ => {}
            }
        }
    }

    fn input(&mut self, events: &[Sequenced], send_us: u64) {
        let new = self.injector.apply(events);
        if new.is_empty() {
            return;
        }
        // Gamepad events are full states: forward only the newest new one per pad.
        let mut newest: [Option<GamepadState>; 4] = [None; 4];
        let mut release = false;
        for e in new {
            match e.event {
                InputEvent::Gamepad(g) if (g.index as usize) < newest.len() => {
                    newest[g.index as usize] = Some(g)
                }
                InputEvent::ReleaseAll => release = true,
                _ => {}
            }
        }
        if release {
            self.gamepads.neutral();
        }
        for g in newest.into_iter().flatten() {
            self.gamepads.update(g);
        }
        // Same-machine clocks: client send → keyboard/mouse injected.
        self.latency
            .push(clock::now_us().saturating_sub(send_us) as u32);
    }

    fn control(&mut self, msg: Option<Control>, from: SocketAddr, from_peer: bool) {
        match msg {
            Some(Control::Hello) => {
                let mut peer = self.shared.peer.lock().unwrap();
                if *peer != Some(from) {
                    tracing::info!("client {from} connected");
                    crate::panel::set_has_client(true);
                    if let Some(rv) = self.rv.as_mut() {
                        rv.connected = true;
                    }
                    self.injector.reset();
                    self.gamepads.disconnect_all();
                }
                *peer = Some(from);
                drop(peer);
                self.last_heard = Instant::now();
                self.shared.want_idr.store(true, Ordering::Relaxed);
                self.shared.want_cursor_shape.store(true, Ordering::Relaxed);
            }
            Some(Control::RequestKeyframe) if from_peer => {
                self.shared.want_idr.store(true, Ordering::Relaxed)
            }
            Some(Control::RequestCursorShape) if from_peer => {
                self.shared.want_cursor_shape.store(true, Ordering::Relaxed)
            }
            Some(Control::Bye) if from_peer => {
                tracing::info!("client {from} disconnected");
                self.drop_peer();
            }
            Some(Control::Feedback(f)) if from_peer => {
                self.shared
                    .cc
                    .lock()
                    .unwrap()
                    .on_feedback(&f, Instant::now());
            }
            Some(Control::Pong { t_us }) if from_peer => {
                let rtt = clock::now_us().saturating_sub(t_us).min(u32::MAX as u64) as u32;
                self.shared.cc.lock().unwrap().on_rtt(rtt);
            }
            Some(Control::Nack {
                frame_id,
                chunk_id,
                base,
                mask,
            }) if from_peer => {
                self.shared
                    .history
                    .resend(&self.socket, from, frame_id, chunk_id, base, mask);
            }
            Some(Control::FrameLost { frame_id: first }) if from_peer => {
                // Reports of losses already handled (repeats) are ignored; of
                // several pending ones the oldest matters, since everything
                // after it is tainted.
                let acked = self.shared.lost_acked.load(Ordering::Acquire);
                let handled = acked != u32::MAX && !frame_before(acked, first);
                let mut lost = self.shared.lost_pending.lock().unwrap();
                if !handled && lost.is_none_or(|l| frame_before(first, l)) {
                    *lost = Some(first);
                }
            }
            _ => {}
        }
    }

    fn drop_peer(&mut self) {
        self.forget_peer();
        if let Some(rv) = self.rv.as_mut() {
            rv.on_disconnect(&self.socket);
        }
    }

    fn forget_peer(&mut self) {
        *self.shared.peer.lock().unwrap() = None;
        crate::panel::set_has_client(false);
        self.injector.reset();
        self.gamepads.disconnect_all();
    }

    /// Peer timeout and the once-per-second input latency report.
    fn housekeeping(&mut self) {
        if let Some(rv) = self.rv.as_mut() {
            rv.tick(&self.socket);
        }
        if crate::panel::take_disconnect() {
            if let Some(peer) = self.shared.peer() {
                // Sealed while the session still exists; repeated against loss.
                for _ in 0..3 {
                    let _ = self.socket.send_to(&Control::Bye.datagram(0), peer);
                }
                tracing::info!("disconnected client {peer}");
                self.drop_peer();
            }
        }
        if let Some(peer) = self.shared.peer() {
            if self.last_heard.elapsed() > PEER_TIMEOUT {
                tracing::warn!("client {peer} silent for {PEER_TIMEOUT:?}; dropping it");
                self.drop_peer();
            } else if self.ping_at.elapsed() >= PING_EVERY {
                self.ping_at = Instant::now();
                let rtt_us = self
                    .shared
                    .cc
                    .lock()
                    .unwrap()
                    .snapshot(Instant::now())
                    .rtt_us;
                let ping = Control::Ping {
                    t_us: clock::now_us(),
                    rtt_us,
                };
                let _ = self.socket.send_to(&ping.datagram(0), peer);
            }
        }
        if Instant::now() < self.report_at {
            return;
        }
        self.report_at = Instant::now() + Duration::from_secs(1);
        if self.shared.peer().is_some() {
            let s = self.shared.cc.lock().unwrap().snapshot(Instant::now());
            let sim = self.socket.sim().map(|l| l.stats());
            tracing::info!(
                "cc: target {:.1} Mbps | recv {:.1} Mbps | qdelay {:.1} ms | loss {:.1}% | fec {:.0}% | rtt {:.1} ms{}",
                s.rate_bps / 1e6,
                s.recv_bps / 1e6,
                s.qdelay_us as f64 / 1e3,
                s.loss * 100.0,
                s.fec_ratio * 100.0,
                s.rtt_us as f64 / 1e3,
                sim.map_or(String::new(), |st| format!(
                    " | sim sent {} rnd-drop {} queue-drop {}",
                    st.sent, st.dropped_random, st.dropped_queue
                ))
            );
        }
        if let Some((p50, p95, max)) = self.latency.take() {
            tracing::info!(
                "input: client send → injected p50 {:.3} ms p95 {:.3} ms max {:.3} ms",
                p50 as f64 / 1e3,
                p95 as f64 / 1e3,
                max as f64 / 1e3
            );
        }
    }
}
