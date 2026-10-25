//! Adaptive audio jitter buffer, indexed by 5 ms frame number.
//!
//! The render thread pulls one frame at a time. The buffer holds back
//! `target` frames, sized from measured arrival jitter (transit time above
//! its recent minimum), so playout is as early as the network allows:
//!
//! - a missing frame with later ones present is concealed (Opus PLC);
//! - nothing buffered but packets arrived moments ago (the newest frames
//!   were lost): conceal and keep going; only a stream that has gone quiet
//!   is an underrun (silence, then re-prime);
//! - below target, one concealed frame stretches the buffer;
//! - the minimum depth over 200 ms above target: drop one frame (slack
//!   left by a stretch, clock drift);
//! - far above target for 100 ms (a spike that passed, a device stall):
//!   drop back to target + 1 in one step.
//!
//! The target follows the 98th percentile of jitter over the last 2-4 s, so
//! one stall of the receive path doesn't hold extra delay for seconds.

use proto::audio::{FRAME_US, MAX_PACKET};

const SLOTS: usize = 64;
/// Jitter is not measured for the first second of a stream: the receiver's
/// own start-up (decoder creation, first keyframe) stalls it once, and that
/// says nothing about the network.
const WARMUP_FRAMES: u32 = 200;
const START_TARGET: u32 = 2;
/// Jitter statistics are kept over two windows of this length.
const WINDOW_US: u64 = 2_000_000;
/// Margin added to the measured jitter before rounding up to frames.
const MARGIN_US: u64 = 1_500;
const MAX_TARGET: u32 = 20;
/// Depth above target + this for `EXCESS_POPS` pops (100 ms) is cut back.
const EXCESS_FRAMES: u32 = 4;
/// Window for the minimum-depth filter (200 ms of pops).
const DEPTH_WINDOW: u32 = 40;
const EXCESS_POPS: u32 = 20;
/// Jitter histogram: 1 ms buckets.
const BUCKETS: usize = 128;
/// Fraction of samples the target may ignore (the worst 2%: one ~80 ms
/// receive stall per 4 s doesn't raise it; repeated stalls do).
const IGNORE: f32 = 0.02;
/// At most one stretch per this many pops (100 ms), so a buffer that is
/// running dry still reaches an underrun instead of concealing forever.
const STRETCH_EVERY: u32 = 20;

struct Slot {
    seq: u32,
    valid: bool,
    capture_us: u64,
    len: u16,
    data: [u8; MAX_PACKET],
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pop<'a> {
    /// Decode this packet; its first sample was captured at `capture_us`.
    Packet(&'a [u8], u64),
    /// Conceal one frame (lost, or stretching toward the target).
    Conceal,
    /// Nothing to play: output silence.
    Empty,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub played: u64,
    pub concealed: u64,
    pub stretched: u64,
    pub skipped: u64,
    pub underruns: u64,
    pub late: u64,
    pub resets: u64,
}

pub struct JitterBuffer {
    slots: Vec<Slot>,
    /// Next frame to play; None while priming.
    next: Option<u32>,
    newest: Option<u32>,
    primed: u32,
    target: u32,
    excess_pops: u32,
    pops_since_stretch: u32,
    /// Pops since a new (newest) frame arrived.
    pops_since_push: u32,
    min_depth: u32,
    depth_pops: u32,
    /// Newest frames received since the stream (re)started.
    received: u32,
    warmup: u32,
    // Transit-time statistics: (current, previous) window.
    transit_min: [i64; 2],
    hist: [[u16; BUCKETS]; 2],
    window_start: u64,
    pub stats: Stats,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// `a - b` for wrapping frame numbers.
fn diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

impl JitterBuffer {
    pub fn new() -> Self {
        JitterBuffer {
            slots: (0..SLOTS)
                .map(|_| Slot {
                    seq: 0,
                    valid: false,
                    capture_us: 0,
                    len: 0,
                    data: [0; MAX_PACKET],
                })
                .collect(),
            next: None,
            newest: None,
            primed: 0,
            target: START_TARGET,
            excess_pops: 0,
            pops_since_stretch: STRETCH_EVERY,
            pops_since_push: 0,
            min_depth: u32::MAX,
            depth_pops: 0,
            received: 0,
            warmup: WARMUP_FRAMES,
            transit_min: [i64::MAX; 2],
            hist: [[0; BUCKETS]; 2],
            window_start: 0,
            stats: Stats::default(),
        }
    }

