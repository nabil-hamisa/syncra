//! Receiver-side statistics for congestion control and FEC, sent to the host
//! every [`INTERVAL`] as `Control::Feedback`.
//!
//! Queuing delay is measured from one-way delay (`arrival - send_us`) minus
//! its recent minimum. Any constant clock offset between host and client
//! cancels out, so this works across machines without clock sync; the
//! two-bucket windowed minimum also absorbs slow clock drift.
//!
//! The reported minimum comes from each frame's *first* datagram when the
//! interval has one: later datagrams of a frame wait behind the earlier ones
//! at a slow bottleneck (a 10 KB frame takes 40 ms at 2 Mbps), which is our
//! own burst serializing, not a standing queue.

use std::time::Duration;

use proto::{flags, Feedback, Header};

pub const INTERVAL: Duration = Duration::from_millis(10);
/// The base-delay minimum is taken over the last 5-10 s.
const BASE_WINDOW_US: u64 = 5_000_000;

#[derive(Default)]
pub struct FeedbackBuilder {
    interval_start_us: u64,
    /// Next sequence number expected at the start of this interval.
    seq_base: Option<u32>,
    seq_max: u32,
    received: u32,
    recv_bytes: u32,
    qdelay_max: u32,
    qdelay_min: u32,
    qdelay_sum: u64,
    samples: u32,
    /// Minimum over frame-start datagrams this interval (None: none seen).
    start_min: Option<u32>,
    /// Last frame-start queuing delay, for intervals without a frame start.
    last_start: Option<u32>,
    last_frame: u32,
    /// Windowed minimum of one-way delay: (current bucket, previous bucket).
    base: [i64; 2],
    base_bucket_start: u64,
}

impl FeedbackBuilder {
    pub fn new(now_us: u64) -> Self {
        FeedbackBuilder {
            interval_start_us: now_us,
            base: [i64::MAX; 2],
            base_bucket_start: now_us,
            ..Default::default()
        }
    }

    /// Account one received video datagram.
    pub fn on_video(&mut self, h: &Header, bytes: usize, arrival_us: u64) {
        if h.flags & flags::RETRANSMIT != 0 {
            return;
        }
        let base = *self.seq_base.get_or_insert(h.sequence);
        if (h.sequence.wrapping_sub(base) as i32) < 0 {
            return; // straggler from a previous interval: not counted twice
        }
        if self.received == 0 || (h.sequence.wrapping_sub(self.seq_max) as i32) > 0 {
            self.seq_max = h.sequence;
        }
        self.received += 1;
        self.recv_bytes = self.recv_bytes.saturating_add(bytes as u32);

        let owd = arrival_us as i64 - h.send_us as i64;
        if arrival_us.saturating_sub(self.base_bucket_start) > BASE_WINDOW_US {
            self.base = [i64::MAX, self.base[0]];
            self.base_bucket_start = arrival_us;
        }
        self.base[0] = self.base[0].min(owd);
        let floor = self.base[0].min(self.base[1]);
        let q = (owd - floor).clamp(0, u32::MAX as i64) as u32;
        self.qdelay_max = self.qdelay_max.max(q);
        self.qdelay_min = if self.samples == 0 {
            q
        } else {
            self.qdelay_min.min(q)
        };
        self.qdelay_sum += q as u64;
        self.samples += 1;
        if h.chunk_id == 0 && h.frag_index == 0 {
            self.start_min = Some(self.start_min.map_or(q, |m| m.min(q)));
            self.last_start = Some(q);
        }
    }

    pub fn on_frame_complete(&mut self, frame_id: u32) {
        self.last_frame = frame_id;
    }

