//! Streams the host's audio: loopback capture → Opus (5 ms frames) → one
//! datagram per frame that also repeats frames 1, 2 and 4 back, so a loss
//! burst of up to 3 datagrams (or scattered single losses) costs nothing.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use audio::capture::{self, CaptureConfig};
use audio::Encoder;
use net::Socket;
use proto::audio::{CHANNELS, MAX_COUNT, MAX_PACKET, RATE, REDUNDANT_BACK};
use proto::{clock, Channel, Header, HEADER_LEN, MAX_DATAGRAM};

use crate::session::Shared;

/// Opus bitrate: transparent for stereo music at 5 ms frames.
const BITRATE: i32 = 128_000;
/// Encoded frames kept for redundancy (power of two > max back).
const HISTORY: usize = 8;

pub fn spawn(
    socket: Socket,
    shared: Arc<Shared>,
    cfg: CaptureConfig,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    let mut enc = Encoder::new(RATE, CHANNELS, BITRATE)?;
    tracing::info!(
        "audio: Opus {} kbps, 5 ms frames, look-ahead {:.1} ms, copies {:?} frames back",
        BITRATE / 1000,
        enc.lookahead() as f64 * 1e3 / RATE as f64,
        REDUNDANT_BACK
    );
    let mut seq = 0u32;
    // Ring of recent packets by seq; len 0 = not sent (no peer then).
    let mut hist = [[0u8; MAX_PACKET]; HISTORY];
    let mut hist_len = [0usize; HISTORY];
    let mut dgram = [0u8; MAX_DATAGRAM];
    capture::start(cfg, stop, move |pcm, capture_us| {
        let this = seq;
        seq = seq.wrapping_add(1);
        let slot = this as usize % HISTORY;
        let n = match enc.encode(pcm, &mut hist[slot]) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("audio encode: {e}");
                hist_len[slot] = 0;
                return;
            }
        };
        let Some(peer) = shared.peer() else {
            hist_len = [0; HISTORY];
            return;
        };
        hist_len[slot] = n;
        let mut packets: [(u8, &[u8]); MAX_COUNT] = [(0, &[]); MAX_COUNT];
        packets[0] = (0, &hist[slot][..n]);
        let mut count = 1;
        for back in REDUNDANT_BACK {
            let s = this.wrapping_sub(back as u32) as usize % HISTORY;
            if hist_len[s] > 0 {
                packets[count] = (back, &hist[s][..hist_len[s]]);
                count += 1;
            }
        }
        if let Some(len) = proto::audio::write(this, &packets[..count], &mut dgram[HEADER_LEN..]) {
            Header {
                channel: Channel::Audio as u8,
                sequence: this,
                capture_us,
                send_us: clock::now_us(),
                ..Default::default()
            }
            .write(&mut dgram[..HEADER_LEN]);
            let _ = socket.send_to(&dgram[..HEADER_LEN + len], peer);
        }
    })
}
