//! Host: desktop capture → NVENC → network, input injection, audio. The
//! `host` binary is a thin wrapper around [`run_main`]; the combined
//! `syncra` app runs [`share`] next to the client.

mod audio_send;
mod capture;
mod encode;
mod gamepad;
mod input_inject;
pub mod panel;
mod pattern;
mod rendezvous;
pub mod rt;
mod scale;
mod session;
mod stats;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;

use capture::Acquired;
use encode::{Encoder, EncoderConfig};
use net::cc::CcConfig;
use net::{ChunkMeta, Socket, VideoSender};
use proto::input::{CursorShape, CursorState};
use proto::{clock, Channel, HEADER_LEN};
use session::Shared;
use stats::Stats;

/// Capture the desktop and encode it with NVENC at minimum latency.
#[derive(Parser, Debug)]
#[command(version)]
pub struct Args {
    /// List adapters and monitors, then exit.
    #[arg(long)]
    list: bool,

    /// Monitor index on the NVIDIA adapter.
    #[arg(long, default_value_t = 0)]
    output: u32,

    /// Stream to a client: bind this UDP address and wait for a Hello.
    #[arg(long)]
    listen: Option<SocketAddr>,

    /// Signal server "host:port": get an ID that clients connect with
    /// (with your approval), end-to-end encrypted. Binds --listen, or any port.
    #[arg(long)]
    signal: Option<String>,

    /// Accept every device that asks, without the approval popup (tests).
    #[arg(long, requires = "signal")]
    auto_approve: bool,

    /// Write the raw Annex-B H.264 stream to this file.
    #[arg(long)]
    dump: Option<PathBuf>,

    /// Stop after this many encoded frames (0 = run forever). Defaults to
    /// forever when streaming, 600 otherwise.
    #[arg(long)]
    frames: Option<u64>,

    /// Target frame rate (also sets the VBV size).
    #[arg(long, default_value_t = 120)]
    fps: u32,

    /// Bitrate in Mbps.
    #[arg(long, default_value_t = 30.0)]
    bitrate: f64,

    /// Slices per picture (sub-frame streaming granularity).
    #[arg(long, default_value_t = 4)]
    slices: u32,

    /// Disable sub-frame readback (wait for whole frame).
    #[arg(long)]
    no_subframe: bool,

    /// Disable UDP segmentation offload.
    #[arg(long)]
    no_uso: bool,

    /// Simulate the host→client link, e.g. "loss=2%,burst=3,delay=20ms,jitter=2ms,bw=20mbps,queue=50ms".
    #[arg(long)]
    sim: Option<net::sim::SimConfig>,

    /// FEC: "auto" (adapts to measured loss), "off", or a fixed parity ratio like 0.2.
    #[arg(long, default_value = "auto")]
    fec: String,

    /// Lowest bitrate congestion control may pick, in Mbps.
    #[arg(long, default_value_t = 0.5)]
    min_bitrate: f64,

    /// Re-encode the last frame when the desktop is idle, to benchmark at a constant rate.
    #[arg(long)]
    force_fps: bool,

    /// Stream synthetic scrolling noise at --fps instead of the desktop
    /// (worst-case content that keeps NVENC at its full bitrate).
    #[arg(long)]
    test_pattern: bool,

    /// Force an IDR every N frames (worst-case encode benchmark; 0 = never).
    #[arg(long, default_value_t = 0)]
    idr_every: u64,

    /// Encode one frame of a solid "r,g,b" (0-1) color to --dump and exit.
    /// Used to determine NVENC's RGB→YUV matrix and range.
    #[arg(long, hide = true, value_delimiter = ',')]
    color_probe: Option<Vec<f32>>,

    /// Open the host window (what running without arguments does), e.g. to
    /// combine it with other options.
    #[arg(long)]
    pub gui: bool,

    /// Don't stream audio.
    #[arg(long)]
    no_audio: bool,

    /// Playback device to capture (part of its name; default device if omitted).
    #[arg(long)]
    audio_device: Option<String>,

    /// Play a 440 Hz tone on the captured device (audio tests).
    #[arg(long)]
    test_tone: bool,

    /// List playback devices and exit.
    #[arg(long)]
    list_audio: bool,

    /// Always encode at the capture resolution (no adaptive downscaling on
    /// slow links).
    #[arg(long)]
    no_adaptive_res: bool,
}

const RING: usize = 2;

