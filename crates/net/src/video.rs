//! Chunk packetizer (host) and reassembler (client), used for video and for
//! cursor shapes (any message larger than one datagram).
//!
//! Each chunk becomes `k` data shards plus `m` Reed-Solomon parity shards
//! (`fec`), all the same size so the whole chunk leaves with one USO send.
//! The receiver rebuilds a chunk as soon as any `k` shards have arrived.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::SocketAddr;

use proto::{clock, flags, Channel, Header, HEADER_LEN};

use crate::fec::{self, FecDecoder, FecEncoder};
use crate::socket::Socket;

/// Splits encoder output chunks into datagrams and sends them.
pub struct VideoSender {
    sequence: u32,
    buf: Vec<u8>,
    fec: FecEncoder,
}

pub struct ChunkMeta {
    pub channel: Channel,
    pub frame_id: u32,
    pub chunk_id: u16,
    pub last: bool,
    pub keyframe: bool,
    /// The frame references none of the frames reported lost up to `lost_acked`.
    pub recovery: bool,
    pub lost_acked: u32,
    pub capture_us: u64,
    /// Parity shards per data shard (0 = no FEC).
    pub fec_ratio: f32,
}

impl Default for VideoSender {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoSender {
    pub fn new() -> Self {
        VideoSender {
            sequence: 0,
            buf: Vec::with_capacity(1 << 20),
            fec: FecEncoder::new(),
        }
    }

    /// Build the datagrams for one chunk into a contiguous buffer laid out for
    /// USO. Every datagram is the same size, which is returned with the buffer.
    pub fn packetize(&mut self, meta: &ChunkMeta, data: &[u8]) -> (&[u8], usize) {
        let k = fec::data_shards(data.len());
        let s = fec::shard_bytes(data.len(), k);
        let m = fec::parity_shards(k, meta.fec_ratio);
        assert!(k + m <= u16::MAX as usize, "chunk too large");
        let segment = HEADER_LEN + s;

        let mut f = 0;
        if meta.last {
            f |= flags::LAST_CHUNK;
        }
        if meta.keyframe {
            f |= flags::KEYFRAME;
        }
        if meta.recovery {
            f |= flags::RECOVERY;
        }
        let header = |index: usize, sequence: u32| Header {
            channel: meta.channel as u8,
            flags: f,
            sequence,
            frame_id: meta.frame_id,
            chunk_id: meta.chunk_id,
            frag_index: index as u16,
            frag_count: k as u16,
            parity_count: m as u16,
            chunk_len: data.len() as u32,
            capture_us: meta.capture_us,
            send_us: clock::now_us(),
            lost_acked: meta.lost_acked,
        };

        self.buf.clear();
        self.buf.resize((k + m) * segment, 0);
        for i in 0..k {
            let d = &mut self.buf[i * segment..(i + 1) * segment];
            header(i, self.sequence).write(&mut d[..HEADER_LEN]);
            self.sequence = self.sequence.wrapping_add(1);
            let start = (i * s).min(data.len());
            let end = (start + s).min(data.len());
            // The last shard's tail stays zero: shards are padded to `s`.
            d[HEADER_LEN..HEADER_LEN + end - start].copy_from_slice(&data[start..end]);
        }
        let (buf, seq) = (&mut self.buf, &mut self.sequence);
        self.fec.encode(data, m, |i, parity| {
            let d = &mut buf[(k + i) * segment..(k + i + 1) * segment];
            header(k + i, *seq).write(&mut d[..HEADER_LEN]);
            *seq = seq.wrapping_add(1);
            d[HEADER_LEN..].copy_from_slice(parity);
        });
        (&self.buf, segment)
    }

