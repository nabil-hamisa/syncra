//! Headless stream receiver for network tests on machines without the
//! Windows client (e.g. a Linux laptop on a phone hotspot).
//!
//! It connects exactly like the client (`--id` through the signal server, or
//! `--connect` directly), reassembles and FEC-repairs the video, and drives
//! the host's congestion control and loss recovery with the same feedback,
//! NACKs and loss reports, but doesn't decode. Every second it logs frame
//! rate, bitrate, losses, RTT and the one-way network delay above the
//! fastest frame seen (clocks differ between machines, so absolute one-way
//! delay is unknown; the variation is what queues and jitter add).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use net::feedback::FeedbackBuilder;
use net::{Event, Reassembler, Socket};
use proto::{clock, Channel, Control, Header};

#[derive(Parser)]
struct Args {
    /// Host to stream from directly (unencrypted), e.g. 192.168.1.6:9000.
    #[arg(long, conflicts_with = "id")]
    connect: Option<SocketAddr>,

    /// Host ID to connect to through the signal server.
    #[arg(long, requires = "signal")]
    id: Option<String>,

    /// Signal server "host:port" for --id.
    #[arg(long)]
    signal: Option<String>,

    /// Always relay through the signal server.
    #[arg(long)]
    force_relay: bool,

    /// Simulate the probe→host link, e.g. "delay=20ms,loss=1%".
    #[arg(long)]
    sim: Option<net::sim::SimConfig>,

    /// Stop after this many seconds (0 = until killed).
    #[arg(long, default_value_t = 30)]
    seconds: u64,
}

const KEEPALIVE: Duration = Duration::from_millis(500);
const NACK_MAX_RTT_US: u32 = 20_000;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let mut socket = Socket::bind("0.0.0.0:0".parse().unwrap())?;
    if let Some(sim) = args.sim {
        socket.set_sim(sim)?;
    }
    // Short timeout instead of busy-polling: the probe measures the network,
    // and a 1 ms wake-up granularity is far below what it reports.
    socket.set_read_timeout(Some(Duration::from_millis(1)))?;
    let host = match (&args.connect, &args.id, &args.signal) {
        (Some(h), _, _) => *h,
        (None, Some(id), Some(signal)) => {
            let id = net::crypto::parse_id(id).context("--id: expected 9 digits")?;
            let server = net::addr::resolve_v4(signal)?;
            net::connect::connect(&socket, server, id, args.force_relay)?
        }
        _ => anyhow::bail!("pass --connect HOST:PORT or --id ID --signal HOST:PORT"),
    };
    run(&args, &socket, host)
}

/// Loss recovery in progress (mirrors the client): frames from `first` on
/// are tainted until a keyframe, or a recovery frame covering `last`.
struct Recovering {
    first: u32,
    last: u32,
    since: Instant,
    reported: Instant,
}

#[derive(Default)]
struct Window {
    frames: u32,
    bytes: u64,
    lost: u32,
    recoveries: Vec<u32>,
    /// Last-datagram send → frame complete, raw (clock offset included).
    delays: Vec<i64>,
    max_gap: Duration,
}