/// Adaptive resolution: the video bitrate a level needs, in bits per pixel
/// at the configured frame rate (at 120 fps: 1080p ≈ 10 Mbps, 720p ≈ 4.4,
/// 540p ≈ 2.5, 360p ≈ 1.1). Resolution drops before frame rate does; the
/// fps divisor only acts when even the smallest level overshoots.
const RES_BITS_PER_PIXEL: f64 = 0.04;
/// Drop a level after the target stays below its need this long...
const RES_DOWN_AFTER: Duration = Duration::from_secs(1);
/// ...raise one after the target exceeds the next level's need by
/// `RES_UP_MARGIN` this long...
const RES_UP_AFTER: Duration = Duration::from_secs(3);
const RES_UP_MARGIN: f64 = 1.3;
/// ...and never change twice within this (each change costs an IDR).
const RES_HOLD: Duration = Duration::from_secs(2);
/// During slow start, steps up need only this gap: the session opens at the
/// level the starting rate supports and climbs as the probe finds headroom,
/// so a slow link never sees a full-size IDR and a LAN reaches full size
/// within about a second.
const RES_STARTUP_STEP: Duration = Duration::from_millis(300);

/// Streaming state owned by the encode loop. Receiving (control and input)
/// happens on the `net-rx` thread; see `session`.
struct Stream {
    socket: Socket,
    video: VideoSender,
    cursor_shapes: VideoSender,
    shared: Arc<Shared>,
    cursor_seq: u32,
    shape_msg_id: u32,
    last_shape: Option<CursorShape>,
    last_cursor: Option<CursorState>,
    cursor_pos: (u16, u16),
    last_rate_change: Instant,
    last_idr: Instant,
    /// Wire bytes / video bytes, smoothed over ~1 s of frames.
    wire_overhead: f64,
    /// Encode every Nth captured frame (1, 2 or 4): when even the encoder's
    /// smallest frames exceed what the link carries, a lower frame rate beats
    /// a stream that drowns its own bottleneck.
    fps_divisor: u32,
    fps_cut_at_target: f64,
    candidate_frames: u64,
    rate_window_start: Instant,
    rate_window_bytes: usize,
    rate_window_video: usize,
    /// Consecutive windows in which the encoder overshot its own bitrate.
    overshoot_windows: u32,
    /// Adaptive resolution: index into the ladder (0 = capture size).
    res_level: usize,
    /// The first pick sets the level outright (no hysteresis).
    res_started: bool,
    status_frames: u32,
    status_at: Instant,
    res_changed: Instant,
    res_below: Option<Instant>,
    res_above: Option<Instant>,
}

impl Stream {
    fn bind(
        addr: SocketAddr,
        uso: bool,
        sim: Option<net::sim::SimConfig>,
        cc: CcConfig,
        monitor: windows::Win32::Foundation::RECT,
        signal: Option<rendezvous::Config>,
    ) -> Result<Self> {
        let mut socket = Socket::bind(addr)?;
        if uso {
            socket.enable_uso()?;
        }
        if let Some(sim) = sim {
            socket.set_sim(sim)?;
        }
        tracing::info!(
            "listening on {} (USO {})",
            socket.local_addr()?,
            if socket.uso_enabled() { "ON" } else { "off" }
        );
        let shared = Arc::new(Shared::new(cc));
        session::spawn_receiver(socket.try_clone()?, shared.clone(), monitor, signal)?;
        Ok(Stream {
            socket,
            video: VideoSender::new(),
            cursor_shapes: VideoSender::new(),
            shared,
            cursor_seq: 0,
            shape_msg_id: 0,
            last_shape: None,
            last_cursor: None,
            cursor_pos: (0, 0),
            last_rate_change: Instant::now(),
            last_idr: Instant::now() - Duration::from_secs(1),
            wire_overhead: 1.1,
            fps_divisor: 1,
            fps_cut_at_target: 0.0,
            candidate_frames: 0,
            rate_window_start: Instant::now(),
            rate_window_bytes: 0,
            rate_window_video: 0,
            overshoot_windows: 0,
            res_level: 0,
            res_started: false,
            status_frames: 0,
            status_at: Instant::now(),
            res_changed: Instant::now(),
            res_below: None,
            res_above: None,
        })
    }

