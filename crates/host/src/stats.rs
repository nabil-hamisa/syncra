//! Per-stage latency histograms printed once per reporting window.

use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Series {
    samples: Vec<u32>,
}

impl Series {
    pub fn push(&mut self, v: u32) {
        self.samples.push(v);
    }

    /// (p50, p95, max) and clears the series.
    pub fn take(&mut self) -> Option<(u32, u32, u32)> {
        if self.samples.is_empty() {
            return None;
        }
        self.samples.sort_unstable();
        let n = self.samples.len();
        let p = |q: f64| self.samples[((n - 1) as f64 * q).round() as usize];
        let out = (p(0.50), p(0.95), self.samples[n - 1]);
        self.samples.clear();
        Some(out)
    }
}

pub struct Stats {
    pub copy_us: Series,
    pub first_chunk_us: Series,
    pub encode_us: Series,
    pub kbytes: Series,
    frames: u32,
    idr: u32,
    window_start: Instant,
    window: Duration,
}

impl Stats {
    pub fn new(window: Duration) -> Self {
        Stats {
            copy_us: Series::default(),
            first_chunk_us: Series::default(),
            encode_us: Series::default(),
            kbytes: Series::default(),
            frames: 0,
            idr: 0,
            window_start: Instant::now(),
            window,
        }
    }

    pub fn frame(&mut self, idr: bool) {
        self.frames += 1;
        self.idr += idr as u32;
    }

    pub fn maybe_report(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < self.window {
            return;
        }
        let fps = self.frames as f64 / elapsed.as_secs_f64();
        let fmt = |name: &str, s: &mut Series, scale: f64, unit: &str| match s.take() {
            Some((p50, p95, max)) => format!(
                "{name} p50 {:.2}{unit} p95 {:.2}{unit} max {:.2}{unit}",
                p50 as f64 * scale,
                p95 as f64 * scale,
                max as f64 * scale
            ),
            None => format!("{name} -"),
        };
        tracing::info!(
            "{:5.1} fps | {} | {} | {} | {} | idr {}",
            fps,
            fmt("copy", &mut self.copy_us, 1e-3, "ms"),
            fmt("1st-slice", &mut self.first_chunk_us, 1e-3, "ms"),
            fmt("encode", &mut self.encode_us, 1e-3, "ms"),
            fmt("size", &mut self.kbytes, 1.0, "KB"),
            self.idr
        );
        self.frames = 0;
        self.idr = 0;
        self.window_start = Instant::now();
    }
}
