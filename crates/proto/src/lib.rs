//! Wire format shared by host and client.
//!
//! Every datagram starts with a fixed 44-byte little-endian [`Header`].
//! Video frames are split into chunks (one or more NVENC slices, delivered as
//! soon as the encoder writes them), and each chunk into fragments that fit in
//! a single datagram.

pub mod audio;
pub mod clock;
pub mod input;
pub mod rendezvous;

/// Total datagram budget. 1200 bytes survives every common internet path MTU
/// (IPv6 minimum is 1280) without IP fragmentation.
pub const MAX_DATAGRAM: usize = 1200;
pub const HEADER_LEN: usize = 44;
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN;
pub const VERSION: u8 = 2;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Video = 0,
    Audio = 1,
    Input = 2,
    Control = 3,
    /// Cursor state (single datagram).
    Cursor = 4,
    /// Cursor shape, fragmented like video chunks.
    CursorShape = 5,
}

impl Channel {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Channel::Video,
            1 => Channel::Audio,
            2 => Channel::Input,
            3 => Channel::Control,
            4 => Channel::Cursor,
            5 => Channel::CursorShape,
            _ => return None,
        })
    }
}

pub mod flags {
    /// This chunk ends the picture.
    pub const LAST_CHUNK: u16 = 1 << 0;
    /// The picture is an IDR.
    pub const KEYFRAME: u16 = 1 << 1;
    /// The picture references no frame reported lost up to `Header::lost_acked`
    /// (encoded after NVENC reference invalidation): decoding is clean again.
    pub const RECOVERY: u16 = 1 << 2;
    /// A retransmission (NACK reply): excluded from loss and delay statistics.
    pub const RETRANSMIT: u16 = 1 << 3;
}

/// Fixed datagram header.
///
/// ```text
///  0 u8  version        1 u8  channel       2 u16 flags
///  4 u32 sequence       (per channel, monotonically increasing)
///  8 u32 frame_id
/// 12 u16 chunk_id      14 u16 frag_index   16 u16 frag_count  18 u16 parity_count
/// 20 u32 chunk_len     (bytes of the whole chunk)
/// 24 u64 capture_us    (host clock when the frame was captured)
/// 32 u64 send_us       (host clock when this datagram was sent)
/// 40 u32 lost_acked    (newest lost frame the host has excluded from references)
/// ```
///
/// Fragments `0..frag_count` carry data, `frag_count..frag_count+parity_count`
/// carry Reed-Solomon parity (see `net::fec`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub channel: u8,
    pub flags: u16,
    pub sequence: u32,
    pub frame_id: u32,
    pub chunk_id: u16,
    pub frag_index: u16,
    pub frag_count: u16,
    pub parity_count: u16,
    pub chunk_len: u32,
    pub capture_us: u64,
    pub send_us: u64,
    pub lost_acked: u32,
}

impl Header {
    pub fn write(&self, out: &mut [u8]) {
        out[0] = VERSION;
        out[1] = self.channel;
        out[2..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..12].copy_from_slice(&self.frame_id.to_le_bytes());
        out[12..14].copy_from_slice(&self.chunk_id.to_le_bytes());
        out[14..16].copy_from_slice(&self.frag_index.to_le_bytes());
        out[16..18].copy_from_slice(&self.frag_count.to_le_bytes());
        out[18..20].copy_from_slice(&self.parity_count.to_le_bytes());
        out[20..24].copy_from_slice(&self.chunk_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.capture_us.to_le_bytes());
        out[32..40].copy_from_slice(&self.send_us.to_le_bytes());
        out[40..44].copy_from_slice(&self.lost_acked.to_le_bytes());
    }