    /// Per-frame transport decisions: apply the congestion controller's
    /// bitrate to NVENC, pick the FEC ratio, and handle reported losses by
    /// invalidating every tainted reference (or forcing an IDR when the taint
    /// reaches past the encoder's reference window).
    /// Returns (fec_ratio, recovery, force_idr).
    fn before_encode(&mut self, encoder: &mut Encoder) -> Result<(f32, bool, bool)> {
        let now = Instant::now();
        let (target, fec_ratio) = {
            let cc = self.shared.cc.lock().unwrap();
            (cc.rate_bps(), cc.fec_ratio(now))
        };
        // The target is for the wire: leave room for headers and parity
        // (measured, including the per-chunk parity floors) on top of the
        // video so everything together fits the link.
        // NVENC budgets each frame for the configured 120 fps; when only every
        // Nth frame is encoded, scale up so the per-frame budget (and the wire
        // rate) matches the target.
        let video_target = target / self.wire_overhead.max(1.0) * self.fps_divisor as f64;
        let current = encoder.bitrate_bps() as f64;
        let change = (video_target - current) / current;
        // Cuts apply immediately; raises at most every 100 ms.
        if change <= -0.03
            || (change >= 0.05 && now - self.last_rate_change >= Duration::from_millis(100))
        {
            encoder.set_bitrate(video_target as u32)?;
            self.last_rate_change = now;
        }

        let (mut recovery, mut force_idr) = (false, false);
        // Take the report and release the lock first: a guard in the `if let`
        // scrutinee would live through the block, which re-locks to re-queue.
        let reported = self.shared.lost_pending.lock().unwrap().take();
        if let Some(first) = reported {
            let next = encoder.next_frame_index();
            let newest = next.wrapping_sub(1);
            let tainted = next.wrapping_sub(first);
            let acked = self.shared.lost_acked.load(Ordering::Acquire);
            let handled = acked != u32::MAX && !net::video::frame_before(acked, first);
            if !handled {
                let mut ok = tainted <= encoder.ref_frames() && encoder.supports_ref_invalidation();
                let mut f = first;
                while ok && f != next {
                    ok = encoder.invalidate(f)?;
                    f = f.wrapping_add(1);
                }
                if !ok {
                    // Same IDR rate limit as for requests: a slow link must not
                    // be buried in keyframes. Keep the loss pending until allowed.
                    let rtt = Duration::from_micros(
                        self.shared.cc.lock().unwrap().snapshot(now).rtt_us as u64,
                    );
                    if self.last_idr.elapsed() < Duration::from_millis(250).max(rtt * 3) {
                        let mut pending = self.shared.lost_pending.lock().unwrap();
                        if pending.is_none_or(|p| net::video::frame_before(first, p)) {
                            *pending = Some(first);
                        }
                        return Ok((fec_ratio, false, false));
                    }
                    self.last_idr = now;
                }
                if ok {
                    recovery = true;
                } else {
                    force_idr = true;
                }
                tracing::debug!(
                    "loss from frame {first}: {} frames tainted -> {}",
                    tainted,
                    if ok { "reference invalidation" } else { "IDR" }
                );
                self.shared.lost_acked.store(newest, Ordering::Release);
            }
        }
        Ok((fec_ratio, recovery, force_idr))
    }

    /// Once a second: stream stats for the host window.
    fn publish_status(&mut self, encoder_bps: u32, size: (u32, u32)) {
        self.status_frames += 1;
        let el = self.status_at.elapsed();
        if el < Duration::from_secs(1) {
            return;
        }
        let rtt = self
            .shared
            .cc
            .lock()
            .unwrap()
            .snapshot(Instant::now())
            .rtt_us;
        panel::STATUS.set_detail(format!(
            "{:.0} fps  ·  {}x{}  ·  {:.1} Mbps  ·  RTT {:.0} ms",
            self.status_frames as f64 / el.as_secs_f64(),
            size.0,
            size.1,
            encoder_bps as f64 / 1e6,
            rtt as f64 / 1e3
        ));
        self.status_frames = 0;
        self.status_at = Instant::now();
    }