    pub fn target_frames(&self) -> u32 {
        self.target
    }

    /// Frames buffered ahead of playout.
    pub fn depth(&self) -> u32 {
        match (self.next, self.newest) {
            (Some(n), Some(w)) => (diff(w, n) + 1).max(0) as u32,
            _ => 0,
        }
    }

    fn reset(&mut self) {
        for s in &mut self.slots {
            s.valid = false;
        }
        self.next = None;
        self.newest = None;
        self.primed = 0;
        // A restarted stream has a new timeline: forget its jitter history.
        self.transit_min = [i64::MAX; 2];
        self.hist = [[0; BUCKETS]; 2];
        self.target = START_TARGET;
        self.received = 0;
        self.stats.resets += 1;
    }

    /// `primary`: the datagram's own frame (not a redundant copy). Only a
    /// primary that misses its playout counts as late.
    pub fn push(
        &mut self,
        seq: u32,
        capture_us: u64,
        arrival_us: u64,
        packet: &[u8],
        primary: bool,
    ) {
        if packet.len() > MAX_PACKET {
            return;
        }
        if let Some(n) = self.next {
            let d = diff(seq, n);
            if d < -(SLOTS as i32) || d >= SLOTS as i32 {
                // The host restarted or we fell far behind: start over.
                self.reset();
            } else if d < 0 {
                if primary {
                    self.stats.late += 1;
                }
                return;
            }
        } else if let Some(w) = self.newest {
            if diff(seq, w).unsigned_abs() as usize >= SLOTS {
                self.reset();
            }
        }
        let slot = &mut self.slots[seq as usize % SLOTS];
        if slot.valid && slot.seq == seq {
            return; // duplicate (redundant copy)
        }
        slot.seq = seq;
        slot.valid = true;
        slot.capture_us = capture_us;
        slot.len = packet.len() as u16;
        slot.data[..packet.len()].copy_from_slice(packet);
        if self.newest.is_none_or(|w| diff(seq, w) > 0) {
            self.newest = Some(seq);
            self.pops_since_push = 0;
            self.received = self.received.saturating_add(1);
            if self.received > self.warmup {
                self.observe(capture_us, arrival_us);
            }
        }
        if self.next.is_none() {
            self.primed += 1;
        }
    }

    /// Track transit jitter (newest frames only, so redundant copies don't
    /// count as late arrivals) and derive the target depth.
    fn observe(&mut self, capture_us: u64, arrival_us: u64) {
        if arrival_us.saturating_sub(self.window_start) > WINDOW_US {
            self.transit_min = [i64::MAX, self.transit_min[0]];
            self.hist = [[0; BUCKETS], self.hist[0]];
            self.window_start = arrival_us;
        }
        let transit = arrival_us as i64 - capture_us as i64;
        self.transit_min[0] = self.transit_min[0].min(transit);
        let floor = self.transit_min[0].min(self.transit_min[1]);
        let jitter_ms = ((transit - floor).max(0) / 1000) as usize;
        let b = &mut self.hist[0][jitter_ms.min(BUCKETS - 1)];
        *b = b.saturating_add(1);
        // 98th percentile over both windows, scanning down from the top.
        let total: u32 = (0..BUCKETS)
            .map(|i| self.hist[0][i] as u32 + self.hist[1][i] as u32)
            .sum();
        let allowed = (total as f32 * IGNORE) as u32;
        let mut above = 0u32;
        let mut p99_ms = 0;
        for i in (0..BUCKETS).rev() {
            above += self.hist[0][i] as u32 + self.hist[1][i] as u32;
            if above > allowed {
                p99_ms = i;
                break;
            }
        }
        // Bucket i covers [i, i+1) ms: use its upper edge.
        let peak = (p99_ms as u64 + 1) * 1000;
        self.target = ((peak + MARGIN_US).div_ceil(FRAME_US) as u32).clamp(1, MAX_TARGET);
    }

