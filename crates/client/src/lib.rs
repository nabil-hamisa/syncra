//! Client: receive → NVDEC → present, input and gamepad capture, audio.
//! The `client` binary is a thin wrapper around [`run_main`]; the combined
//! `syncra` app calls [`stream_session`] once it has connected.

mod annexb;
mod cursor;
mod decode;
mod gamepad;
mod input;
mod latency;
mod launcher;
mod render;
mod window;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;

use decode::Decoder;
use latency::{FrameTimes, Latency};
use net::feedback::FeedbackBuilder;
use net::{Event, Reassembler, Socket};
use proto::input::{CursorShape, CursorState};
use proto::{clock, Channel, Control, Header};
use render::Renderer;

/// Decode and display an H.264 stream with minimum latency.
#[derive(Parser, Debug, Clone)]
#[command(version)]
pub struct Args {
    /// Host to stream from, e.g. 127.0.0.1:9000.
    #[arg(long, conflicts_with = "input")]
    connect: Option<SocketAddr>,

    /// Host ID to connect to through the signal server (asks the host's user
    /// to approve; end-to-end encrypted), e.g. "482 913 077".
    #[arg(long, conflicts_with_all = ["connect", "input"], requires = "signal")]
    id: Option<String>,

    /// Signal server "host:port" for --id.
    #[arg(long)]
    signal: Option<String>,

    /// Always use the signal server's relay instead of a direct path (tests).
    #[arg(long)]
    force_relay: bool,

    /// Annex-B H.264 file to play (e.g. produced by `host --dump`).
    #[arg(long)]
    input: Option<PathBuf>,

    /// File playback rate; 0 decodes and presents as fast as possible.
    #[arg(long, default_value_t = 120)]
    fps: u32,

    /// Number of times to play the file (0 = forever).
    #[arg(long, default_value_t = 1)]
    loops: u32,

    /// Decode up to frame N (0-based), write it as raw BGRA to this path, and exit.
    #[arg(long, value_names = ["FRAME", "PATH"], num_args = 2)]
    verify: Option<Vec<String>>,

    /// Window position "x,y" in desktop coordinates.
    #[arg(long, value_delimiter = ',')]
    pos: Option<Vec<i32>>,

    /// Window client size "w,h".
    #[arg(long, value_delimiter = ',', default_value = "1280,720")]
    size: Vec<u32>,

    /// Borderless fullscreen on the window's monitor (toggle: Ctrl+Shift+F).
    /// Lets DWM hand the swapchain straight to the display (independent
    /// flip), so a tearing present skips composition.
    #[arg(long)]
    fullscreen: bool,

    /// Do not send keyboard, mouse or gamepad input to the host (useful when
    /// host and client share one machine and one mouse).
    #[arg(long)]
    view_only: bool,

    /// Forward gamepads even when the host is on this machine. Off by default:
    /// the client would see the host's own virtual pads and feed them back.
    #[arg(long)]
    loopback_gamepads: bool,

    /// Simulate the client→host link (feedback, input), e.g. "delay=20ms,loss=1%".
    #[arg(long)]
    sim: Option<net::sim::SimConfig>,

    /// Open the connect window (what running without arguments does), e.g.
    /// to combine it with other options.
    #[arg(long)]
    pub gui: bool,

    /// Don't play the host's audio.
    #[arg(long)]
    no_audio: bool,

    /// Playback device (part of its name; default device if omitted). On a
    /// single-machine test, pick one the host is not capturing, or the
    /// host hears its own stream back.
    #[arg(long)]
    audio_device: Option<String>,

    /// List playback devices and exit.
    #[arg(long)]
    list_audio: bool,

    /// Stop after this many seconds (0 = until the window closes).
    #[arg(long, default_value_t = 0)]
    seconds: u64,
}

/// Close the stream window opened by [`stream_session`], from any thread.
pub fn disconnect() {
    window::close();
}

/// Open a stream window for an already connected session (from the combined
/// app) and run it on the calling thread until the window closes.
pub fn stream_session(socket: Socket, host: SocketAddr, label: &str) -> Result<()> {
    let args = Args::parse_from(["client"]);
    let win = window::create(
        &format!("Syncra: {label}"),
        args.size[0],
        args.size[1],
        None,
        args.fullscreen,
    )?;
    let worker = std::thread::Builder::new()
        .name("stream".into())
        .spawn(move || {
            let r = run(&args, win, Some((socket, host)));
            window::close();
            r
        })?;
    window::run_message_loop();
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("stream thread panicked"))?
}

