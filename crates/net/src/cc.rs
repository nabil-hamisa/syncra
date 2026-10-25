//! Host-side congestion control and FEC sizing, driven by client feedback.
//!
//! Delay-based (in the spirit of GCC/SCReAM): the signal is queuing delay at
//! the bottleneck, so the rate backs off before the queue causes latency or
//! drops, and keeps a near-empty queue in steady state.
//!
//! - queue above `HIGH` (or heavy loss): cut to 85 % of the rate that actually
//!   arrived, at most once per RTT + 50 ms so the cut has time to take effect;
//! - queue below `LOW` and little loss: multiplicative ramp-up;
//! - in between: hold.
//!
//! Random loss alone does not cut the rate (FEC repairs it); only loss heavy
//! enough to look like congestion (policers, no queue) does. The FEC ratio
//! tracks a decaying peak of measured loss plus margin.

use std::time::{Duration, Instant};

use proto::Feedback;

/// Below this standing queue, ramp up; above HIGH_US, cut. In between, hold.
/// LOW sits above typical scheduling jitter (a few ms) so noise does not
/// pin the rate in the hold band.
const LOW_US: u32 = 2_500;
const HIGH_US: u32 = 5_000;
/// A single spike this large means a queue is building now.
const SPIKE_US: u32 = 20_000;
/// Loss this heavy with no queue looks like a policer, not random loss.
const HEAVY_LOSS: f32 = 0.30;
const DECREASE: f64 = 0.85;
/// Ramp per 100 ms without congestion: fast right after start, gentle near
/// the last rate that caused trouble.
const RAMP_FAST: f64 = 0.10;
/// Slow start: begin here (or at the max, if lower) and ramp this fast per
/// 100 ms until the first sign of congestion. Starting at the max flooded a
/// 2 Mbps uplink for the first second (350 ms queue, 42% loss).
const START_BPS: f64 = 2.5e6;
const RAMP_STARTUP: f64 = 0.25;
const RAMP_SLOW: f64 = 0.03;
/// FEC overhead at least this, and this many times the recent loss peak.
const FEC_BASE: f32 = 0.05;
const FEC_LOSS_GAIN: f32 = 2.5;
const FEC_MAX: f32 = 0.6;
/// Loss peak half-life for FEC sizing.
const FEC_DECAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub struct CcConfig {
    pub min_bps: f64,
    pub max_bps: f64,
    /// Fixed FEC ratio instead of adaptive (None = adaptive, Some(0) = off).
    pub fixed_fec: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CcSnapshot {
    pub rate_bps: f64,
    pub fec_ratio: f32,
    pub loss: f32,
    pub qdelay_us: u32,
    pub recv_bps: f64,
    pub rtt_us: u32,
}

pub struct CongestionController {
    cfg: CcConfig,
    rate: f64,
    /// No congestion seen yet: ramp at `RAMP_STARTUP`.
    startup: bool,
    /// Rate at the last decrease: ramp slowly when approaching it.
    last_trouble_rate: f64,
    last_decrease: Option<Instant>,
    loss_ewma: f32,
    /// Loss measured while the queue was short (drives FEC).
    random_loss_ewma: f32,
    loss_peak: f32,
    loss_peak_at: Instant,
    qdelay_ewma: f32,
    recv_bps: f64,
    rtt_us: u32,
    last_update: Option<Instant>,
}

impl CongestionController {
    pub fn new(cfg: CcConfig, now: Instant) -> Self {
        CongestionController {
            cfg,
            rate: START_BPS.clamp(cfg.min_bps, cfg.max_bps),
            startup: true,
            last_trouble_rate: 0.0,
            last_decrease: None,
            loss_ewma: 0.0,
            random_loss_ewma: 0.0,
            loss_peak: 0.0,
            loss_peak_at: now,
            qdelay_ewma: 0.0,
            recv_bps: 0.0,
            rtt_us: 0,
            last_update: None,
        }
    }

    pub fn on_rtt(&mut self, rtt_us: u32) {
        self.rtt_us = if self.rtt_us == 0 {
            rtt_us
        } else {
            (self.rtt_us * 7 + rtt_us) / 8
        };
    }

    pub fn on_feedback(&mut self, f: &Feedback, now: Instant) {
        let dt = self
            .last_update
            .map_or(Duration::from_millis(10), |t| now - t);
        self.last_update = Some(now);

        // Drops while a queue is building are the bottleneck overflowing:
        // adding parity would only feed the congestion, so FEC sizing learns
        // only from loss seen with a short queue (random / wireless loss).
        let queue_building = self.qdelay_ewma > HIGH_US as f32 || f.qdelay_min_us > SPIKE_US;
        if f.expected > 0 {
            let loss = 1.0 - (f.received as f32 / f.expected as f32).min(1.0);
            self.loss_ewma = 0.9 * self.loss_ewma + 0.1 * loss;
            if !queue_building {
                self.random_loss_ewma = 0.9 * self.random_loss_ewma + 0.1 * loss;
                let decayed = self.loss_peak
                    * 0.5f32.powf(
                        now.duration_since(self.loss_peak_at).as_secs_f32()
                            / FEC_DECAY.as_secs_f32(),
                    );
                if loss >= decayed {
                    self.loss_peak = loss;
                    self.loss_peak_at = now;
                }
            }
        }
        if f.interval_us > 0 {
            let r = f.recv_bytes as f64 * 8.0 / (f.interval_us as f64 / 1e6);
            self.recv_bps = if self.recv_bps == 0.0 {
                r
            } else {
                0.8 * self.recv_bps + 0.2 * r
            };
        }
        // Standing queue only: the per-interval minimum ignores the
        // serialization delay of our own frame bursts.
        self.qdelay_ewma = 0.7 * self.qdelay_ewma + 0.3 * f.qdelay_min_us as f32;

        let congested = self.qdelay_ewma > HIGH_US as f32
            || f.qdelay_min_us > SPIKE_US
            || self.loss_ewma > HEAVY_LOSS;
        let settle = Duration::from_micros(self.rtt_us as u64) + Duration::from_millis(50);
        let may_cut = self.last_decrease.is_none_or(|t| now - t > settle);

        if congested {
            self.startup = false;
            if may_cut {
                // Cut relative to what actually got through, not to what we asked for.
                let base = if self.recv_bps > 0.0 {
                    self.recv_bps.min(self.rate)
                } else {
                    self.rate
                };
                self.last_trouble_rate = self.rate;
                self.rate = (base * DECREASE).max(self.cfg.min_bps);
                self.last_decrease = Some(now);
            }
        } else if self.qdelay_ewma < LOW_US as f32 && self.loss_ewma < HEAVY_LOSS / 2.0 {
            // Probe gently only in a band around the rate that last caused a queue.
            let t = self.last_trouble_rate;
            let near_trouble = t > 0.0 && self.rate > 0.8 * t && self.rate < 1.25 * t;
            let ramp = if self.startup {
                RAMP_STARTUP
            } else if near_trouble {
                RAMP_SLOW
            } else {
                RAMP_FAST
            };
            let mut next =
                (self.rate * (1.0 + ramp).powf(dt.as_secs_f64() / 0.1)).min(self.cfg.max_bps);
            // Past the rate that last built a queue, only probe as far as the
            // traffic actually flowing supports: an application-limited sender
            // would otherwise ramp to a target it has never tested and flood
            // the link the moment it uses it.
            if t > 0.0 && next > t && self.recv_bps > 0.0 {
                next = next
                    .min((1.5 * self.recv_bps).max(t))
                    .max(self.rate.min(next));
            }
            self.rate = next;
        }
    }

    /// Still in slow start (no congestion seen yet): the rate is a probe,
    /// not a measurement of the link.
    pub fn in_startup(&self) -> bool {
        self.startup
    }

    pub fn rate_bps(&self) -> f64 {
        self.rate
    }

    pub fn fec_ratio(&self, now: Instant) -> f32 {
        if let Some(f) = self.cfg.fixed_fec {
            return f;
        }
        let peak = self.loss_peak
            * 0.5f32.powf(
                now.duration_since(self.loss_peak_at).as_secs_f32() / FEC_DECAY.as_secs_f32(),
            );
        (FEC_BASE + FEC_LOSS_GAIN * peak.max(self.random_loss_ewma)).min(FEC_MAX)
    }

    pub fn snapshot(&self, now: Instant) -> CcSnapshot {
        CcSnapshot {
            rate_bps: self.rate,
            fec_ratio: self.fec_ratio(now),
            loss: self.loss_ewma,
            qdelay_us: self.qdelay_ewma as u32,
            recv_bps: self.recv_bps,
            rtt_us: self.rtt_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc() -> (CongestionController, Instant) {
        let t = Instant::now();
        (
            CongestionController::new(
                CcConfig {
                    min_bps: 2e6,
                    max_bps: 30e6,
                    fixed_fec: None,
                },
                t,
            ),
            t,
        )
    }

    fn fb(expected: u32, received: u32, mbps: f64, qavg: u32, qmax: u32) -> Feedback {
        Feedback {
            interval_us: 10_000,
            expected,
            received,
            recv_bytes: (mbps * 1e6 / 8.0 / 100.0) as u32,
            qdelay_max_us: qmax,
            qdelay_avg_us: qavg,
            qdelay_min_us: qavg,
            last_frame: 0,
        }
    }

    #[test]
    fn queue_growth_cuts_toward_received_rate() {
        let (mut c, mut t) = cc();
        for _ in 0..20 {
            t += Duration::from_millis(10);
            c.on_feedback(&fb(100, 100, 10.0, 15_000, 18_000), t);
        }
        assert!(
            c.rate_bps() <= 10e6 * DECREASE + 1.0,
            "rate {}",
            c.rate_bps()
        );
        assert!(c.rate_bps() >= 2e6);
    }

    #[test]
    fn random_loss_does_not_cut_rate_but_raises_fec() {
        let (mut c, mut t) = cc();
        for i in 0..200 {
            t += Duration::from_millis(10);
            let lost = if i % 2 == 0 { 10 } else { 0 }; // ~5 % loss, empty queue
            c.on_feedback(&fb(100, 100 - lost, 30.0, 200, 500), t);
        }
        assert_eq!(c.rate_bps(), 30e6);
        let fec = c.fec_ratio(t);
        assert!(fec > 0.2 && fec <= FEC_MAX, "fec {fec}");
        // Loss stops: FEC decays back toward the base.
        for _ in 0..1000 {
            t += Duration::from_millis(10);
            c.on_feedback(&fb(100, 100, 30.0, 200, 500), t);
        }
        assert!(c.fec_ratio(t) < 0.08, "fec {}", c.fec_ratio(t));
    }

    #[test]
    fn own_burst_serialization_is_not_congestion() {
        let (mut c, mut t) = cc();
        for _ in 0..300 {
            t += Duration::from_millis(10);
            // Frame bursts: high average and max, but the first datagram of
            // each burst sees an empty queue.
            let f = Feedback {
                qdelay_min_us: 0,
                ..fb(100, 100, 18.0, 3_000, 30_000)
            };
            c.on_feedback(&f, t);
        }
        assert_eq!(c.rate_bps(), 30e6, "no cut without a standing queue");
    }

    #[test]
    fn congestion_drops_do_not_inflate_fec() {
        let (mut c, mut t) = cc();
        // Bottleneck overflowing: long queue and 40 % drops.
        for _ in 0..200 {
            t += Duration::from_millis(10);
            c.on_feedback(&fb(100, 60, 20.0, 40_000, 50_000), t);
        }
        assert!(c.rate_bps() < 20e6, "rate cut below the bottleneck");
        assert!(
            c.fec_ratio(t) < 0.1,
            "fec stayed near base: {}",
            c.fec_ratio(t)
        );
    }

    #[test]
    fn recovers_after_congestion_ends() {
        let (mut c, mut t) = cc();
        for _ in 0..30 {
            t += Duration::from_millis(10);
            c.on_feedback(&fb(100, 100, 5.0, 20_000, 30_000), t);
        }
        let low = c.rate_bps();
        assert!(low < 6e6);
        for _ in 0..600 {
            t += Duration::from_millis(10);
            c.on_feedback(&fb(100, 100, c.rate_bps() / 1e6, 300, 800), t);
        }
        assert!(c.rate_bps() > 25e6, "ramped back to {}", c.rate_bps());
    }
}