    /// Pick the encode resolution from the congestion controller's target:
    /// on a slow link, fewer pixels at full frame rate beat 1080p starved of
    /// bits. Returns the new ladder index when it changes.
    fn pick_resolution(&mut self, sizes: &[(u32, u32)], fps: u32) -> Option<usize> {
        let now = Instant::now();
        let hold = now - self.res_changed;
        if self.res_started && hold < RES_STARTUP_STEP {
            return None;
        }
        let (target, startup) = {
            let cc = self.shared.cc.lock().unwrap();
            (cc.rate_bps(), cc.in_startup())
        };
        if self.res_started && !startup && hold < RES_HOLD {
            return None;
        }
        // Compare video against video: the target includes headers and FEC.
        let video = target / self.wire_overhead.max(1.0);
        let need =
            |i: usize| sizes[i].0 as f64 * sizes[i].1 as f64 * fps as f64 * RES_BITS_PER_PIXEL;
        let last = sizes.len() - 1;
        let fits = (0..sizes.len()).find(|&i| video >= need(i)).unwrap_or(last);
        let fits_with_margin = (0..sizes.len())
            .find(|&i| video >= need(i) * RES_UP_MARGIN)
            .unwrap_or(last);
        let cur = self.res_level;
        let mut next = cur;
        if !self.res_started {
            self.res_started = true;
            next = fits;
        } else if startup && fits_with_margin < cur {
            // Slow start found headroom: climb now.
            next = fits_with_margin;
        }
        // Down: not while slow start is still probing (its target says
        // nothing about the link yet).
        if fits > cur && !startup {
            let since = *self.res_below.get_or_insert(now);
            if now - since >= RES_DOWN_AFTER {
                next = fits;
            }
        } else {
            self.res_below = None;
        }
        // Up: straight to the largest level with margin.
        if fits_with_margin < cur {
            let since = *self.res_above.get_or_insert(now);
            if now - since >= RES_UP_AFTER {
                next = fits_with_margin;
            }
        } else {
            self.res_above = None;
        }
        if next == cur {
            return None;
        }
        self.res_level = next;
        self.res_changed = now;
        self.res_below = None;
        self.res_above = None;
        // A new resolution re-tests the frame rate from full speed.
        self.fps_divisor = 1;
        self.fps_cut_at_target = 0.0;
        self.overshoot_windows = 0;
        self.rate_window_start = now;
        self.rate_window_bytes = 0;
        self.rate_window_video = 0;
        tracing::info!(
            "resolution {}x{} -> {}x{} (video target {:.1} Mbps)",
            sizes[cur].0,
            sizes[cur].1,
            sizes[next].0,
            sizes[next].1,
            video / 1e6
        );
        Some(next)
    }

    /// Once per second: halve the frame rate while the wire rate overshoots
    /// the congestion target (the encoder is at its floor), double it back
    /// when the link clearly has room.
    fn adapt_frame_rate(&mut self, encoder_bps: u32) {
        let el = self.rate_window_start.elapsed();
        if el < Duration::from_secs(1) {
            return;
        }
        let wire_bps = self.rate_window_bytes as f64 * 8.0 / el.as_secs_f64();
        let video_bps = self.rate_window_video as f64 * 8.0 / el.as_secs_f64();
        let target = self.shared.cc.lock().unwrap().rate_bps();
        let before = self.fps_divisor;
        // The signal is the encoder overshooting the bitrate it was given
        // (frames already at its floor), sustained for two windows; a wire
        // rate above a freshly cut target is just a congestion transient.
        let asked = encoder_bps as f64 / self.fps_divisor as f64;
        self.overshoot_windows = if video_bps > 1.3 * asked {
            self.overshoot_windows + 1
        } else {
            0
        };
        if self.overshoot_windows >= 2 && self.fps_divisor < 4 {
            // The encoder cannot get frames small enough for this rate.
            self.fps_divisor *= 2;
            self.fps_cut_at_target = target;
            self.overshoot_windows = 0;
        } else if self.fps_divisor > 1 && target > 2.5 * self.fps_cut_at_target {
            // Hysteresis: step back up only with clear headroom over the rate
            // that forced the reduction.
            self.fps_divisor /= 2;
            self.fps_cut_at_target = target / 2.5;
        }
        if self.fps_divisor != before {
            tracing::info!(
                "frame rate 1/{} (wire {:.1} Mbps vs target {:.1} Mbps)",
                self.fps_divisor,
                wire_bps / 1e6,
                target / 1e6
            );
        }
        self.rate_window_start = Instant::now();
        self.rate_window_bytes = 0;
        self.rate_window_video = 0;
    }

    fn send_cursor_state(&mut self, peer: SocketAddr, state: CursorState) {
        let mut d = [0u8; HEADER_LEN + 10];
        let n = proto::write_datagram(Channel::Cursor, self.cursor_seq, &state.encode(), &mut d);
        self.cursor_seq = self.cursor_seq.wrapping_add(1);
        let _ = self.socket.send_to(&d[..n], peer);
    }