/// `gui`: show the connect window first (what running without arguments does).
pub fn run_main(args: Args, gui: bool) -> Result<()> {
    if args.list_audio {
        for n in audio::wasapi::list_outputs()? {
            println!("{n}");
        }
        return Ok(());
    }
    unsafe {
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        windows::Win32::Media::timeBeginPeriod(1);
    }

    let connected = if gui {
        match launcher::run()? {
            Some(c) => Some(c),
            None => return Ok(()), // launcher closed
        }
    } else {
        None
    };
    let title = connected
        .as_ref()
        .map_or("Syncra client".to_string(), |c| format!("Syncra: {}", c.2));
    let pos = args.pos.as_ref().map(|p| (p[0], p[1]));
    let win = window::create(&title, args.size[0], args.size[1], pos, args.fullscreen)?;

    // Everything latency-critical runs off the UI thread so window message
    // handling (drags, focus changes) can never stall it.
    let worker = std::thread::Builder::new()
        .name("stream".into())
        .spawn(move || {
            let r = run(&args, win, connected.map(|c| (c.0, c.1)));
            window::close();
            r
        })?;
    window::run_message_loop();
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("stream thread panicked"))?
}

fn run(
    args: &Args,
    win: window::WindowHandle,
    connected: Option<(Socket, SocketAddr)>,
) -> Result<()> {
    realtime_thread();
    let mut renderer = Renderer::new(win, args.size[0], args.size[1])?;
    let mut decoder = Decoder::new(&renderer.device)?;
    if let Some(path) = &args.input {
        return play_file(args, path, &mut renderer, &mut decoder);
    }
    if let Some((socket, host)) = connected {
        return stream(args, socket, host, &mut renderer, &mut decoder);
    }
    let mut socket = Socket::bind("0.0.0.0:0".parse().unwrap())?;
    socket.set_nonblocking(true)?;
    if let Some(sim) = args.sim {
        socket.set_sim(sim)?;
    }
    let host = match (&args.connect, &args.id, &args.signal) {
        (Some(host), _, _) => *host,
        (None, Some(id), Some(signal)) => {
            let id = net::crypto::parse_id(id).context("--id: expected 9 digits")?;
            let server = net::addr::resolve_v4(signal)?;
            net::connect::connect(&socket, server, id, args.force_relay)?
        }
        _ => anyhow::bail!("pass --connect HOST:PORT, --id ID --signal HOST:PORT, or --input FILE"),
    };
    stream(args, socket, host, &mut renderer, &mut decoder)
}

/// MMCSS "Games" scheduling for the calling thread.
pub(crate) fn realtime_thread() {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut idx = 0u32;
    if let Err(e) = unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Games"), &mut idx) } {
        tracing::warn!("MMCSS registration failed: {e}");
    }
}

/// Loss recovery in progress: frames from `first` on are tainted until a
/// keyframe, or a recovery frame whose `lost_acked` covers `last`.
struct Recovering {
    first: u32,
    last: u32,
    since: Instant,
    reported: Instant,
}

