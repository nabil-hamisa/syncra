//! Minimal libopus bindings (hand-written: the API surface used is tiny)
//! and safe encoder/decoder wrappers.

use std::ffi::c_int;
use std::ptr::NonNull;

#[repr(C)]
pub struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct OpusDecoder {
    _private: [u8; 0],
}

pub const OPUS_OK: c_int = 0;
/// CELT only: lowest algorithmic delay (2.5 ms look-ahead), no speech mode.
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;

extern "C" {
    pub fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    pub fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    pub fn opus_encoder_destroy(st: *mut OpusEncoder);

    pub fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    pub fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_decoder_destroy(st: *mut OpusDecoder);
}

#[derive(Debug)]
pub struct Error(pub c_int);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "opus error {}", self.0)
    }
}

impl std::error::Error for Error {}

fn check(r: c_int) -> Result<c_int, Error> {
    if r < 0 {
        Err(Error(r))
    } else {
        Ok(r)
    }
}

/// Low-delay CELT encoder for interleaved f32 PCM.
pub struct Encoder {
    st: NonNull<OpusEncoder>,
    channels: usize,
}

// SAFETY: an OpusEncoder has no thread affinity; `&mut self` serializes use.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(rate: u32, channels: usize, bitrate: i32) -> Result<Self, Error> {
        let mut err = 0;
        // SAFETY: plain constructor; `err` is a valid out-pointer.
        let st = unsafe {
            opus_encoder_create(
                rate as i32,
                channels as c_int,
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        check(err)?;
        let st = NonNull::new(st).ok_or(Error(-7))?;
        let e = Encoder { st, channels };
        // SAFETY: valid encoder; these requests take one opus_int32 argument.
        unsafe {
            check(opus_encoder_ctl(
                e.st.as_ptr(),
                OPUS_SET_BITRATE_REQUEST,
                bitrate,
            ))?;
            check(opus_encoder_ctl(e.st.as_ptr(), OPUS_SET_VBR_REQUEST, 0i32))?;
            check(opus_encoder_ctl(
                e.st.as_ptr(),
                OPUS_SET_COMPLEXITY_REQUEST,
                5i32,
            ))?;
        }
        Ok(e)
    }

    /// Algorithmic look-ahead in samples.
    pub fn lookahead(&self) -> i32 {
        let mut v = 0i32;
        // SAFETY: valid encoder; the request takes an opus_int32 out-pointer.
        unsafe {
            opus_encoder_ctl(
                self.st.as_ptr(),
                OPUS_GET_LOOKAHEAD_REQUEST,
                &mut v as *mut i32,
            )
        };
        v
    }

    /// Encode one frame (`pcm.len() / channels` samples per channel, a valid
    /// Opus frame size) into `out`; returns the packet length.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, Error> {
        let frame = (pcm.len() / self.channels) as c_int;
        // SAFETY: pcm holds frame * channels samples; out is writable for its length.
        let n = unsafe {
            opus_encode_float(
                self.st.as_ptr(),
                pcm.as_ptr(),
                frame,
                out.as_mut_ptr(),
                out.len().min(i32::MAX as usize) as i32,
            )
        };
        Ok(check(n)? as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, destroyed once.
        unsafe { opus_encoder_destroy(self.st.as_ptr()) }
    }
}

pub struct Decoder {
    st: NonNull<OpusDecoder>,
    channels: usize,
}

// SAFETY: as for Encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(rate: u32, channels: usize) -> Result<Self, Error> {
        let mut err = 0;
        // SAFETY: plain constructor; `err` is a valid out-pointer.
        let st = unsafe { opus_decoder_create(rate as i32, channels as c_int, &mut err) };
        check(err)?;
        Ok(Decoder {
            st: NonNull::new(st).ok_or(Error(-7))?,
            channels,
        })
    }

    /// Decode a packet into `out` (room for one frame); `None` conceals a
    /// lost frame. Returns samples per channel.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize, Error> {
        let (ptr, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        let frame = (out.len() / self.channels) as c_int;
        // SAFETY: packet (or null for PLC) is readable for len; out holds
        // frame * channels samples.
        let n =
            unsafe { opus_decode_float(self.st.as_ptr(), ptr, len, out.as_mut_ptr(), frame, 0) };
        Ok(check(n)? as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create, destroyed once.
        unsafe { opus_decoder_destroy(self.st.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_5ms_stereo() {
        let mut e = Encoder::new(48_000, 2, 128_000).unwrap();
        let mut d = Decoder::new(48_000, 2).unwrap();
        let mut pkt = [0u8; 1500];
        let mut out = [0f32; 480];
        let mut energy = 0.0f32;
        for f in 0..50 {
            let pcm: Vec<f32> = (0..480)
                .map(|i| {
                    (((f * 240 + i / 2) as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0)
                        .sin()
                        * 0.5
                })
                .collect();
            let n = e.encode(&pcm, &mut pkt).unwrap();
            assert!(n > 10 && n < 200, "{n} bytes");
            assert_eq!(d.decode(Some(&pkt[..n]), &mut out).unwrap(), 240);
            if f > 10 {
                energy += out.iter().map(|s| s * s).sum::<f32>();
            }
        }
        assert!(energy > 10.0, "decoded tone has energy {energy}");
        // Loss concealment produces a full frame too.
        assert_eq!(d.decode(None, &mut out).unwrap(), 240);
        assert!(e.lookahead() > 0);
    }

    /// `cargo test -p opus-sys --release -- --ignored cost --nocapture`
    #[test]
    #[ignore]
    fn cost() {
        let mut e = Encoder::new(48_000, 2, 128_000).unwrap();
        let mut d = Decoder::new(48_000, 2).unwrap();
        let pcm: Vec<f32> = (0..480).map(|i| ((i as f32) * 0.07).sin() * 0.4).collect();
        let (mut pkt, mut out) = ([0u8; 1500], [0f32; 480]);
        let (mut enc, mut dec) = (Vec::new(), Vec::new());
        for _ in 0..5000 {
            let t = std::time::Instant::now();
            let n = e.encode(&pcm, &mut pkt).unwrap();
            let t2 = std::time::Instant::now();
            d.decode(Some(&pkt[..n]), &mut out).unwrap();
            dec.push(t2.elapsed().as_nanos() as u64);
            enc.push((t2 - t).as_nanos() as u64);
        }
        for (w, v) in [("encode", &mut enc), ("decode", &mut dec)] {
            v.sort_unstable();
            eprintln!(
                "{w} 5 ms stereo: p50 {} us p95 {} us",
                v[v.len() / 2] / 1000,
                v[v.len() * 95 / 100] / 1000
            );
        }
    }
}