    /// Forward pointer changes to the client: a small state datagram, plus the
    /// shape (fragmented) whenever it changes or the client asks for it.
    fn send_cursor(
        &mut self,
        peer: SocketAddr,
        update: Option<capture::PointerUpdate>,
        size: (u32, u32),
    ) {
        if let Some(u) = update {
            if let Some(shape) = u.shape {
                self.last_shape = Some(shape);
                self.shared.want_cursor_shape.store(true, Ordering::Relaxed);
            }
            let norm = |v: i32, extent: u32| {
                (v.clamp(0, extent as i32 - 1) as i64 * 65535 / (extent - 1).max(1) as i64) as u16
            };
            self.cursor_pos = (norm(u.x, size.0), norm(u.y, size.1));
        }
        // Checked every iteration: an app can hide the cursor without DXGI
        // reporting a pointer update. DXGI's own visibility flag is not used
        // because it is false whenever the pointer is on another monitor;
        // only an app actually hiding it (games) should make the client
        // switch to relative mode.
        let state = CursorState {
            visible: cursor_shown(),
            shape_id: self.last_shape.as_ref().map_or(0, |s| s.shape_id),
            x: self.cursor_pos.0,
            y: self.cursor_pos.1,
        };
        // The client only acts on visibility and shape (it draws its own
        // cursor at its own mouse position), so position alone sends nothing.
        let changed = self
            .last_cursor
            .is_none_or(|c| c.visible != state.visible || c.shape_id != state.shape_id);
        self.last_cursor = Some(state);
        if changed {
            self.send_cursor_state(peer, state);
        }
        if self.shared.want_cursor_shape.swap(false, Ordering::Relaxed) {
            if let Some(shape) = &self.last_shape {
                let meta = ChunkMeta {
                    channel: Channel::CursorShape,
                    frame_id: self.shape_msg_id,
                    chunk_id: 0,
                    last: true,
                    keyframe: false,
                    recovery: false,
                    lost_acked: 0,
                    capture_us: clock::now_us(),
                    // Shapes are rare and small: always protect them.
                    fec_ratio: 0.25,
                };
                self.shape_msg_id = self.shape_msg_id.wrapping_add(1);
                let _ = self
                    .cursor_shapes
                    .send(&self.socket, peer, &meta, &shape.encode());
            }
            // Resend the state too, so a new client learns visibility immediately.
            if let Some(state) = self.last_cursor {
                self.send_cursor_state(peer, state);
            }
        }
    }
}

/// Whether the system cursor is currently shown (not hidden by an application).
fn cursor_shown() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetCursorInfo(&mut ci).is_ok()
            && ci.flags.0 & CURSOR_SHOWING.0 != 0
            && !ci.hCursor.is_invalid()
    }
}

/// Share this PC through the saved signal server, without a window of its
/// own (the combined app shows [`panel::STATUS`]). Returns when sharing
/// stops ([`panel::request_close`]).
pub fn share(signal: String) -> Result<()> {
    let mut args = Args::parse_from(["host"]);
    args.signal = Some(signal);
    run_main(args, true, true)
}