    pub fn send(
        &mut self,
        socket: &Socket,
        to: SocketAddr,
        meta: &ChunkMeta,
        data: &[u8],
    ) -> io::Result<()> {
        let (buf, segment) = self.packetize(meta, data);
        socket.send_segments(buf, segment, to)
    }
}

/// A chunk ready for the decoder, in bitstream order.
#[derive(Debug)]
pub struct ReadyChunk {
    pub frame_id: u32,
    pub chunk_id: u16,
    pub data: Vec<u8>,
    pub last: bool,
    pub keyframe: bool,
    pub recovery: bool,
    pub lost_acked: u32,
    pub capture_us: u64,
    /// Host send time of the chunk's datagrams.
    pub send_us: u64,
}

#[derive(Debug)]
pub enum Event {
    Chunk(ReadyChunk),
    /// Frames `first..=last` could not be completed and were dropped.
    FrameLost {
        first: u32,
        last: u32,
    },
    /// Ask the host to resend data shards `base + i` (bit i of `mask`) of a
    /// stalled chunk (`mask == u128::MAX` with nothing received: resend all).
    Nack {
        frame_id: u32,
        chunk_id: u16,
        base: u16,
        mask: u128,
    },
}

/// Reassembly counters, cumulative.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReassemblyStats {
    /// Chunks that needed parity shards to complete.
    pub fec_recovered: u64,
    /// Frames dropped because they could not be completed.
    pub frames_lost: u64,
    /// Retransmission requests sent.
    pub nacks: u64,
}

struct ChunkAsm {
    data: Vec<u8>,
    have: Vec<bool>,
    have_count: usize,
    parity: Vec<Option<Vec<u8>>>,
    parity_count: usize,
    complete: bool,
    last: bool,
    send_us: u64,
}

#[derive(Default)]
struct FrameAsm {
    chunks: BTreeMap<u16, ChunkAsm>,
    keyframe: bool,
    recovery: bool,
    lost_acked: u32,
    capture_us: u64,
    /// Set once the LAST chunk is complete.
    last_chunk_complete: bool,
}

/// Reassembles datagrams into chunks and releases them strictly in order:
/// frame by frame, chunk by chunk, as soon as each chunk is complete
/// (directly, or rebuilt from parity).
///
/// With NACK enabled (`set_nack_wait`, for short RTTs), a chunk that stalls
/// while later data is already arriving gets one retransmission request, and
/// its frame is dropped only if the resend hasn't completed it within the
/// wait. Without NACK, a frame is dropped as soon as a later one completes.
#[derive(Default)]
pub struct Reassembler {
    frames: HashMap<u32, FrameAsm>,
    /// Frame currently being released, and the next chunk id expected in it.
    current: Option<(u32, u16)>,
    fec: FecDecoder,
    pub stats: ReassemblyStats,
    /// How long to wait for a retransmission (0 = NACK disabled).
    nack_wait_us: u64,
    /// Newest (frame, chunk) any datagram belonged to.
    newest_seen: Option<(u32, u16)>,
    /// Chunk last NACKed and when.
    nacked: Option<(u32, u16, u64)>,
    /// Oldest later frame that is complete while the current one is stuck.
    pending_drop: Option<u32>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable NACKs with this retransmission wait (None = disabled).
    pub fn set_nack_wait(&mut self, wait: Option<std::time::Duration>) {
        self.nack_wait_us = wait.map_or(0, |w| w.as_micros() as u64);
    }

    /// Time-based work: request resends for stalled chunks and give up on
    /// frames whose resend did not arrive in time. Call regularly.
    pub fn poll(&mut self, out: &mut Vec<Event>) {
        let now = clock::now_us();
        self.maybe_nack(now, out);
        self.maybe_drop(now, out);
        self.release(out);
    }

