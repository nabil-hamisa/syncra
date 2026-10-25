//! Recently sent video chunks, kept for NACK retransmission.
//!
//! The encode thread records each chunk's datagrams right after sending; the
//! receive thread answers a `Control::Nack` by resending the requested data
//! shards, flagged `RETRANSMIT` (excluded from loss and delay statistics) and
//! re-stamped with the current send time. Entries older than `KEEP` are
//! recycled, so steady state allocates nothing.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use proto::{clock, flags, Header, HEADER_LEN};

use crate::socket::Socket;

/// Longer than any useful NACK round trip: older data would arrive too late.
const KEEP: Duration = Duration::from_millis(200);
const MAX_ENTRIES: usize = 1024;

struct Entry {
    frame: u32,
    chunk: u16,
    segment: usize,
    buf: Vec<u8>,
    at: Instant,
}

#[derive(Default)]
pub struct SendHistory {
    entries: Mutex<VecDeque<Entry>>,
}

impl SendHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember the datagrams (`buf`, all `segment` bytes) of one chunk.
    pub fn record(&self, frame: u32, chunk: u16, buf: &[u8], segment: usize) {
        let now = Instant::now();
        let mut q = self.entries.lock().unwrap();
        let mut recycled = None;
        while q.front().is_some_and(|e| now - e.at > KEEP) || q.len() >= MAX_ENTRIES {
            recycled = q.pop_front().map(|e| e.buf);
        }
        let mut b = recycled.unwrap_or_default();
        b.clear();
        b.extend_from_slice(buf);
        q.push_back(Entry {
            frame,
            chunk,
            segment,
            buf: b,
            at: now,
        });
    }

    /// Resend data shards `base + i` (bit i of `mask`; `u128::MAX` = all) of
    /// a recorded chunk. Returns how many datagrams went out.
    pub fn resend(
        &self,
        socket: &Socket,
        to: SocketAddr,
        frame: u32,
        chunk: u16,
        base: u16,
        mask: u128,
    ) -> usize {
        let mut out: Vec<Vec<u8>> = Vec::new();
        {
            let q = self.entries.lock().unwrap();
            let Some(e) = q
                .iter()
                .rev()
                .find(|e| e.frame == frame && e.chunk == chunk)
            else {
                return 0;
            };
            let Some((h, _)) = Header::read(&e.buf[..e.segment.min(e.buf.len())]) else {
                return 0;
            };
            let k = h.frag_count as usize;
            for idx in 0..k {
                let wanted = if mask == u128::MAX {
                    true
                } else {
                    let i = idx as i64 - base as i64;
                    (0..128).contains(&i) && mask & (1u128 << i) != 0
                };
                let (start, end) = (idx * e.segment, (idx + 1) * e.segment);
                if wanted && end <= e.buf.len() {
                    out.push(e.buf[start..end].to_vec());
                }
            }
        }
        let send_us = clock::now_us();
        for d in &mut out {
            let mut f = u16::from_le_bytes([d[2], d[3]]);
            f |= flags::RETRANSMIT;
            d[2..4].copy_from_slice(&f.to_le_bytes());
            d[32..40].copy_from_slice(&send_us.to_le_bytes());
            debug_assert!(d.len() >= HEADER_LEN);
            let _ = socket.send_to(d, to);
        }
        out.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{ChunkMeta, VideoSender};
    use proto::Channel;

    #[test]
    fn resends_requested_shards_flagged() {
        let rx = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let tx = Socket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let hist = SendHistory::new();
        let meta = ChunkMeta {
            channel: Channel::Video,
            frame_id: 9,
            chunk_id: 2,
            last: true,
            keyframe: false,
            recovery: false,
            lost_acked: 0,
            capture_us: 0,
            fec_ratio: 0.0,
        };
        let data = vec![7u8; 5000];
        let mut s = VideoSender::new();
        let (buf, seg) = s.packetize(&meta, &data);
        hist.record(9, 2, buf, seg);

        assert_eq!(
            hist.resend(&tx, rx.local_addr().unwrap(), 9, 2, 1, 0b101),
            2
        ); // shards 1 and 3
        let mut d = [0u8; 2000];
        for want in [1u16, 3] {
            let (n, _) = rx.recv_from(&mut d).unwrap();
            let (h, _) = Header::read(&d[..n]).unwrap();
            assert_eq!(h.frag_index, want);
            assert!(h.flags & flags::RETRANSMIT != 0);
        }
        assert_eq!(
            hist.resend(&tx, rx.local_addr().unwrap(), 9, 3, 0, u128::MAX),
            0,
            "unknown chunk"
        );
        assert_eq!(
            hist.resend(&tx, rx.local_addr().unwrap(), 9, 2, 0, u128::MAX),
            5,
            "all data shards"
        );
    }
}