/// `gui`: share through the signal server with status for a window;
/// `embedded`: some other window shows that status (no panel of our own).
pub fn run_main(mut args: Args, gui: bool, embedded: bool) -> Result<()> {
    // Before any window exists (the panel's included): the DPI mode can't
    // change once one does, and desktop duplication needs per-monitor v2.
    rt::init_process();
    if gui {
        let settings = net::config::Settings::load();
        args.signal = Some(args.signal.take().unwrap_or(settings.signal.clone()));
        let id = net::crypto::Identity::load_or_create("host")?.host_id();
        panel::STATUS.set_id(net::crypto::format_id(id));
        panel::STATUS.set_state("Connecting to the server…");
        if !embedded {
            panel::spawn(
                net::crypto::format_id(id),
                args.signal.clone().unwrap_or_default(),
            );
        }
        if args.signal.as_deref().is_none_or(|s| s.trim().is_empty()) {
            panel::STATUS.set_state("No signal server set (see settings.txt)");
            anyhow::bail!("no signal server set: add `signal=HOST:PORT` to settings.txt");
        }
    }

    if args.list {
        return capture::list_outputs();
    }
    if args.list_audio {
        for n in audio::wasapi::list_outputs()? {
            println!("{n}");
        }
        return Ok(());
    }

    let (gpu, mut capturer) = capture::open(args.output)?;
    tracing::info!(
        "capturing output {} on {}: {}x{}",
        args.output,
        gpu.adapter_name,
        capturer.width,
        capturer.height
    );

    let ring = capture::create_ring(&gpu.device, capturer.width, capturer.height, RING)?;
    let test_pattern = if args.test_pattern {
        Some(pattern::TestPattern::new(
            &gpu.device,
            capturer.width,
            capturer.height,
        )?)
    } else {
        None
    };
    // Created before the encoder: its textures must outlive their NVENC
    // registration (the encoder unregisters them when dropped).
    let scaler = if args.no_adaptive_res {
        None
    } else {
        match scale::Scaler::new(&gpu.device, &ring, capturer.width, capturer.height) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("adaptive resolution off: {e:#}");
                None
            }
        }
    };
    let mut encoder = Encoder::new(
        &gpu.device,
        &ring,
        EncoderConfig {
            width: capturer.width,
            height: capturer.height,
            fps: args.fps,
            bitrate_bps: (args.bitrate * 1e6) as u32,
            slices: args.slices,
            subframe: !args.no_subframe,
        },
    )?;

    // Ladder sizes (index 0 = capture size) and each level's first input.
    let mut res_sizes = vec![(capturer.width, capturer.height)];
    let mut res_inputs = vec![0usize];
    if let Some(sc) = &scaler {
        for l in &sc.levels {
            res_inputs.push(encoder.register(&l.targets, l.width, l.height)?);
            res_sizes.push((l.width, l.height));
        }
        tracing::info!("adaptive resolution ladder: {res_sizes:?}");
    }

    let mut dump = match &args.dump {
        Some(p) => Some(BufWriter::with_capacity(1 << 20, File::create(p)?)),
        None => None,
    };

    if let Some(rgb) = &args.color_probe {
        unsafe {
            let mut rtv = None;
            gpu.device
                .CreateRenderTargetView(&ring[0], None, Some(&mut rtv))?;
            gpu.device
                .GetImmediateContext()?
                .ClearRenderTargetView(&rtv.unwrap(), &[rgb[0], rgb[1], rgb[2], 1.0]);
        }
        encoder.encode(0, true, |chunk, _| {
            if let Some(d) = dump.as_mut() {
                let _ = d.write_all(chunk);
            }
        })?;
        if let Some(mut d) = dump {
            d.flush()?;
        }
        return Ok(());
    }

    let fixed_fec = match args.fec.as_str() {
        "auto" => None,
        "off" => Some(0.0),
        v => Some(
            v.parse::<f32>()
                .context("--fec: expected auto, off or a ratio")?,
        ),
    };
    let cc = CcConfig {
        min_bps: args.min_bitrate * 1e6,
        max_bps: args.bitrate * 1e6,
        fixed_fec,
    };
    let signal = args
        .signal
        .as_deref()
        .map(|s| {
            Ok::<_, anyhow::Error>(rendezvous::Config {
                server: net::addr::resolve_v4(s)?,
                auto_approve: args.auto_approve,
            })
        })
        .transpose()?;
    let listen = match (args.listen, &signal) {
        (Some(a), _) => Some(a),
        (None, Some(_)) => Some("0.0.0.0:0".parse().unwrap()),
        (None, None) => None,
    };
    let mut stream = match listen {
        Some(addr) => Some(Stream::bind(
            addr,
            !args.no_uso,
            args.sim,
            cc,
            capturer.desktop_rect,
            signal,
        )?),
        None => None,
    };
    let audio_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut audio_thread = None;
    if let (Some(s), false) = (stream.as_ref(), args.no_audio) {
        let cfg = audio::capture::CaptureConfig {
            device: args.audio_device.clone(),
            test_tone: args.test_tone,
        };
        // Audio failing (no device, odd format) must not stop video.
        match audio_send::spawn(
            s.socket.try_clone()?,
            s.shared.clone(),
            cfg,
            audio_stop.clone(),
        ) {
            Ok(t) => audio_thread = Some(t),
            Err(e) => tracing::warn!("audio disabled: {e:#}"),
        }
    }
    let max_frames = args
        .frames
        .unwrap_or(if stream.is_some() { 0 } else { 600 });

    rt::mmcss_current_thread(rt::MmcssTask::Games);

    let frame_interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let mut stats = Stats::new(Duration::from_secs(1));
    let mut slot = 0usize;
    let mut last_present_us = 0u64;
    let mut have_frame = false;
    let mut encoded = 0u64;
    let mut next_forced = Instant::now();
    // Report any encode-loop section that blocks for longer than this.
    const STALL: Duration = Duration::from_millis(20);
    let stall = |what: &str, t: Instant| {
        if t.elapsed() > STALL {
            tracing::warn!(
                "stall: {what} took {:.1} ms",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    };

    while (max_frames == 0 || encoded < max_frames) && !panel::closing() {
        let peer = stream.as_ref().and_then(|s| s.shared.peer());
        let streaming = peer.is_some();

        // Wait briefly while idle so control packets are still serviced; in
        // benchmark mode wait only until the next forced frame is due.
        let timeout_ms = if args.force_fps && have_frame {
            next_forced
                .saturating_duration_since(Instant::now())
                .as_millis() as u32
        } else if stream.is_some() {
            10
        } else {
            100
        };
        let acquired = match &test_pattern {
            Some(p) => {
                // Fixed-rate synthetic frames: wait for the next frame slot.
                if stream.is_some() && !streaming {
                    // Nobody to stream to: idle instead of flooding the GPU
                    // queue with pattern copies the first encode would wait behind.
                    std::thread::sleep(Duration::from_millis(10));
                    next_forced = Instant::now();
                    continue;
                }
                let slept = Instant::now() < next_forced;
                while Instant::now() < next_forced {
                    let left = next_forced - Instant::now();
                    if left > Duration::from_millis(2) {
                        std::thread::sleep(left - Duration::from_millis(1));
                    } else {
                        std::hint::spin_loop();
                    }
                }
                let late = Instant::now() - next_forced;
                if slept && late > STALL {
                    tracing::warn!(
                        "stall: frame pacing woke {:.1} ms late",
                        late.as_secs_f64() * 1e3
                    );
                }
                // Start the latency clock before rendering, like the desktop
                // path starts it at DWM's present, so the numbers compare.
                let present_us = clock::now_us();
                let t = Instant::now();
                p.render(&ring[slot], encoded);
                stall("pattern render", t);
                Acquired::Frame {
                    copy_us: 0,
                    present_us,
                }
            }
            None => capturer.acquire(&ring[slot], timeout_ms)?,
        };
        let fresh = matches!(acquired, Acquired::Frame { .. });
        // Cursor work is kept off the capture -> encode path: it runs after
        // the encode below, or right away when there is nothing to encode.
        let service_cursor = |capturer: &mut capture::Capturer, stream: &mut Option<Stream>| {
            let pointer = capturer.take_pointer();
            if let (Some(s), Some(peer)) = (stream.as_mut(), peer) {
                s.send_cursor(peer, pointer, (capturer.width, capturer.height));
            }
        };

        let input = if let Acquired::Frame {
            copy_us,
            present_us,
        } = acquired
        {
            have_frame = true;
            last_present_us = present_us;
            stats.copy_us.push(copy_us);
            slot
        } else if args.force_fps && have_frame && Instant::now() >= next_forced {
            // Re-encode the previous slot to keep a constant benchmark rate.
            (slot + RING - 1) % RING
        } else {
            service_cursor(&mut capturer, &mut stream);
            stats.maybe_report();
            continue;
        };
        if fresh {
            slot = (slot + 1) % RING;
        }
        if stream.is_some() && !streaming {
            service_cursor(&mut capturer, &mut stream);
            continue; // nobody to send to
        }
        if let Some(s) = stream.as_mut() {
            s.candidate_frames += 1;
            if s.fps_divisor > 1 && !s.candidate_frames.is_multiple_of(s.fps_divisor as u64) {
                service_cursor(&mut capturer, &mut stream);
                next_forced += frame_interval;
                continue; // frame-rate reduction
            }
        }

        // Latency starts when DWM presented the image; for a forced re-encode
        // of an unchanged frame it starts now.
        let capture_us = if fresh {
            last_present_us
        } else {
            clock::now_us()
        };
        // Requested keyframes are rate-limited: each IDR is a large burst, and
        // on a slow link back-to-back IDRs refill the queue they are meant to
        // recover from. The request stays pending until it is allowed.
        let want_idr = stream.as_mut().is_some_and(|s| {
            if !s.shared.want_idr.load(Ordering::Relaxed) {
                return false;
            }
            let rtt = Duration::from_micros(
                s.shared.cc.lock().unwrap().snapshot(Instant::now()).rtt_us as u64,
            );
            let min_gap = Duration::from_millis(250).max(rtt * 3);
            if s.last_idr.elapsed() >= min_gap && s.shared.want_idr.swap(false, Ordering::Relaxed) {
                s.last_idr = Instant::now();
                true
            } else {
                false
            }
        });
        // Adaptive resolution: switch (reconfigure + IDR) when the link
        // calls for it, then encode the matching downscaled copy.
        let mut res_switched = false;
        if let (Some(s), Some(_)) = (stream.as_mut(), scaler.as_ref()) {
            let before = s.res_level;
            if let Some(l) = s.pick_resolution(&res_sizes, args.fps) {
                let (w, h) = res_sizes[l];
                match encoder.set_resolution(w, h) {
                    Ok(()) => {
                        // The reset emptied the DPB: the next picture is an
                        // IDR, so every earlier frame counts as excluded from
                        // references and older loss reports are moot.
                        res_switched = true;
                        s.last_idr = Instant::now();
                        *s.shared.lost_pending.lock().unwrap() = None;
                        s.shared.lost_acked.store(
                            encoder.next_frame_index().wrapping_sub(1),
                            Ordering::Release,
                        );
                    }
                    Err(e) => {
                        // Keep streaming at the old size.
                        tracing::warn!("resolution change refused: {e:#}");
                        s.res_level = before;
                    }
                }
            }
        }
        let level = stream.as_ref().map_or(0, |s| s.res_level);
        let input = match (&scaler, level) {
            (Some(sc), l) if l > 0 => {
                let t = Instant::now();
                sc.downscale(input, l - 1);
                stall("downscale", t);
                res_inputs[l] + input
            }
            _ => input,
        };
        let t = Instant::now();
        let (fec_ratio, recovery, loss_idr) = match stream.as_mut() {
            Some(s) => s.before_encode(&mut encoder)?,
            None => (0.0, false, false),
        };
        stall("before_encode", t);
        let force_idr = encoded == 0
            || res_switched
            || want_idr
            || loss_idr
            || (args.idr_every > 0 && encoded.is_multiple_of(args.idr_every));
        let lost_acked = stream
            .as_ref()
            .map_or(u32::MAX, |s| s.shared.lost_acked.load(Ordering::Acquire));
        // NVENC timestamps == frame ids, so reference invalidation can name frames.
        let frame_id = encoder.next_frame_index();
        let mut chunk_id = 0u16;
        let mut send_err = None;
        let mut send_time = Duration::ZERO;
        let (mut video_bytes, mut wire_bytes) = (0usize, 0usize);

        let t = Instant::now();
        let timing = encoder.encode(input, force_idr, |chunk, last| {
            if let Some(d) = dump.as_mut() {
                let _ = d.write_all(chunk);
            }
            if let (Some(s), Some(peer)) = (stream.as_mut(), peer) {
                let meta = ChunkMeta {
                    channel: Channel::Video,
                    frame_id,
                    chunk_id,
                    last,
                    keyframe: force_idr,
                    recovery,
                    lost_acked,
                    capture_us,
                    fec_ratio,
                };
                let ts = Instant::now();
                let (buf, segment) = s.video.packetize(&meta, chunk);
                video_bytes += chunk.len();
                wire_bytes += buf.len();
                if let Err(e) = s.socket.send_segments(buf, segment, peer) {
                    send_err.get_or_insert(e);
                }
                // Kept for NACK resends (answered by net-rx).
                // Only data shards are ever resent; skip storing parity.
                let k = net::fec::data_shards(chunk.len());
                s.shared.history.record(
                    frame_id,
                    chunk_id,
                    &buf[..(k * segment).min(buf.len())],
                    segment,
                );
                send_time += ts.elapsed();
            }
            chunk_id += 1;
        })?;
        if t.elapsed() > STALL {
            tracing::warn!(
                "stall: encode+send took {:.1} ms (send {:.1} ms, encoder {:.1} ms)",
                t.elapsed().as_secs_f64() * 1e3,
                send_time.as_secs_f64() * 1e3,
                timing.total_us as f64 / 1e3
            );
        }
        if let Some(e) = send_err {
            tracing::warn!("send failed: {e}");
        }
        let t = Instant::now();
        service_cursor(&mut capturer, &mut stream);
        stall("cursor", t);
        if let Some(s) = stream.as_mut() {
            if video_bytes > 0 {
                let r = wire_bytes as f64 / video_bytes as f64;
                s.wire_overhead = 0.97 * s.wire_overhead + 0.03 * r;
            }
            // IDRs are big by design: they say nothing about whether the
            // encoder can meet its per-frame budget.
            if !timing.idr {
                s.rate_window_bytes += wire_bytes;
                s.rate_window_video += video_bytes;
            }
            s.adapt_frame_rate(encoder.bitrate_bps());
        }

        if let Some(s) = stream.as_mut() {
            s.publish_status(encoder.bitrate_bps(), res_sizes[s.res_level]);
        }
        stats.first_chunk_us.push(timing.first_chunk_us);
        stats.encode_us.push(timing.total_us);
        stats.kbytes.push(timing.bytes / 1024);
        stats.frame(timing.idr);
        if encoded < 3 {
            tracing::debug!(?timing, "frame {encoded}");
        }

        encoded += 1;
        next_forced += frame_interval;
        if next_forced < Instant::now() {
            next_forced = Instant::now();
        }
        stats.maybe_report();
    }

    audio_stop.store(true, Ordering::Relaxed);
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    if let Some(mut d) = dump {
        d.flush()?;
        tracing::info!("wrote {encoded} frames to {}", args.dump.unwrap().display());
    }
    Ok(())
}
