//! Linear-interpolation resampler for interleaved stereo. Used only when a
//! device does not run at 48 kHz; at audio rates the quality is adequate for
//! a remote-desktop stream and it costs nothing.

pub struct Linear {
    /// Input samples per output sample.
    step: f64,
    /// Read position relative to the start of the next input block; may be
    /// in [-1, 0) to interpolate from the previous block's last frame.
    pos: f64,
    prev: [f32; 2],
}

impl Linear {
    pub fn new(from: u32, to: u32) -> Self {
        Linear {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    pub fn is_identity(&self) -> bool {
        self.step == 1.0
    }

    /// Append `input` (interleaved stereo) resampled to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        if self.is_identity() {
            out.extend_from_slice(&input[..n * 2]);
            return;
        }
        let at = |i: isize, c: usize| -> f32 {
            if i < 0 {
                self.prev[c]
            } else {
                input[i as usize * 2 + c]
            }
        };
        while self.pos < (n - 1) as f64 {
            let i = self.pos.floor();
            let f = (self.pos - i) as f32;
            let i = i as isize;
            for c in 0..2 {
                let (a, b) = (at(i, c), at(i + 1, c));
                out.push(a + (b - a) * f);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = [input[(n - 1) * 2], input[(n - 1) * 2 + 1]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_conversion_keeps_length_and_shape() {
        let mut r = Linear::new(44_100, 48_000);
        let mut out = Vec::new();
        // 1 s of a 441 Hz ramp-free sine in 10 ms blocks.
        let tone: Vec<f32> = (0..44_100)
            .flat_map(|i| {
                let v = (i as f32 * 2.0 * std::f32::consts::PI * 441.0 / 44_100.0).sin();
                [v, -v]
            })
            .collect();
        for block in tone.chunks(441 * 2) {
            r.process(block, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
        // Output is still the same tone at the new rate.
        for k in (1000..47_000).step_by(997) {
            let want = (k as f32 * 2.0 * std::f32::consts::PI * 441.0 / 48_000.0).sin();
            assert!((out[k * 2] - want).abs() < 0.02, "sample {k}");
            assert_eq!(out[k * 2], -out[k * 2 + 1]);
        }
    }
}