    fn maybe_nack(&mut self, now: u64, out: &mut Vec<Event>) {
        let (Some((f, c)), Some(newest)) = (self.current, self.newest_seen) else {
            return;
        };
        if self.nack_wait_us == 0 || self.nacked.is_some_and(|(nf, nc, _)| (nf, nc) == (f, c)) {
            return;
        }
        let chunk = self.frames.get(&f).and_then(|fr| fr.chunks.get(&c));
        if chunk.is_some_and(|ch| ch.complete) {
            return;
        }
        // NACK once later data has arrived (the stalled chunk's shards were
        // sent before it). A time-based "chunk went quiet" trigger was tried
        // (0.5 ms and 3 ms) and lost more frames at 10% bursty loss (46-53
        // vs 33-37 per 12 s): it spends the chunk's one NACK too early.
        if !after(newest, (f, c)) {
            return;
        }
        let (base, mask) = match chunk {
            Some(ch) => {
                let Some(first) = ch.have.iter().position(|h| !h) else {
                    return;
                };
                let mut mask = 0u128;
                for (i, h) in ch.have.iter().enumerate().skip(first).take(128) {
                    if !h {
                        mask |= 1 << (i - first);
                    }
                }
                (first as u16, mask)
            }
            None => (0, u128::MAX), // nothing of this chunk arrived
        };
        self.nacked = Some((f, c, now));
        self.stats.nacks += 1;
        out.push(Event::Nack {
            frame_id: f,
            chunk_id: c,
            base,
            mask,
        });
    }

    fn maybe_drop(&mut self, now: u64, out: &mut Vec<Event>) {
        let (Some(target), Some((f, c))) = (self.pending_drop, self.current) else {
            return;
        };
        if !frame_before(f, target) {
            self.pending_drop = None;
            return;
        }
        let expired = match self.nacked {
            Some((nf, nc, at)) if (nf, nc) == (f, c) => now.saturating_sub(at) > self.nack_wait_us,
            _ => self.nack_wait_us == 0,
        };
        if expired {
            self.drop_until(target, out);
        }
    }

    pub fn push(&mut self, h: &Header, payload: &[u8], out: &mut Vec<Event>) {
        let (cur_frame, next_chunk) = *self.current.get_or_insert((h.frame_id, 0));
        if frame_before(h.frame_id, cur_frame)
            || (h.frame_id == cur_frame && h.chunk_id < next_chunk)
        {
            return; // late shard for a chunk already released or dropped
        }
        let k = h.frag_count as usize;
        let m = h.parity_count as usize;
        let len = h.chunk_len as usize;
        if k == 0 || k != fec::data_shards(len) {
            return; // malformed
        }
        let s = fec::shard_bytes(len, k);

        let frame = self.frames.entry(h.frame_id).or_default();
        frame.keyframe |= h.flags & flags::KEYFRAME != 0;
        frame.recovery |= h.flags & flags::RECOVERY != 0;
        frame.lost_acked = h.lost_acked;
        frame.capture_us = h.capture_us;
        let chunk = frame.chunks.entry(h.chunk_id).or_insert_with(|| ChunkAsm {
            data: vec![0; len],
            have: vec![false; k],
            have_count: 0,
            parity: vec![None; m],
            parity_count: 0,
            complete: false,
            last: h.flags & flags::LAST_CHUNK != 0,
            send_us: h.send_us,
        });
        if chunk.complete
            || chunk.have.len() != k
            || chunk.parity.len() != m
            || chunk.data.len() != len
        {
            return; // already done, or inconsistent with earlier shards
        }
        let idx = h.frag_index as usize;
        if idx < k {
            if chunk.have[idx] {
                return; // duplicate
            }
            let start = (idx * s).min(len);
            let end = (start + s).min(len);
            if payload.len() < end - start {
                return;
            }
            chunk.data[start..end].copy_from_slice(&payload[..end - start]);
            chunk.have[idx] = true;
            chunk.have_count += 1;
        } else if idx < k + m {
            let p = idx - k;
            if chunk.parity[p].is_some() || payload.len() < s {
                return;
            }
            chunk.parity[p] = Some(payload[..s].to_vec());
            chunk.parity_count += 1;
        } else {
            return;
        }
        chunk.send_us = chunk.send_us.max(h.send_us);

        if chunk.have_count == k {
            chunk.complete = true;
        } else if chunk.have_count + chunk.parity_count >= k {
            let parity: Vec<(usize, &[u8])> = chunk
                .parity
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.as_deref().map(|p| (i, p)))
                .collect();
            if self.fec.recover(&mut chunk.data, &chunk.have, &parity, m) {
                chunk.complete = true;
                self.stats.fec_recovered += 1;
            }
        }
        if chunk.complete {
            chunk.parity = Vec::new();
            if chunk.last {
                frame.last_chunk_complete = true;
            }
        }