fn stream(
    args: &Args,
    socket: Socket,
    host: SocketAddr,
    renderer: &mut Renderer,
    decoder: &mut Decoder,
) -> Result<()> {
    tracing::info!("connecting to {host} from {}", socket.local_addr()?);

    let mut control_seq = 0u32;
    let mut send_control = |c: Control| {
        let _ = socket.send_to(&c.datagram(control_seq), host);
        control_seq += 1;
    };

    // Host on this machine: its audio would feed back into its own capture,
    // and its virtual pads would be read back as local ones.
    let same_machine = host.ip().is_loopback()
        || net::addr::local_candidates(0)
            .iter()
            .any(|a| std::net::IpAddr::V4(*a.ip()) == host.ip());
    let gamepad_stop = Arc::new(AtomicBool::new(false));
    let gamepad_thread = if args.view_only {
        None
    } else {
        input::start(socket.try_clone()?, host);
        if same_machine && !args.loopback_gamepads {
            tracing::info!("host is on this machine: gamepad forwarding off (use --loopback-gamepads to force)");
            None
        } else {
            Some(gamepad::spawn(gamepad_stop.clone()))
        }
    };

    let audio_device_given = args.audio_device.is_some();
    let mut player = if args.no_audio {
        None
    } else if same_machine && !audio_device_given {
        tracing::info!(
            "host is on this machine: audio off (it would feed back; pick another device with --audio-device)"
        );
        None
    } else {
        match audio::render::Player::start(args.audio_device.clone()) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("audio disabled: {e:#}");
                None
            }
        }
    };

    let mut reasm = Reassembler::new();
    let mut events = Vec::new();
    let mut feedback = FeedbackBuilder::new(clock::now_us());
    let mut cursor_reasm = Reassembler::new();
    let mut cursor_events = Vec::new();
    let mut cursor_shape_id = 0u32;
    let mut last_shape_req = Instant::now() - Duration::from_secs(1);
    let mut video_size = (0, 0);
    let mut buf = vec![0u8; 65536];
    let mut latency = Latency::new();
    let mut waiting_keyframe = true;
    let mut recovering: Option<Recovering> = None;
    let mut last_hello = Instant::now() - Duration::from_secs(1);
    // The host drops clients it hasn't heard from in 3 s.
    const KEEPALIVE: Duration = Duration::from_millis(500);
    // Re-send a loss report this often until the recovery frame arrives;
    // ask for a keyframe if none arrives in time. Both scale with the RTT:
    // a fixed timeout shorter than the round trip makes every recovery look
    // late and turns into a keyframe storm that buries a slow link.
    const REPORT_EVERY: Duration = Duration::from_millis(30);
    const RECOVERY_TIMEOUT_MIN: Duration = Duration::from_millis(300);
    let mut rtt = Duration::ZERO;
    let recovery_timeout = |rtt: Duration| RECOVERY_TIMEOUT_MIN.max(rtt * 4);
    let keyframe_every = |rtt: Duration| Duration::from_millis(100).max(rtt * 2);
    // NACK only below this RTT (a resend must beat a recovery frame).
    const NACK_MAX_RTT_US: u32 = 20_000;
    let mut last_keepalive = Instant::now();
    let mut last_keyframe_req = Instant::now() - Duration::from_secs(1);
    let mut got_any = false;
    let mut frame_send_us = 0u64;
    let started = Instant::now();

    // Report any loop section that blocks for longer than this.
    const STALL: Duration = Duration::from_millis(20);
    let stall = |what: &str, t: Instant| {
        let el = t.elapsed();
        if el > STALL {
            tracing::warn!("stall: {what} took {:.1} ms", el.as_secs_f64() * 1e3);
        }
    };

    'session: loop {
        let t = Instant::now();
        let alive = renderer.poll()?;
        stall("window poll", t);
        if !alive || (args.seconds > 0 && started.elapsed().as_secs() >= args.seconds) {
            break;
        }
        if !got_any && last_hello.elapsed() > Duration::from_millis(250) {
            send_control(Control::Hello);
            last_hello = Instant::now();
        }

        // Busy-poll the socket: lowest wake-up latency, costs one core.
        let t = Instant::now();
        let mut received = false;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from == host => {
                    let arrival_us = clock::now_us();
                    received = true;
                    got_any = true;
                    let Some((h, payload)) = Header::read(&buf[..n]) else {
                        continue;
                    };
                    match h.channel() {
                        Some(Channel::Video) => {
                            feedback.on_video(&h, n, clock::now_us());
                            reasm.push(&h, payload, &mut events)
                        }
                        Some(Channel::Audio) => {
                            if let (Some(p), Some(items)) =
                                (player.as_mut(), proto::audio::read(payload))
                            {
                                for (seq, pkt) in items {
                                    let back = h.sequence.wrapping_sub(seq) as u64;
                                    let capture_us =
                                        h.capture_us.saturating_sub(back * proto::audio::FRAME_US);
                                    p.push(seq, back == 0, capture_us, arrival_us, pkt);
                                }
                            }
                        }
                        Some(Channel::CursorShape) => {
                            cursor_reasm.push(&h, payload, &mut cursor_events)
                        }
                        Some(Channel::Cursor) => {
                            if let Some(c) = CursorState::decode(payload) {
                                window::set_host_cursor_visible(c.visible);
                                if c.shape_id != 0
                                    && c.shape_id != cursor_shape_id
                                    && last_shape_req.elapsed() > Duration::from_millis(200)
                                {
                                    send_control(Control::RequestCursorShape);
                                    last_shape_req = Instant::now();
                                }
                            }
                        }
                        Some(Channel::Control) => match Control::decode(payload) {
                            Some(Control::Ping { t_us, rtt_us }) => {
                                send_control(Control::Pong { t_us });
                                rtt = Duration::from_micros(rtt_us as u64);
                                // Resends pay off only while a round trip is cheap
                                // compared to the frame budget; beyond that, FEC and
                                // reference invalidation handle loss without waiting.
                                let wait = (rtt_us > 0 && rtt_us < NACK_MAX_RTT_US)
                                    .then(|| Duration::from_micros(rtt_us as u64 * 3 / 2 + 1_000));
                                reasm.set_nack_wait(wait);
                            }
                            Some(Control::Rumble {
                                index,
                                large,
                                small,
                            }) => gamepad::rumble(index, large, small),
                            Some(Control::Bye) => {
                                tracing::info!("the host ended the session");
                                break 'session;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break, // host not up yet
                Err(e) => return Err(e).context("recv"),
            }
        }
        stall("socket drain", t);

        reasm.poll(&mut events);
        let t = Instant::now();
        let n_events = events.len();
        for ev in events.drain(..) {
            match ev {
                Event::Nack {
                    frame_id,
                    chunk_id,
                    base,
                    mask,
                } => {
                    send_control(Control::Nack {
                        frame_id,
                        chunk_id,
                        base,
                        mask,
                    });
                }
                Event::FrameLost { first, last } => {
                    tracing::debug!("frames {first}..={last} lost");
                    latency.lost(last.wrapping_sub(first).wrapping_add(1));
                    // Keep decoding (the reference chain stays intact) but show
                    // nothing until the host has cut the tainted references.
                    send_control(Control::FrameLost { frame_id: first });
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
                            tracing::debug!(
                                "recovered at frame {} ({}) after {:?}",
                                c.frame_id,
                                if c.keyframe {
                                    "IDR"
                                } else {
                                    "ref invalidation"
                                },
                                r.since.elapsed()
                            );
                            latency.recovered(r.since.elapsed());
                            recovering = None;
                        }
                    }
                    frame_send_us = frame_send_us.max(c.send_us);
                    let complete_us = clock::now_us();
                    let tf = Instant::now();
                    let ready = decoder.feed(&c.data, c.last, c.frame_id as i64)?;
                    stall(
                        if c.last {
                            "feed (last chunk)"
                        } else {
                            "feed (partial chunk)"
                        },
                        tf,
                    );
                    if !c.last {
                        continue;
                    }
                    feedback.on_frame_complete(c.frame_id);
                    let last_send_us = std::mem::take(&mut frame_send_us);
                    if !ready {
                        continue;
                    }
                    let decoded_us = clock::now_us();
                    stall(
                        "decode",
                        Instant::now() - Duration::from_micros(decoded_us - complete_us),
                    );
                    let (shown_id, decode_ok) = decoder.last_output();
                    if !decode_ok {
                        // NVDEC concealed or failed: never show it, and treat
                        // it as a loss so the host repairs the references.
                        latency.corrupt();
                        if recovering.is_none() {
                            let id = shown_id as u32;
                            send_control(Control::FrameLost { frame_id: id });
                            let now = Instant::now();
                            recovering = Some(Recovering {
                                first: id,
                                last: id,
                                since: now,
                                reported: now,
                            });
                        }
                        continue;
                    }
                    if recovering.is_some() {
                        latency.suppressed();
                        continue;
                    }
                    let tp = Instant::now();
                    if let Some(p) = decoder.planes() {
                        if (p.width, p.height) != video_size {
                            video_size = (p.width, p.height);
                            window::set_video_size(p.width, p.height);
                        }
                        renderer.present(p)?;
                    }
                    stall("present", tp);
                    latency.record(FrameTimes {
                        capture_us: c.capture_us,
                        last_send_us,
                        complete_us,
                        decoded_us,
                        presented_us: clock::now_us(),
                    });
                }
            }
        }
        if t.elapsed() > STALL {
            tracing::warn!(
                "stall: processing {n_events} events took {:.1} ms",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
        if let Some(f) = feedback.poll(clock::now_us()) {
            if got_any {
                send_control(Control::Feedback(f));
            }
        }
        if let Some(r) = recovering.as_mut() {
            if r.since.elapsed() > recovery_timeout(rtt) {
                if last_keyframe_req.elapsed() > keyframe_every(rtt) {
                    send_control(Control::RequestKeyframe);
                    last_keyframe_req = Instant::now();
                }
            } else if r.reported.elapsed() > REPORT_EVERY {
                send_control(Control::FrameLost { frame_id: r.first });
                r.reported = Instant::now();
            }
        }

        // Cursor shapes are handled after the video so they never delay a
        // frame; the native cursor is built on the UI thread.
        for ev in cursor_events.drain(..) {
            if let Event::Chunk(c) = ev {
                match CursorShape::decode(&c.data) {
                    Some(s) => {
                        tracing::debug!(
                            "cursor shape {} {:?} {}x{} hotspot ({},{})",
                            s.shape_id,
                            s.kind,
                            s.width,
                            s.height,
                            s.hot_x,
                            s.hot_y
                        );
                        cursor_shape_id = s.shape_id;
                        window::set_cursor_shape(s);
                    }
                    None => tracing::warn!("malformed cursor shape"),
                }
            }
        }
        if got_any && last_keepalive.elapsed() > KEEPALIVE {
            send_control(Control::KeepAlive);
            last_keepalive = Instant::now();
        }
        if waiting_keyframe && got_any && last_keyframe_req.elapsed() > keyframe_every(rtt) {
            send_control(Control::RequestKeyframe);
            last_keyframe_req = Instant::now();
        }

        latency.set_fec_recovered(reasm.stats.fec_recovered);
        if latency.tick() {
            tracing::info!("{}", latency.summary);
            window::set_title(&format!("Syncra | p50/p95/max ms: {}", latency.summary));
        }
        if !received {
            std::hint::spin_loop();
        }
    }
    send_control(Control::Bye);
    input::stop();
    gamepad_stop.store(true, Ordering::Relaxed);
    if let Some(t) = gamepad_thread {
        let _ = t.join();
    }
    Ok(())
}
fn play_file(
    args: &Args,
    path: &PathBuf,
    renderer: &mut Renderer,
    decoder: &mut Decoder,
) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let aus = annexb::split_access_units(&data);
    tracing::info!(
        "{}: {} access units, {:.1} KB",
        path.display(),
        aus.len(),
        data.len() as f64 / 1024.0
    );

    if let Some(v) = &args.verify {
        let target: usize = v[0].parse().context("verify FRAME")?;
        let mut decoded = 0usize;
        for (i, au) in aus.iter().enumerate() {
            if decoder.decode(au, i as i64)? {
                if decoded == target {
                    let planes = decoder.planes().context("no output planes")?;
                    let px = renderer.read_back(planes)?;
                    std::fs::write(&v[1], &px)?;
                    tracing::info!(
                        "wrote frame {target} ({}x{} BGRA) to {}",
                        planes.width,
                        planes.height,
                        v[1]
                    );
                    return Ok(());
                }
                decoded += 1;
            }
        }
        anyhow::bail!("stream has only {decoded} frames");
    }

    let interval = (args.fps > 0).then(|| Duration::from_secs_f64(1.0 / args.fps as f64));
    let mut decode_us = Vec::with_capacity(aus.len());
    let mut present_us = Vec::with_capacity(aus.len());
    let mut next = Instant::now();
    let mut ts = 0i64;
    let mut pass = 0;
    let started = Instant::now();

    'outer: while args.loops == 0 || pass < args.loops {
        for au in &aus {
            if !renderer.poll()? {
                break 'outer;
            }
            if let Some(iv) = interval {
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                next += iv;
            }
            renderer.wait_ready(100);

            let t0 = Instant::now();
            let ready = decoder.decode(au, ts)?;
            let t1 = Instant::now();
            ts += 1;
            if !ready {
                continue;
            }
            if let Some(p) = decoder.planes() {
                renderer.present(p)?;
            }
            let t2 = Instant::now();
            // Skip the first frame: it includes decoder creation.
            if ts > 1 {
                decode_us.push((t1 - t0).as_micros() as u32);
                present_us.push((t2 - t1).as_micros() as u32);
            }
        }
        pass += 1;
    }

    let secs = started.elapsed().as_secs_f64();
    report("decode (NAL in -> texture written)", &mut decode_us);
    report("render + present call", &mut present_us);
    tracing::info!(
        "{} frames in {:.2}s = {:.1} fps",
        ts,
        secs,
        ts as f64 / secs
    );
    Ok(())
}

fn report(name: &str, v: &mut [u32]) {
    if v.is_empty() {
        return;
    }
    v.sort_unstable();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1000.0;
    tracing::info!(
        "{name}: p50 {:.2} ms  p95 {:.2} ms  p99 {:.2} ms  max {:.2} ms",
        p(0.5),
        p(0.95),
        p(0.99),
        p(1.0)
    );
}