    /// The next 5 ms of playout.
    pub fn pop(&mut self) -> Pop<'_> {
        let n = match self.next {
            Some(n) => n,
            None => {
                let Some(w) = self.newest.filter(|_| self.primed >= self.target) else {
                    return Pop::Empty;
                };
                // Start `target` frames behind the newest.
                let n = w.wrapping_sub(self.target - 1);
                self.next = Some(n);
                n
            }
        };
        let depth = self.depth();
        self.pops_since_push = self.pops_since_push.saturating_add(1);
        if depth == 0 {
            if self.pops_since_push <= self.target + 1 {
                // The stream is alive; its newest frames were lost.
                self.next = Some(n.wrapping_add(1));
                self.stats.concealed += 1;
                return Pop::Conceal;
            }
            self.stats.underruns += 1;
            self.next = None;
            self.primed = 0;
            return Pop::Empty;
        }
        // Minimum-depth filter: slack that persists for 200 ms is removed
        // one frame at a time.
        self.min_depth = self.min_depth.min(depth);
        self.depth_pops += 1;
        if self.depth_pops >= DEPTH_WINDOW {
            let excess = self.min_depth > self.target;
            self.depth_pops = 0;
            self.min_depth = u32::MAX;
            if excess {
                self.slots[n as usize % SLOTS].valid = false;
                self.stats.skipped += 1;
                self.next = Some(n.wrapping_add(1));
                return self.pop();
            }
        }
        if depth > self.target + EXCESS_FRAMES {
            self.excess_pops += 1;
            if self.excess_pops >= EXCESS_POPS {
                // Drop the oldest frames so `target + 1` remain.
                self.excess_pops = 0;
                let drop = depth - (self.target + 1);
                for k in 0..drop {
                    self.slots[n.wrapping_add(k) as usize % SLOTS].valid = false;
                }
                self.stats.skipped += drop as u64;
                self.next = Some(n.wrapping_add(drop));
                return self.pop();
            }
        } else {
            self.excess_pops = 0;
        }
        self.pops_since_stretch = self.pops_since_stretch.saturating_add(1);
        if depth < self.target && depth <= 1 && self.pops_since_stretch >= STRETCH_EVERY {
            self.pops_since_stretch = 0;
            // Running dry against a raised target: buy one frame now.
            self.stats.stretched += 1;
            return Pop::Conceal;
        }
        self.next = Some(n.wrapping_add(1));
        let slot = &mut self.slots[n as usize % SLOTS];
        if slot.valid && slot.seq == n {
            slot.valid = false;
            self.stats.played += 1;
            Pop::Packet(&slot.data[..slot.len as usize], slot.capture_us)
        } else {
            self.stats.concealed += 1;
            Pop::Conceal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No warm-up, start at 1 frame: tests drive the adaptation directly.
    fn jb() -> JitterBuffer {
        let mut jb = JitterBuffer::new();
        jb.warmup = 0;
        jb.target = 1;
        jb
    }

    fn push(jb: &mut JitterBuffer, seq: u32, arrival_us: u64) {
        jb.push(seq, seq as u64 * FRAME_US, arrival_us, &[seq as u8], true);
    }

    #[test]
    fn steady_stream_plays_in_order_with_minimal_depth() {
        let mut jb = jb();
        for seq in 0..100u32 {
            push(&mut jb, seq, seq as u64 * FRAME_US + 300);
            match jb.pop() {
                Pop::Packet(p, _) => assert_eq!(p, &[seq as u8]),
                other => panic!("{seq}: {other:?}"),
            }
        }
        assert_eq!(jb.target_frames(), 1);
        assert_eq!(jb.stats.concealed + jb.stats.underruns, 0);
    }

    #[test]
    fn loss_is_concealed_and_late_packets_dropped() {
        let mut jb = jb();
        push(&mut jb, 0, 300);
        assert!(matches!(jb.pop(), Pop::Packet(..)));
        push(&mut jb, 2, 2 * FRAME_US + 300); // 1 lost
        assert_eq!(jb.pop(), Pop::Conceal);
        assert!(matches!(jb.pop(), Pop::Packet(&[2], _)));
        push(&mut jb, 1, 3 * FRAME_US); // too late now
        assert_eq!(jb.stats.late, 1);
    }

    #[test]
    fn jitter_raises_target_and_underrun_reprimes() {
        let mut jb = jb();
        // Every 4th frame arrives 12 ms late.
        for seq in 0..40u32 {
            let late = if seq % 4 == 3 { 12_000 } else { 0 };
            push(&mut jb, seq, seq as u64 * FRAME_US + 300 + late);
        }
        assert_eq!(jb.target_frames(), 3, "12 ms + margin -> 3 frames");
        // Nothing else arrives: drain, then an underrun.
        let mut empties = 0;
        for _ in 0..60 {
            if jb.pop() == Pop::Empty {
                empties += 1;
            }
        }
        assert!(empties > 0 && jb.stats.underruns == 1);
        // Re-priming waits for `target` new frames.
        push(&mut jb, 40, 0);
        assert_eq!(jb.pop(), Pop::Empty);
    }

    #[test]
    fn one_stall_does_not_set_the_target_and_excess_drains_fast() {
        let mut jb = jb();
        // 4 s steady, with one 60 ms receive stall (12 frames arrive at once).
        for seq in 0..800u32 {
            let t = seq as u64 * FRAME_US + 300;
            let arrival = if (100..112).contains(&seq) {
                112 * FRAME_US
            } else {
                t
            };
            push(&mut jb, seq, arrival);
            jb.pop();
        }
        assert!(jb.target_frames() <= 2, "target {}", jb.target_frames());
        assert!(jb.depth() <= jb.target_frames() + 1, "depth {}", jb.depth());
    }

    #[test]
    fn startup_stall_is_ignored() {
        let mut jb = JitterBuffer::new();
        // The first 100 ms arrive in one 80 ms-late burst (receiver start-up).
        for seq in 0..20u32 {
            push(&mut jb, seq, 20 * FRAME_US + 80_000);
        }
        assert_eq!(jb.target_frames(), START_TARGET);
    }

    #[test]
    fn slack_is_removed_and_lost_head_is_concealed() {
        let mut jb = jb();
        // 6 frames queue up before playout starts (on time, so no jitter),
        // then a steady stream: depth 6 vs target 1.
        for seq in 0..6u32 {
            push(&mut jb, seq, seq as u64 * FRAME_US);
        }
        for seq in 6..300u32 {
            push(&mut jb, seq, seq as u64 * FRAME_US);
            jb.pop();
        }
        assert!(jb.depth() <= 2, "slack left: depth {}", jb.depth());
        // The next frame is lost: concealed, not an underrun.
        assert_eq!(jb.stats.underruns, 0);
        while jb.depth() > 0 {
            jb.pop();
        }
        assert_eq!(jb.pop(), Pop::Conceal);
        push(&mut jb, 301, 301 * FRAME_US);
        assert!(matches!(jb.pop(), Pop::Packet(&[45], _)), "301 as u8 = 45");
        assert_eq!(jb.stats.underruns, 0);
    }

    #[test]
    fn restart_resets() {
        let mut jb = jb();
        push(&mut jb, 1000, 0);
        assert!(matches!(jb.pop(), Pop::Packet(..)));
        push(&mut jb, 3, 0); // host restarted its counter
        assert_eq!(jb.stats.resets, 1);
        assert!(matches!(jb.pop(), Pop::Packet(&[3], _)));
    }
}