        if self
            .newest_seen
            .is_none_or(|n| after((h.frame_id, h.chunk_id), n))
        {
            self.newest_seen = Some((h.frame_id, h.chunk_id));
        }
        // A later frame is fully received while the current one is not: the
        // current frame is unrecoverable (unless a resend is on its way).
        if h.frame_id != cur_frame
            && self.frames[&h.frame_id].last_chunk_complete
            && self
                .pending_drop
                .is_none_or(|p| frame_before(h.frame_id, p))
        {
            self.pending_drop = Some(h.frame_id);
        }
        self.release(out);
        let now = clock::now_us();
        self.maybe_nack(now, out);
        self.maybe_drop(now, out);
        self.release(out);
    }

    /// Drop every frame before `frame_id` and resume from its first chunk.
    fn drop_until(&mut self, frame_id: u32, out: &mut Vec<Event>) {
        let (first, _) = self.current.unwrap();
        let last = frame_id.wrapping_sub(1);
        self.frames.retain(|&f, _| !frame_before(f, frame_id));
        self.stats.frames_lost += frame_id.wrapping_sub(first) as u64;
        out.push(Event::FrameLost { first, last });
        self.current = Some((frame_id, 0));
        self.pending_drop = None;
        self.nacked = None;
    }

    fn release(&mut self, out: &mut Vec<Event>) {
        while let Some((frame_id, next_chunk)) = self.current {
            let Some(frame) = self.frames.get_mut(&frame_id) else {
                return;
            };
            if !frame.chunks.get(&next_chunk).is_some_and(|c| c.complete) {
                return;
            }
            let c = frame.chunks.remove(&next_chunk).unwrap();
            let last = c.last;
            out.push(Event::Chunk(ReadyChunk {
                frame_id,
                chunk_id: next_chunk,
                data: c.data,
                last,
                keyframe: frame.keyframe,
                recovery: frame.recovery,
                lost_acked: frame.lost_acked,
                capture_us: frame.capture_us,
                send_us: c.send_us,
            }));
            if last {
                self.frames.remove(&frame_id);
                self.current = Some((frame_id.wrapping_add(1), 0));
            } else {
                self.current = Some((frame_id, next_chunk + 1));
            }
        }
    }
}

/// Is (frame, chunk) `a` strictly later in the stream than `b`?
fn after(a: (u32, u16), b: (u32, u16)) -> bool {
    frame_before(b.0, a.0) || (a.0 == b.0 && a.1 > b.1)
}