    /// Close the interval if it is due and return its report.
    pub fn poll(&mut self, now_us: u64) -> Option<Feedback> {
        let elapsed = now_us.saturating_sub(self.interval_start_us);
        if elapsed < INTERVAL.as_micros() as u64 {
            return None;
        }
        let expected = match self.seq_base {
            Some(base) if self.received > 0 => self
                .seq_max
                .wrapping_sub(base)
                .wrapping_add(1)
                .max(self.received),
            _ => 0,
        };
        let f = Feedback {
            interval_us: elapsed.min(u32::MAX as u64) as u32,
            expected,
            received: self.received,
            recv_bytes: self.recv_bytes,
            qdelay_max_us: self.qdelay_max,
            // Burst-free when possible: frame starts, else the last frame
            // start, else (no frame structure, e.g. tests) all datagrams.
            qdelay_min_us: self
                .start_min
                .or(self.last_start.filter(|_| self.samples > 0))
                .map_or(self.qdelay_min, |m| m.min(self.qdelay_max)),
            qdelay_avg_us: if self.samples > 0 {
                (self.qdelay_sum / self.samples as u64) as u32
            } else {
                0
            },
            last_frame: self.last_frame,
        };
        self.interval_start_us = now_us;
        if self.received > 0 {
            self.seq_base = Some(self.seq_max.wrapping_add(1));
        }
        self.received = 0;
        self.recv_bytes = 0;
        self.qdelay_max = 0;
        self.qdelay_min = 0;
        self.qdelay_sum = 0;
        self.samples = 0;
        self.start_min = None;
        Some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::Channel;

    fn hdr(seq: u32, send_us: u64) -> Header {
        Header {
            channel: Channel::Video as u8,
            sequence: seq,
            send_us,
            ..Default::default()
        }
    }

    #[test]
    fn counts_loss_from_sequence_gaps() {
        let mut fb = FeedbackBuilder::new(0);
        for seq in (0..100).filter(|s| s % 10 != 3) {
            fb.on_video(&hdr(seq, 1000), 1000, 1500);
        }
        let f = fb.poll(10_000).unwrap();
        assert_eq!((f.expected, f.received), (100, 90));
        // Next interval continues from 100.
        fb.on_video(&hdr(101, 2000), 1000, 2500);
        let f = fb.poll(20_000).unwrap();
        assert_eq!((f.expected, f.received), (2, 1));
    }

    #[test]
    fn queuing_delay_ignores_clock_offset() {
        // Host clock is 1 s ahead of the client: one-way delay is negative.
        let mut fb = FeedbackBuilder::new(0);
        fb.on_video(&hdr(0, 1_000_500), 1200, 500); // base
        fb.on_video(&hdr(1, 1_001_000), 1200, 1000 + 7_000); // 7 ms queued
        let f = fb.poll(10_000).unwrap();
        assert_eq!(f.qdelay_max_us, 7_000);
        assert_eq!(f.qdelay_avg_us, 3_500);
        assert_eq!(f.qdelay_min_us, 0);
    }

    #[test]
    fn queue_delay_ignores_own_burst_serialization() {
        // A 10-datagram frame through a slow link: each datagram waits 4 ms
        // behind the previous one. No standing queue, so the minimum is 0.
        let mut fb = FeedbackBuilder::new(0);
        fb.on_video(&hdr(0, 0), 1200, 1000); // base, earlier frame
        for i in 0..10u32 {
            let mut h = hdr(1 + i, 100_000);
            h.frame_id = 1;
            h.frag_index = i as u16;
            fb.on_video(&h, 1200, 101_000 + i as u64 * 4_000);
        }
        let f = fb.poll(140_000).unwrap();
        assert_eq!(f.qdelay_min_us, 0);
        // Next interval holds only the frame's tail: still no queue reported.
        let mut h = hdr(11, 100_000);
        h.frag_index = 10;
        fb.on_video(&h, 1200, 141_000);
        assert_eq!(fb.poll(150_000).unwrap().qdelay_min_us, 0);
    }

    #[test]
    fn retransmits_are_excluded() {
        let mut fb = FeedbackBuilder::new(0);
        let mut h = hdr(0, 0);
        h.flags = flags::RETRANSMIT;
        fb.on_video(&h, 100, 10);
        let f = fb.poll(10_000).unwrap();
        assert_eq!((f.expected, f.received), (0, 0));
    }
}
