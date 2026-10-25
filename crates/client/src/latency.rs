//! End-to-end latency breakdown per frame. All timestamps share the host's QPC
//! timebase (exact on loopback; cross-machine needs clock sync, later phase).

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct FrameTimes {
    /// DWM presented the desktop image on the host.
    pub capture_us: u64,
    /// Host sent the last datagram of the picture.
    pub last_send_us: u64,
    /// Client reassembled the last chunk.
    pub complete_us: u64,
    /// Decoded picture written into the render textures.
    pub decoded_us: u64,
    /// `Present` returned.
    pub presented_us: u64,
}

pub const STAGES: [&str; 5] = ["host", "net", "decode", "present", "TOTAL"];

#[derive(Default)]
struct Series(Vec<u32>);

impl Series {
    fn take(&mut self) -> (f64, f64, f64) {
        let v = &mut self.0;
        v.sort_unstable();
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1000.0;
        let r = (p(0.5), p(0.95), p(1.0));
        v.clear();
        r
    }
}

pub struct Latency {
    stages: [Series; 5],
    frames: u32,
    lost: u32,
    suppressed: u32,
    corrupt: u32,
    fec_recovered: u64,
    fec_base: u64,
    recovery_ms: Vec<u32>,
    last_present: Option<Instant>,
    gap_max: Duration,
    window_start: Instant,
    pub summary: String,
}

impl Latency {
    pub fn new() -> Self {
        Latency {
            stages: Default::default(),
            frames: 0,
            lost: 0,
            suppressed: 0,
            corrupt: 0,
            fec_recovered: 0,
            fec_base: 0,
            recovery_ms: Vec::new(),
            last_present: None,
            gap_max: Duration::ZERO,
            window_start: Instant::now(),
            summary: String::new(),
        }
    }

    pub fn record(&mut self, t: FrameTimes) {
        let d = |a: u64, b: u64| b.saturating_sub(a) as u32;
        let v = [
            d(t.capture_us, t.last_send_us),
            d(t.last_send_us, t.complete_us),
            d(t.complete_us, t.decoded_us),
            d(t.decoded_us, t.presented_us),
            d(t.capture_us, t.presented_us),
        ];
        for (s, x) in self.stages.iter_mut().zip(v) {
            s.0.push(x);
        }
        self.frames += 1;
        let now = Instant::now();
        if let Some(prev) = self.last_present.replace(now) {
            self.gap_max = self.gap_max.max(now - prev);
        }
    }

    /// Frames lost in the network (FEC could not repair them).
    pub fn lost(&mut self, frames: u32) {
        self.lost += frames;
    }

    /// Decoded but not shown: tainted by a loss, waiting for a recovery frame.
    pub fn suppressed(&mut self) {
        self.suppressed += 1;
    }

    /// NVDEC reported an error or concealment for a picture; it was not shown.
    pub fn corrupt(&mut self) {
        self.corrupt += 1;
    }

    pub fn recovered(&mut self, after: Duration) {
        self.recovery_ms.push(after.as_millis() as u32);
    }

    /// Cumulative chunks repaired by FEC (from the reassembler).
    pub fn set_fec_recovered(&mut self, total: u64) {
        self.fec_recovered = total;
    }

    /// Once per second, rebuild `summary` and return true.
    pub fn tick(&mut self) -> bool {
        let el = self.window_start.elapsed();
        if el < Duration::from_secs(1) {
            return false;
        }
        let fps = self.frames as f64 / el.as_secs_f64();
        let mut parts = vec![format!("{fps:.0} fps")];
        if self.frames > 0 {
            for (name, s) in STAGES.iter().zip(self.stages.iter_mut()) {
                let (p50, p95, max) = s.take();
                parts.push(format!("{name} {p50:.2}/{p95:.2}/{max:.2}"));
            }
        }
        parts.push(format!("gap {:.1}", self.gap_max.as_secs_f64() * 1e3));
        let fec = self.fec_recovered - self.fec_base;
        if fec > 0 {
            parts.push(format!("FEC {fec}"));
        }
        if self.lost > 0 {
            parts.push(format!("LOST {}", self.lost));
        }
        if self.suppressed > 0 {
            parts.push(format!("HIDDEN {}", self.suppressed));
        }
        if self.corrupt > 0 {
            parts.push(format!("CORRUPT {}", self.corrupt));
        }
        if !self.recovery_ms.is_empty() {
            let max = self.recovery_ms.iter().max().unwrap();
            parts.push(format!("RECOVER {}x max {max}ms", self.recovery_ms.len()));
        }
        self.summary = parts.join(" | ");
        self.frames = 0;
        self.lost = 0;
        self.suppressed = 0;
        self.corrupt = 0;
        self.fec_base = self.fec_recovered;
        self.recovery_ms.clear();
        self.gap_max = Duration::ZERO;
        self.window_start = Instant::now();
        true
    }
}