/// Wrapping comparison: is `a` strictly older than `b`?
pub fn frame_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagrams_fec(
        frame_id: u32,
        chunk_id: u16,
        last: bool,
        data: &[u8],
        fec_ratio: f32,
    ) -> Vec<Vec<u8>> {
        let mut s = VideoSender::new();
        let meta = ChunkMeta {
            channel: Channel::Video,
            frame_id,
            chunk_id,
            last,
            keyframe: frame_id == 0,
            recovery: false,
            lost_acked: 0,
            capture_us: 1,
            fec_ratio,
        };
        let (buf, segment) = s.packetize(&meta, data);
        assert_eq!(buf.len() % segment, 0, "equal-size datagrams for USO");
        buf.chunks(segment).map(|d| d.to_vec()).collect()
    }

    fn datagrams(frame_id: u32, chunk_id: u16, last: bool, data: &[u8]) -> Vec<Vec<u8>> {
        datagrams_fec(frame_id, chunk_id, last, data, 0.0)
    }

    fn feed(r: &mut Reassembler, dgs: &[Vec<u8>]) -> Vec<Event> {
        let mut out = Vec::new();
        for d in dgs {
            let (h, p) = Header::read(d).unwrap();
            r.push(&h, p, &mut out);
        }
        out
    }

    fn payload(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn in_order_chunks_round_trip() {
        let a = payload(5000, 1);
        let b = payload(100, 2);
        let mut r = Reassembler::new();
        let mut dgs = datagrams(0, 0, false, &a);
        dgs.extend(datagrams(0, 1, true, &b));
        let ev = feed(&mut r, &dgs);
        assert_eq!(ev.len(), 2);
        match (&ev[0], &ev[1]) {
            (Event::Chunk(c0), Event::Chunk(c1)) => {
                assert_eq!(c0.data, a);
                assert!(!c0.last);
                assert_eq!(c1.data, b);
                assert!(c1.last && c1.keyframe);
            }
            _ => panic!("{ev:?}"),
        }
    }

    #[test]
    fn empty_chunk_still_carries_last_flag() {
        let ev = feed(&mut Reassembler::new(), &datagrams(0, 0, true, &[]));
        assert!(matches!(&ev[..], [Event::Chunk(c)] if c.last && c.data.is_empty()));
    }

    #[test]
    fn reordered_and_duplicated_fragments() {
        let a = payload(10_000, 3);
        let mut dgs = datagrams(0, 0, true, &a);
        dgs.reverse();
        let dup = dgs[2].clone();
        dgs.insert(0, dup);
        let ev = feed(&mut Reassembler::new(), &dgs);
        assert_eq!(ev.len(), 1);
        let Event::Chunk(c) = &ev[0] else { panic!() };
        assert_eq!(c.data, a);
    }

    #[test]
    fn chunks_wait_for_predecessors() {
        let mut r = Reassembler::new();
        let c1 = datagrams(0, 1, true, &payload(50, 1));
        let c0 = datagrams(0, 0, false, &payload(50, 2));
        assert!(
            feed(&mut r, &c1).is_empty(),
            "chunk 1 must wait for chunk 0"
        );
        let ev = feed(&mut r, &c0);
        assert_eq!(ev.len(), 2);
    }

    #[test]
    fn lost_fragment_drops_frame_and_resumes() {
        let mut r = Reassembler::new();
        let mut f0 = datagrams(0, 0, true, &payload(3000, 1));
        f0.remove(1); // lose a fragment, no FEC
        assert!(feed(&mut r, &f0).is_empty());
        let f1 = datagrams(1, 0, true, &payload(200, 2));
        let ev = feed(&mut r, &f1);
        assert!(
            matches!(ev[0], Event::FrameLost { first: 0, last: 0 }),
            "{ev:?}"
        );
        assert!(matches!(&ev[1], Event::Chunk(c) if c.frame_id == 1 && c.last));
        assert_eq!(r.stats.frames_lost, 1);
        // Late fragment for the dropped frame is ignored.
        let late = datagrams(0, 0, true, &payload(3000, 1));
        assert!(feed(&mut r, &late[1..2]).is_empty());
    }

    #[test]
    fn fec_rebuilds_chunk_with_lost_data_shards() {
        let a = payload(20_000, 5); // 18 data shards
        let mut dgs = datagrams_fec(0, 0, true, &a, 0.25); // + 5 parity
        assert_eq!(dgs.len(), 18 + 5);
        for i in [16, 9, 3, 0] {
            dgs.remove(i); // 4 data shards lost
        }
        let mut r = Reassembler::new();
        let ev = feed(&mut r, &dgs);
        assert!(
            matches!(&ev[..], [Event::Chunk(c)] if c.data == a),
            "{ev:?}"
        );
        assert_eq!(r.stats.fec_recovered, 1);
    }

    #[test]
    fn fec_completes_before_all_parity_arrives() {
        let a = payload(5_000, 7);
        let dgs = datagrams_fec(0, 0, true, &a, 0.5);
        let mut r = Reassembler::new();
        // Deliver only the data shards: complete without waiting for parity;
        // the parity that arrives afterwards is ignored.
        let k = net_data_shards(&dgs);
        let ev = feed(&mut r, &dgs[..k]);
        assert!(matches!(&ev[..], [Event::Chunk(c)] if c.data == a));
        assert!(feed(&mut r, &dgs[k..]).is_empty());
        assert_eq!(r.stats.fec_recovered, 0);
    }

    fn net_data_shards(dgs: &[Vec<u8>]) -> usize {
        Header::read(&dgs[0]).unwrap().0.frag_count as usize
    }

    #[test]
    fn fec_tiny_chunk_parity_is_small() {
        let a = payload(300, 9);
        // Clean link: one parity shard, the size of the chunk itself.
        let dgs = datagrams_fec(0, 0, true, &a, 0.1);
        assert_eq!(dgs.len(), 2, "1 data + 1 parity");
        assert!(dgs.iter().all(|d| d.len() == HEADER_LEN + 300));
        let ev = feed(&mut Reassembler::new(), &dgs[1..]);
        assert!(matches!(&ev[..], [Event::Chunk(c)] if c.data == a));
        // Heavy loss: 3 parity shards survive a burst of two.
        let dgs = datagrams_fec(0, 0, true, &a, 0.3);
        assert_eq!(dgs.len(), 4, "1 data + 3 parity");
        let ev = feed(&mut Reassembler::new(), &dgs[2..]);
        assert!(matches!(&ev[..], [Event::Chunk(c)] if c.data == a));
    }

    #[test]
    fn nack_requests_missing_shard_and_resend_completes_in_order() {
        let mut r = Reassembler::new();
        r.set_nack_wait(Some(std::time::Duration::from_secs(5)));
        let f0 = datagrams(0, 0, true, &payload(5000, 1)); // 5 shards, no FEC
        let lost = f0[2].clone();
        let mut ev = feed(
            &mut r,
            &[f0[0].clone(), f0[1].clone(), f0[3].clone(), f0[4].clone()],
        );
        let f1 = datagrams(1, 0, true, &payload(200, 2));
        ev.extend(feed(&mut r, &f1));
        let nacks: Vec<_> = ev
            .iter()
            .filter(|e| matches!(e, Event::Nack { .. }))
            .collect();
        assert_eq!(nacks.len(), 1, "{ev:?}");
        assert!(
            matches!(
                nacks[0],
                Event::Nack {
                    frame_id: 0,
                    chunk_id: 0,
                    base: 2,
                    mask: 1
                }
            ),
            "{ev:?}"
        );
        assert!(
            !ev.iter().any(|e| matches!(e, Event::FrameLost { .. })),
            "waits for the resend"
        );
        // The resend completes frame 0; both frames come out, in order.
        let ev = feed(&mut r, &[lost]);
        let ids: Vec<u32> = ev
            .iter()
            .filter_map(|e| {
                if let Event::Chunk(c) = e {
                    Some(c.frame_id)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ids, [0, 1]);
        assert_eq!(r.stats.nacks, 1);
    }

    #[test]
    fn nack_gives_up_after_wait() {
        let mut r = Reassembler::new();
        r.set_nack_wait(Some(std::time::Duration::from_millis(5)));
        let mut f0 = datagrams(0, 0, true, &payload(3000, 1));
        f0.remove(1);
        feed(&mut r, &f0);
        let ev = feed(&mut r, &datagrams(1, 0, true, &payload(200, 2)));
        assert!(ev.iter().any(|e| matches!(e, Event::Nack { .. })));
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut out = Vec::new();
        r.poll(&mut out);
        assert!(
            matches!(out[0], Event::FrameLost { first: 0, last: 0 }),
            "{out:?}"
        );
        assert!(matches!(&out[1], Event::Chunk(c) if c.frame_id == 1));
    }

    #[test]
    fn frame_ids_wrap() {
        assert!(frame_before(u32::MAX, 0));
        assert!(!frame_before(0, u32::MAX));
        let mut r = Reassembler::new();
        let mut dgs = datagrams(u32::MAX, 0, true, &payload(10, 1));
        dgs.extend(datagrams(0, 0, true, &payload(10, 2)));
        let ev = feed(&mut r, &dgs);
        assert_eq!(ev.len(), 2);
    }
}