    pub fn read(buf: &[u8]) -> Option<(Header, &[u8])> {
        if buf.len() < HEADER_LEN || buf[0] != VERSION || Channel::from_u8(buf[1]).is_none() {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
        let h = Header {
            channel: buf[1],
            flags: u16_at(2),
            sequence: u32_at(4),
            frame_id: u32_at(8),
            chunk_id: u16_at(12),
            frag_index: u16_at(14),
            frag_count: u16_at(16),
            parity_count: u16_at(18),
            chunk_len: u32_at(20),
            capture_us: u64_at(24),
            send_us: u64_at(32),
            lost_acked: u32_at(40),
        };
        Some((h, &buf[HEADER_LEN..]))
    }

    pub fn channel(&self) -> Option<Channel> {
        Channel::from_u8(self.channel)
    }
}

/// Control messages, carried as the payload of a `Channel::Control` datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Client → host: start streaming to the sender's address.
    Hello,
    /// Client → host: decoding broke; send an IDR.
    RequestKeyframe,
    /// Client → host: stop streaming.
    Bye,
    /// Client → host: the cursor shape with this id never arrived.
    RequestCursorShape,
    /// Client → host: still here (the host drops silent clients).
    KeepAlive,
    /// Host → client: gamepad `index` rumble motor speeds.
    Rumble { index: u8, large: u8, small: u8 },
    /// Client → host, every ~10 ms: what arrived and how late (congestion control, FEC).
    Feedback(Feedback),
    /// Host → client: echo this back (RTT measurement). Also carries the host's
    /// current RTT estimate so the client can size its NACK wait.
    Ping { t_us: u64, rtt_us: u32 },
    /// Client → host: echo of a `Ping`.
    Pong { t_us: u64 },
    /// Client → host: frames from `frame_id` on could not be decoded cleanly (the
    /// first of a run that FEC could not repair); stop referencing them.
    FrameLost { frame_id: u32 },
    /// Client → host: resend data shards `base + i` (bit i of `mask`) of this chunk.
    Nack {
        frame_id: u32,
        chunk_id: u16,
        base: u16,
        mask: u128,
    },
}

/// Receiver report for one feedback interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    pub interval_us: u32,
    /// Video datagrams expected (sequence span) and actually received, excluding retransmits.
    pub expected: u32,
    pub received: u32,
    pub recv_bytes: u32,
    /// Queuing delay: one-way delay above its recent minimum. Max and mean over the interval.
    pub qdelay_max_us: u32,
    pub qdelay_avg_us: u32,
    /// Newest frame fully reassembled.
    pub last_frame: u32,
    /// Smallest queuing delay in the interval. Frames leave in bursts, so the
    /// average includes self-inflicted serialization delay; the minimum (each
    /// burst's first datagram) only rises with a standing queue.
    pub qdelay_min_us: u32,
}

/// Largest encoded `Control` payload.
pub const CONTROL_MAX: usize = 40;

