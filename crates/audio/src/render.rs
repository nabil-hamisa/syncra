//! Client audio: jitter buffer → Opus decode → low-latency WASAPI output.
//!
//! The stream thread hands packets over through a wait-free SPSC ring; the
//! render thread (MMCSS "Pro Audio") wakes once per engine period, pulls
//! 5 ms frames from the jitter buffer as the device needs them and keeps at
//! about one period queued in the device (two if a wake-up ever finds it
//! dry).
//!
//! Latency is measured per frame as (time its first sample reaches the
//! device output, estimated from the queued padding) − (host capture time).
//! Host and client share the QPC clock only on one machine, so the number
//! is meaningful on loopback tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use opus_sys::Decoder;
use proto::audio::{CHANNELS, FRAME, MAX_PACKET, RATE};
use proto::clock;
use rtrb::{Consumer, Producer, RingBuffer};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::jitter::{JitterBuffer, Pop};
use crate::resample::Linear;
use crate::wasapi;

/// One received Opus frame, moved through the ring without allocation.
#[derive(Clone, Copy)]
pub struct Packet {
    pub seq: u32,
    pub primary: bool,
    pub capture_us: u64,
    pub arrival_us: u64,
    pub len: u16,
    pub data: [u8; MAX_PACKET],
}

pub struct Player {
    tx: Producer<Packet>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Start playback on the device whose name contains `device` (default
    /// device if None).
    pub fn start(device: Option<String>) -> Result<Self> {
        let (tx, rx) = RingBuffer::new(256);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                wasapi::pro_audio_thread();
                if let Err(e) = run(device.as_deref(), rx, &s) {
                    tracing::error!("audio playback stopped: {e:#}");
                }
            })?;
        Ok(Player {
            tx,
            stop,
            thread: Some(thread),
        })
    }

    /// Queue one Opus frame (dropped if the render thread is far behind).
    /// `arrival_us` should be stamped right at receive, so time the caller
    /// spends on other work (video) doesn't look like network jitter.
    pub fn push(
        &mut self,
        seq: u32,
        primary: bool,
        capture_us: u64,
        arrival_us: u64,
        packet: &[u8],
    ) {
        if packet.len() > MAX_PACKET {
            return;
        }
        let mut p = Packet {
            seq,
            primary,
            capture_us,
            arrival_us,
            len: packet.len() as u16,
            data: [0; MAX_PACKET],
        };
        p.data[..packet.len()].copy_from_slice(packet);
        let _ = self.tx.push(p);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Session-wide latency histogram, 0.1 ms buckets up to 500 ms.
struct Histogram {
    buckets: Vec<u32>,
    count: u64,
}

impl Histogram {
    fn new() -> Self {
        Histogram {
            buckets: vec![0; 5000],
            count: 0,
        }
    }

    fn add(&mut self, us: u64) {
        let i = ((us / 100) as usize).min(self.buckets.len() - 1);
        self.buckets[i] += 1;
        self.count += 1;
    }

    /// Percentile in ms (bucket upper edge).
    fn pct(&self, p: f64) -> f64 {
        let want = (self.count as f64 * p).ceil() as u64;
        let mut seen = 0u64;
        for (i, &b) in self.buckets.iter().enumerate() {
            seen += b as u64;
            if seen >= want.max(1) {
                return (i + 1) as f64 / 10.0;
            }
        }
        0.0
    }
}

/// State that survives reopening the device.
struct Session {
    decoder: Decoder,
    jb: JitterBuffer,
    hist: Histogram,
    window: Vec<u32>,
    silent_frames: u64,
    /// Device starved at a wake-up (glitch): queue one more period from then on.
    dry_wakes: u64,
}

fn run(device: Option<&str>, mut rx: Consumer<Packet>, stop: &AtomicBool) -> Result<()> {
    let mut s = Session {
        decoder: Decoder::new(RATE, CHANNELS)?,
        jb: JitterBuffer::new(),
        hist: Histogram::new(),
        window: Vec::with_capacity(512),
        silent_frames: 0,
        dry_wakes: 0,
    };
    // The device can disappear (unplugged, default changed, exclusive mode
    // elsewhere): reopen it rather than going silent for the session.
    while !stop.load(Ordering::Relaxed) {
        match play(device, &mut rx, stop, &mut s) {
            Ok(()) => break,
            Err(e) if wasapi::is_device_lost(&e) => {
                tracing::warn!("audio device lost ({e}); reopening");
            }
            Err(e) => tracing::error!("audio playback failed: {e:#}; retrying"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if s.hist.count > 0 {
        tracing::info!(
            "audio session: capture→speaker p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} ms over {} frames | concealed {} underruns {} silent {} | device ran dry {}x",
            s.hist.pct(0.50),
            s.hist.pct(0.95),
            s.hist.pct(0.99),
            s.hist.pct(1.0),
            s.hist.count,
            s.jb.stats.concealed,
            s.jb.stats.underruns,
            s.silent_frames,
            s.dry_wakes
        );
    }
    Ok(())
}

fn play(
    device: Option<&str>,
    rx: &mut Consumer<Packet>,
    stop: &AtomicBool,
    s: &mut Session,
) -> Result<()> {
    let (dev, name) = wasapi::output_device(device)?;
    let out = wasapi::open_low_latency_render(&dev)?;
    let (fmt, period) = (&out.fmt, out.period);
    // SAFETY: COM calls on valid interfaces from this (MTA) thread; the
    // render buffer is written only within the frame count GetBuffer granted.
    unsafe {
        let render: IAudioRenderClient = out.client.GetService()?;
        let buf_frames = out.client.GetBufferSize()?;
        tracing::info!(
            "audio: playing on \"{name}\" ({} Hz, {} ch), period {:.2} ms",
            fmt.rate,
            fmt.channels,
            period as f64 * 1e3 / fmt.rate as f64
        );
        out.client.Start()?;

        // Device queue target: one period plus 0.5 ms is enough when the
        // thread wakes promptly (it refills right after each engine pass).
        // A wake-up that finds the device dry adds a period, for good.
        let margin = fmt.rate / 2000;
        let mut queue = (period + margin).min(buf_frames);

        let mut resampler = Linear::new(RATE, fmt.rate);
        let mut pcm = [0f32; FRAME * CHANNELS];
        // Device-rate stereo samples waiting to be written.
        let mut fifo: VecDeque<f32> = VecDeque::with_capacity(16384);
        let mut resampled: Vec<f32> = Vec::with_capacity(4096);
        // (absolute fifo frame index of a 5 ms frame's first sample, capture time)
        let mut marks: VecDeque<(u64, u64)> = VecDeque::with_capacity(64);
        let mut fifo_base = 0u64; // absolute index of fifo[0]
        let mut report_at = Instant::now() + Duration::from_secs(1);
        // Longest gap between device wake-ups this second (device stalls).
        let mut last_wake = Instant::now();
        let mut max_wake_gap = Duration::ZERO;
        let mut started = false;

        let result = (|| -> Result<()> {
            while !stop.load(Ordering::Relaxed) {
                let woke = WaitForSingleObject(out.event.0, 50) == WAIT_OBJECT_0;
                let now_i = Instant::now();
                max_wake_gap = max_wake_gap.max(now_i - last_wake);
                if !woke {
                    continue;
                }
                last_wake = now_i;
                while let Ok(p) = rx.pop() {
                    s.jb.push(
                        p.seq,
                        p.capture_us,
                        p.arrival_us,
                        &p.data[..p.len as usize],
                        p.primary,
                    );
                }
                let pad = out.client.GetCurrentPadding()?;
                if pad == 0 && started && queue < (period * 2).min(buf_frames) {
                    s.dry_wakes += 1;
                    queue = (period * 2).min(buf_frames);
                    tracing::warn!("audio: device ran dry; queueing two periods from now on");
                }
                let want = queue.saturating_sub(pad);
                if want == 0 {
                    continue;
                }
                started = true;
                // Refill the FIFO from the jitter buffer, 5 ms at a time.
                while fifo.len() / 2 < want as usize {
                    let capture_us = match s.jb.pop() {
                        Pop::Packet(pkt, capture_us) => {
                            decode(&mut s.decoder, Some(pkt), &mut pcm);
                            Some(capture_us)
                        }
                        Pop::Conceal => {
                            decode(&mut s.decoder, None, &mut pcm);
                            None
                        }
                        Pop::Empty => {
                            s.silent_frames += 1;
                            pcm.fill(0.0);
                            None
                        }
                    };
                    if let Some(c) = capture_us {
                        marks.push_back((fifo_base + (fifo.len() / 2) as u64, c));
                    }
                    resampled.clear();
                    resampler.process(&pcm, &mut resampled);
                    fifo.extend(resampled.iter());
                }
                // Write `want` frames, mapping stereo onto the device channels.
                let p = render.GetBuffer(want)? as *mut f32;
                let dst = std::slice::from_raw_parts_mut(p, want as usize * fmt.channels);
                for f in dst.chunks_exact_mut(fmt.channels) {
                    let l = fifo.pop_front().unwrap_or(0.0);
                    let r = fifo.pop_front().unwrap_or(0.0);
                    match fmt.channels {
                        1 => f[0] = (l + r) * 0.5,
                        _ => {
                            f[0] = l;
                            f[1] = r;
                            f[2..].fill(0.0);
                        }
                    }
                }
                render.ReleaseBuffer(want, 0)?;
                // Frames whose first sample was just written: it plays after
                // the `pad` frames already queued plus its offset in this write.
                let now = clock::now_us();
                let end = fifo_base + want as u64;
                while let Some(&(at, c)) = marks.front() {
                    if at >= end {
                        break;
                    }
                    marks.pop_front();
                    let ahead = pad as u64 + at.saturating_sub(fifo_base);
                    let lat = (now + ahead * 1_000_000 / fmt.rate as u64).saturating_sub(c);
                    s.hist.add(lat);
                    if s.window.len() < s.window.capacity() {
                        s.window.push(lat.min(u32::MAX as u64) as u32);
                    }
                }
                fifo_base = end;

                if Instant::now() >= report_at {
                    report_at += Duration::from_secs(1);
                    s.window.sort_unstable();
                    let w = &s.window;
                    let pct = |p: usize| w.get(w.len() * p / 100).map_or(0.0, |&v| v as f64 / 1e3);
                    let st = s.jb.stats;
                    if st.played > 0 || s.silent_frames > 0 {
                        tracing::info!(
                            "audio: capture→speaker p50 {:.1} p95 {:.1} ms | buffer {}/{} frames | played {} concealed {} stretched {} skipped {} underruns {} late {} | silent {} | max wake gap {:.1} ms",
                            pct(50),
                            pct(95),
                            s.jb.depth(),
                            s.jb.target_frames(),
                            st.played,
                            st.concealed,
                            st.stretched,
                            st.skipped,
                            st.underruns,
                            st.late,
                            s.silent_frames,
                            max_wake_gap.as_secs_f64() * 1e3
                        );
                    }
                    s.window.clear();
                    max_wake_gap = Duration::ZERO;
                }
            }
            Ok(())
        })();
        let _ = out.client.Stop();
        drop(render);
        result
    }
}

/// Decode one 5 ms frame; anything short or failed is padded with silence
/// so a stale tail is never replayed.
fn decode(dec: &mut Decoder, pkt: Option<&[u8]>, pcm: &mut [f32]) {
    match dec.decode(pkt, pcm) {
        Ok(n) if n == FRAME => {}
        Ok(n) => {
            let from = (n * CHANNELS).min(pcm.len());
            pcm[from..].fill(0.0)
        }
        Err(_) => pcm.fill(0.0),
    }
}