fn run(args: &Args, socket: &Socket, host: SocketAddr) -> Result<()> {
    tracing::info!("streaming from {host}");
    let mut seq = 0u32;
    let send = |c: Control, seq: &mut u32| {
        let _ = socket.send_to(&c.datagram(*seq), host);
        *seq += 1;
    };
    send(Control::Hello, &mut seq);

    let mut reasm = Reassembler::new();
    let mut events = Vec::new();
    let mut feedback = FeedbackBuilder::new(clock::now_us());
    let mut buf = vec![0u8; 65536];
    let mut rtt = Duration::ZERO;
    let mut rtts: Vec<u32> = Vec::new();
    let mut waiting_keyframe = true;
    let mut recovering: Option<Recovering> = None;
    let mut last_keepalive = Instant::now();
    let mut last_keyframe_req = Instant::now();
    let mut last_hello = Instant::now();
    let mut got_any = false;
    let mut last_frame_at: Option<Instant> = None;
    let mut win = Window::default();
    let mut win_start = Instant::now();
    // Fastest frame so far; delays are reported above it.
    let mut min_delay = i64::MAX;
    let mut all_delays: Vec<i64> = Vec::new();
    let mut totals = (0u64, 0u64); // frames, lost
    let started = Instant::now();

    while args.seconds == 0 || started.elapsed().as_secs() < args.seconds {
        if !got_any && last_hello.elapsed() > Duration::from_millis(250) {
            send(Control::Hello, &mut seq);
            last_hello = Instant::now();
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == host => {
                got_any = true;
                if let Some((h, payload)) = Header::read(&buf[..n]) {
                    match h.channel() {
                        Some(Channel::Video) => {
                            win.bytes += n as u64;
                            feedback.on_video(&h, n, clock::now_us());
                            reasm.push(&h, payload, &mut events);
                        }
                        Some(Channel::Control) => {
                            if let Some(Control::Ping { t_us, rtt_us }) = Control::decode(payload) {
                                send(Control::Pong { t_us }, &mut seq);
                                rtt = Duration::from_micros(rtt_us as u64);
                                if rtt_us > 0 {
                                    rtts.push(rtt_us);
                                }
                                let wait = (rtt_us > 0 && rtt_us < NACK_MAX_RTT_US)
                                    .then(|| Duration::from_micros(rtt_us as u64 * 3 / 2 + 1_000));
                                reasm.set_nack_wait(wait);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => return Err(e).context("recv"),
        }

        reasm.poll(&mut events);
        for ev in events.drain(..) {
            match ev {
                Event::Nack {
                    frame_id,
                    chunk_id,
                    base,
                    mask,
                } => send(
                    Control::Nack {
                        frame_id,
                        chunk_id,
                        base,
                        mask,
                    },
                    &mut seq,
                ),
                Event::FrameLost { first, last } => {
                    win.lost += last.wrapping_sub(first).wrapping_add(1);
                    send(Control::FrameLost { frame_id: first }, &mut seq);
                    let now = Instant::now();
                    match recovering.as_mut() {
                        Some(r) => {
                            r.last = last;
                            r.reported = now;
                        }
                        None => {
                            recovering = Some(Recovering {
                                first,
                                last,
                                since: now,
                                reported: now,
                            })
                        }
                    }
                }
                Event::Chunk(c) => {
                    if waiting_keyframe {
                        if !(c.keyframe && c.chunk_id == 0) {
                            continue;
                        }
                        waiting_keyframe = false;
                    }
                    if let Some(r) = &recovering {
                        let clean = c.keyframe
                            || (c.recovery && !net::video::frame_before(c.lost_acked, r.last));
                        if c.chunk_id == 0 && clean {
                            win.recoveries.push(r.since.elapsed().as_millis() as u32);
                            recovering = None;
                        }
                    }
                    if !c.last {
                        continue;
                    }
                    feedback.on_frame_complete(c.frame_id);
                    if recovering.is_some() {
                        continue; // the client would hide this frame
                    }
                    let now = Instant::now();
                    if let Some(t) = last_frame_at {
                        win.max_gap = win.max_gap.max(now - t);
                    }
                    last_frame_at = Some(now);
                    win.frames += 1;
                    let d = clock::now_us() as i64 - c.send_us as i64;
                    min_delay = min_delay.min(d);
                    win.delays.push(d);
                }
            }
        }

        if let Some(f) = feedback.poll(clock::now_us()) {
            if got_any {
                send(Control::Feedback(f), &mut seq);
            }
        }
        if let Some(r) = recovering.as_mut() {
            if r.since.elapsed() > Duration::from_millis(300).max(rtt * 4) {
                if last_keyframe_req.elapsed() > Duration::from_millis(100).max(rtt * 2) {
                    send(Control::RequestKeyframe, &mut seq);
                    last_keyframe_req = Instant::now();
                }
            } else if r.reported.elapsed() > Duration::from_millis(30) {
                send(Control::FrameLost { frame_id: r.first }, &mut seq);
                r.reported = Instant::now();
            }
        }
        if waiting_keyframe
            && got_any
            && last_keyframe_req.elapsed() > Duration::from_millis(100).max(rtt * 2)
        {
            send(Control::RequestKeyframe, &mut seq);
            last_keyframe_req = Instant::now();
        }
        if got_any && last_keepalive.elapsed() > KEEPALIVE {
            send(Control::KeepAlive, &mut seq);
            last_keepalive = Instant::now();
        }

        if win_start.elapsed() >= Duration::from_secs(1) {
            let secs = win_start.elapsed().as_secs_f64();
            let mut above: Vec<i64> = win.delays.iter().map(|d| d - min_delay).collect();
            above.sort_unstable();
            let pct = |v: &[i64], p: usize| v.get(v.len() * p / 100).copied().unwrap_or(0);
            tracing::info!(
                "{:.0} fps | {:.1} Mbps | net delay above min p50 {:.2} p95 {:.2} max {:.2} ms | rtt {:.1} ms | lost {} | fec-fixed {} | nacks {} | recov {:?} ms | gap {:.1} ms",
                win.frames as f64 / secs,
                win.bytes as f64 * 8.0 / secs / 1e6,
                pct(&above, 50) as f64 / 1e3,
                pct(&above, 95) as f64 / 1e3,
                above.last().copied().unwrap_or(0) as f64 / 1e3,
                rtt.as_secs_f64() * 1e3,
                win.lost,
                reasm.stats.fec_recovered,
                reasm.stats.nacks,
                win.recoveries,
                win.max_gap.as_secs_f64() * 1e3,
            );
            totals.0 += win.frames as u64;
            totals.1 += win.lost as u64;
            // Skip the first two seconds (connect, first keyframe, rate ramp).
            if started.elapsed() > Duration::from_secs(3) {
                all_delays.extend_from_slice(&win.delays);
            }
            win = Window::default();
            win_start = Instant::now();
        }
    }
    send(Control::Bye, &mut seq);

    let mut above: Vec<i64> = all_delays.iter().map(|d| d - min_delay).collect();
    above.sort_unstable();
    rtts.sort_unstable();
    let pct = |v: &[i64], p: usize| v.get(v.len() * p / 100).copied().unwrap_or(0) as f64 / 1e3;
    let rpct = |p: usize| rtts.get(rtts.len() * p / 100).copied().unwrap_or(0) as f64 / 1e3;
    tracing::info!(
        "summary: {} frames, {} lost | net delay above min p50 {:.2} p95 {:.2} p99 {:.2} ms | rtt p50 {:.1} p95 {:.1} ms | fec-fixed {} nacks {}",
        totals.0,
        totals.1,
        pct(&above, 50),
        pct(&above, 95),
        pct(&above, 99),
        rpct(50),
        rpct(95),
        reasm.stats.fec_recovered,
        reasm.stats.nacks,
    );
    Ok(())
}