impl Control {
    /// Payload bytes: a type byte plus arguments.
    pub fn encode(self) -> ([u8; CONTROL_MAX], usize) {
        let mut d = [0u8; CONTROL_MAX];
        let n = match self {
            Control::Hello => put(&mut d, 1, &[]),
            Control::RequestKeyframe => put(&mut d, 2, &[]),
            Control::Bye => put(&mut d, 3, &[]),
            Control::RequestCursorShape => put(&mut d, 4, &[]),
            Control::Rumble {
                index,
                large,
                small,
            } => put(&mut d, 5, &[index, large, small]),
            Control::KeepAlive => put(&mut d, 6, &[]),
            Control::Feedback(f) => {
                let mut b = [0u8; 32];
                for (i, v) in [
                    f.interval_us,
                    f.expected,
                    f.received,
                    f.recv_bytes,
                    f.qdelay_max_us,
                    f.qdelay_avg_us,
                    f.last_frame,
                    f.qdelay_min_us,
                ]
                .into_iter()
                .enumerate()
                {
                    b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
                put(&mut d, 7, &b)
            }
            Control::Ping { t_us, rtt_us } => {
                let mut b = [0u8; 12];
                b[..8].copy_from_slice(&t_us.to_le_bytes());
                b[8..].copy_from_slice(&rtt_us.to_le_bytes());
                put(&mut d, 8, &b)
            }
            Control::Pong { t_us } => put(&mut d, 9, &t_us.to_le_bytes()),
            Control::FrameLost { frame_id } => put(&mut d, 10, &frame_id.to_le_bytes()),
            Control::Nack {
                frame_id,
                chunk_id,
                base,
                mask,
            } => {
                let mut b = [0u8; 24];
                b[..4].copy_from_slice(&frame_id.to_le_bytes());
                b[4..6].copy_from_slice(&chunk_id.to_le_bytes());
                b[6..8].copy_from_slice(&base.to_le_bytes());
                b[8..].copy_from_slice(&mask.to_le_bytes());
                put(&mut d, 11, &b)
            }
        };
        (d, n)
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let u32_at = |i: usize| {
            b.get(i..i + 4)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        };
        let u64_at = |i: usize| {
            b.get(i..i + 8)
                .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        };
        Some(match *b.first()? {
            1 => Control::Hello,
            2 => Control::RequestKeyframe,
            3 => Control::Bye,
            4 => Control::RequestCursorShape,
            5 => Control::Rumble {
                index: *b.get(1)?,
                large: *b.get(2)?,
                small: *b.get(3)?,
            },
            6 => Control::KeepAlive,
            7 => Control::Feedback(Feedback {
                interval_us: u32_at(1)?,
                expected: u32_at(5)?,
                received: u32_at(9)?,
                recv_bytes: u32_at(13)?,
                qdelay_max_us: u32_at(17)?,
                qdelay_avg_us: u32_at(21)?,
                last_frame: u32_at(25)?,
                qdelay_min_us: u32_at(29)?,
            }),
            8 => Control::Ping {
                t_us: u64_at(1)?,
                rtt_us: u32_at(9)?,
            },
            9 => Control::Pong { t_us: u64_at(1)? },
            10 => Control::FrameLost {
                frame_id: u32_at(1)?,
            },
            11 => Control::Nack {
                frame_id: u32_at(1)?,
                chunk_id: u16::from_le_bytes(b.get(5..7)?.try_into().ok()?),
                base: u16::from_le_bytes(b.get(7..9)?.try_into().ok()?),
                mask: u128::from_le_bytes(b.get(9..25)?.try_into().ok()?),
            },
            _ => return None,
        })
    }

    /// Encode as a full datagram.
    pub fn datagram(self, sequence: u32) -> Vec<u8> {
        let (payload, n) = self.encode();
        datagram(Channel::Control, sequence, &payload[..n])
    }
}

fn put(d: &mut [u8; CONTROL_MAX], kind: u8, args: &[u8]) -> usize {
    d[0] = kind;
    d[1..1 + args.len()].copy_from_slice(args);
    1 + args.len()
}

/// Build a single-datagram message with a minimal header.
pub fn datagram(channel: Channel, sequence: u32, payload: &[u8]) -> Vec<u8> {
    let mut d = vec![0u8; HEADER_LEN + payload.len()];
    write_datagram(channel, sequence, payload, &mut d);
    d
}

/// Like [`datagram`], but into a caller buffer (no allocation on hot paths).
/// Returns the datagram length. Panics if `out` is too small.
pub fn write_datagram(channel: Channel, sequence: u32, payload: &[u8], out: &mut [u8]) -> usize {
    let n = HEADER_LEN + payload.len();
    Header {
        channel: channel as u8,
        sequence,
        send_us: clock::now_us(),
        ..Default::default()
    }
    .write(&mut out[..HEADER_LEN]);
    out[HEADER_LEN..n].copy_from_slice(payload);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h = Header {
            channel: Channel::Video as u8,
            flags: flags::LAST_CHUNK | flags::KEYFRAME,
            sequence: 0xdead_beef,
            frame_id: 42,
            chunk_id: 3,
            frag_index: 7,
            frag_count: 9,
            parity_count: 2,
            chunk_len: 123_456,
            capture_us: 1_000_000_007,
            send_us: 1_000_000_123,
            lost_acked: 77,
        };
        let mut buf = [0u8; HEADER_LEN + 3];
        h.write(&mut buf);
        buf[HEADER_LEN..].copy_from_slice(b"abc");
        let (r, payload) = Header::read(&buf).unwrap();
        assert_eq!(r, h);
        assert_eq!(payload, b"abc");
    }

    #[test]
    fn control_round_trip() {
        for c in [
            Control::Hello,
            Control::RequestKeyframe,
            Control::Bye,
            Control::RequestCursorShape,
            Control::KeepAlive,
            Control::Rumble {
                index: 2,
                large: 200,
                small: 9,
            },
            Control::Feedback(Feedback {
                interval_us: 10_000,
                expected: 120,
                received: 114,
                recv_bytes: 130_000,
                qdelay_max_us: 4_500,
                qdelay_avg_us: 1_200,
                last_frame: 9_999,
                qdelay_min_us: 300,
            }),
            Control::Ping {
                t_us: u64::MAX - 5,
                rtt_us: 1234,
            },
            Control::Nack {
                frame_id: 7,
                chunk_id: 3,
                base: 128,
                mask: (1u128 << 127) | 5,
            },
            Control::Pong { t_us: 1 },
            Control::FrameLost { frame_id: 0xabcdef },
        ] {
            let d = c.datagram(5);
            assert!(c.encode().1 <= CONTROL_MAX);
            let (h, p) = Header::read(&d).unwrap();
            assert_eq!(h.channel(), Some(Channel::Control));
            assert_eq!(Control::decode(p), Some(c));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(Header::read(&[0u8; 10]).is_none());
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = VERSION + 1;
        assert!(Header::read(&buf).is_none());
    }
}
