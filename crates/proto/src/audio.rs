//! Audio datagram payload (channel `Audio`).
//!
//! One datagram per 5 ms Opus frame. It also repeats a few earlier frames,
//! spread out in time (by default 1, 2 and 4 frames back), so a loss burst
//! that takes several consecutive datagrams still leaves a copy of each
//! frame in a later one:
//!
//! ```text
//! u32 seq          frame number of the newest packet
//! u8  count        packets that follow, newest first
//! count x { u8 back, u16 len, len bytes of Opus }   (frame = seq - back)
//! ```
//!
//! The header's `capture_us` is the host capture time of frame `seq`'s first
//! sample.

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 5 ms at 48 kHz, per channel.
pub const FRAME: usize = 240;
pub const FRAME_US: u64 = 5_000;
/// Largest Opus packet we produce or accept.
pub const MAX_PACKET: usize = 400;
/// Packets per datagram (current + redundant copies).
pub const MAX_COUNT: usize = 4;
/// Which earlier frames each datagram repeats (frames back).
pub const REDUNDANT_BACK: [u8; 3] = [1, 2, 4];

/// Write `[seq][count]` + `(back, packet)` pairs (newest first, `back` 0 for
/// the frame itself) into `out`; returns the length.
pub fn write(seq: u32, packets: &[(u8, &[u8])], out: &mut [u8]) -> Option<usize> {
    if packets.is_empty() || packets.len() > MAX_COUNT {
        return None;
    }
    out.get_mut(..5)?[..4].copy_from_slice(&seq.to_le_bytes());
    out[4] = packets.len() as u8;
    let mut at = 5;
    for (back, p) in packets {
        if p.len() > MAX_PACKET {
            return None;
        }
        *out.get_mut(at)? = *back;
        out.get_mut(at + 1..at + 3)?
            .copy_from_slice(&(p.len() as u16).to_le_bytes());
        out.get_mut(at + 3..at + 3 + p.len())?.copy_from_slice(p);
        at += 3 + p.len();
    }
    Some(at)
}

/// Iterate `(seq, packet)` pairs, newest first.
pub fn read(payload: &[u8]) -> Option<impl Iterator<Item = (u32, &[u8])>> {
    let seq = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
    let count = *payload.get(4)? as usize;
    if count == 0 || count > MAX_COUNT {
        return None;
    }
    let mut at = 5;
    let mut items = [(0u32, &payload[0..0]); MAX_COUNT];
    for item in items.iter_mut().take(count) {
        let back = *payload.get(at)? as u32;
        let len = u16::from_le_bytes(payload.get(at + 1..at + 3)?.try_into().ok()?) as usize;
        if len > MAX_PACKET {
            return None;
        }
        *item = (seq.wrapping_sub(back), payload.get(at + 3..at + 3 + len)?);
        at += 3 + len;
    }
    Some(items.into_iter().take(count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_spread_redundancy() {
        let mut buf = [0u8; 1200];
        let n = write(7, &[(0, b"now"), (1, b"one"), (4, b"four")], &mut buf).unwrap();
        let got: Vec<_> = read(&buf[..n]).unwrap().collect();
        assert_eq!(
            got,
            vec![(7, &b"now"[..]), (6, &b"one"[..]), (3, &b"four"[..])]
        );
        assert!(read(&buf[..n - 1]).is_none(), "truncated");
        assert!(write(1, &[], &mut buf).is_none());
    }
}
