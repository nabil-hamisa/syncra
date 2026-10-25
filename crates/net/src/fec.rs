//! Reed-Solomon forward error correction per chunk: GF(2^8) for chunks of up
//! to 256 shards (all of them at 1080p), Leopard GF(2^16) beyond that.
//!
//! A chunk of `len` bytes is split into `k` data shards of `shard_bytes`
//! (the last one zero-padded) and `m` parity shards of the same size. Any `k`
//! of the `k + m` shards rebuild the chunk. The shard size adapts to the
//! chunk, so a 300-byte slice gets 300-byte parity, not a full datagram.

use std::collections::HashMap;

use proto::MAX_PAYLOAD;
use reed_solomon_erasure::galois_8::ReedSolomon as Gf8;
use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

/// Data shard count for a chunk of `len` bytes.
pub fn data_shards(len: usize) -> usize {
    len.div_ceil(MAX_PAYLOAD).max(1)
}

/// Shard size used for a chunk of `len` bytes split into `k` shards: the
/// smallest even size that fits (Reed-Solomon needs even, equal shards).
pub fn shard_bytes(len: usize, k: usize) -> usize {
    let s = len.div_ceil(k).max(2);
    (s + 1) & !1
}

/// Parity shards to add for `k` data shards at protection `ratio` (0 = none).
pub fn parity_shards(k: usize, ratio: f32) -> usize {
    if ratio <= 0.0 {
        return 0;
    }
    // Loss is bursty (consecutive datagrams of one chunk go together), so
    // under real loss a single parity shard rarely helps; small chunks (the
    // end-of-frame marker, whose loss kills the whole frame) get the most.
    // On a clean link one shard is enough: at low bitrates most chunks are
    // small, and a fixed floor of 3 would quadruple the traffic.
    let floor = if ratio >= 0.25 {
        if k <= 2 {
            3
        } else {
            2
        }
    } else if ratio >= 0.12 {
        2
    } else {
        1
    };
    ((k as f32 * ratio).ceil() as usize)
        .max(floor)
        .min(k.max(1) * 3)
        .min(u16::MAX as usize - k)
}

/// Chunks up to this many shards (data + parity) use GF(2^8) Reed-Solomon
/// (`reed-solomon-erasure`, SIMD): decoding costs O(k·m·s), a few µs for our
/// chunks. Larger ones fall back to Leopard GF(2^16) (`reed-solomon-simd`),
/// whose FFT decoder has a ~0.25 ms fixed cost but no 256-shard limit.
/// Both ends choose by `k + m`, so no codec id is needed on the wire.
const GF8_MAX_SHARDS: usize = 256;

fn use_gf8(k: usize, m: usize) -> bool {
    k + m <= GF8_MAX_SHARDS
}

/// GF(2^8) codecs are expensive to build (matrix setup); keep one per (k, m).
#[derive(Default)]
struct Gf8Cache(HashMap<(usize, usize), Gf8>);

impl Gf8Cache {
    fn get(&mut self, k: usize, m: usize) -> Option<&Gf8> {
        if !self.0.contains_key(&(k, m)) {
            let rs = Gf8::new(k, m).ok()?;
            if self.0.len() > 512 {
                self.0.clear();
            }
            self.0.insert((k, m), rs);
        }
        self.0.get(&(k, m))
    }
}

/// Copy data shard `i` of `data` (zero-padded to `s`) into `out`.
fn data_shard<'a>(data: &'a [u8], i: usize, s: usize, pad: &'a mut Vec<u8>) -> &'a [u8] {
    let start = (i * s).min(data.len());
    let end = (start + s).min(data.len());
    if end - start == s {
        return &data[start..end];
    }
    pad.clear();
    pad.extend_from_slice(&data[start..end]);
    pad.resize(s, 0);
    pad
}

/// Reusable encoder (keeps its working memory between chunks).
#[derive(Default)]
pub struct FecEncoder {
    leopard: Option<ReedSolomonEncoder>,
    gf8: Gf8Cache,
    parity: Vec<Vec<u8>>,
    padded: Vec<Vec<u8>>,
    pad: Vec<u8>,
}

impl FecEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute `m` parity shards for `data` and hand each to `out(index, bytes)`.
    pub fn encode(&mut self, data: &[u8], m: usize, mut out: impl FnMut(usize, &[u8])) {
        if m == 0 {
            return;
        }
        let k = data_shards(data.len());
        let s = shard_bytes(data.len(), k);
        if use_gf8(k, m) {
            // Padded copies of the data shards (only the last one differs from
            // a slice of `data`, but the codec wants equal-length inputs).
            // Buffers are reused across chunks: no allocation once warm.
            self.padded.resize_with(k, Vec::new);
            for (i, buf) in self.padded[..k].iter_mut().enumerate() {
                let start = (i * s).min(data.len());
                let end = (start + s).min(data.len());
                buf.clear();
                buf.extend_from_slice(&data[start..end]);
                buf.resize(s, 0);
            }
            self.parity.resize_with(m, Vec::new);
            for p in &mut self.parity[..m] {
                p.clear();
                p.resize(s, 0);
            }
            let rs = self.gf8.get(k, m).expect("supported shard counts");
            rs.encode_sep(&self.padded[..k], &mut self.parity[..m])
                .expect("equal shard sizes");
            for (i, p) in self.parity[..m].iter().enumerate() {
                out(i, p);
            }
            return;
        }
        let rs = match self.leopard.as_mut() {
            Some(rs) => {
                rs.reset(k, m, s).expect("supported shard counts");
                rs
            }
            None => self
                .leopard
                .insert(ReedSolomonEncoder::new(k, m, s).expect("supported shard counts")),
        };
        for i in 0..k {
            rs.add_original_shard(data_shard(data, i, s, &mut self.pad))
                .unwrap();
        }
        let result = rs.encode().expect("all original shards added");
        for (i, shard) in result.recovery_iter().enumerate() {
            out(i, shard);
        }
    }
}

/// Reusable decoder.
#[derive(Default)]
pub struct FecDecoder {
    leopard: Option<ReedSolomonDecoder>,
    gf8: Gf8Cache,
    pad: Vec<u8>,
}

impl FecDecoder {
    /// Rebuild the missing data shards of a chunk in place.
    ///
    /// `data` is the chunk buffer (`len` bytes) with the received data shards
    /// already copied in; `have[i]` says whether data shard `i` arrived.
    /// `parity` holds the received parity shards as (parity index, bytes).
    /// Returns false if there aren't enough shards.
    pub fn recover(
        &mut self,
        data: &mut [u8],
        have: &[bool],
        parity: &[(usize, &[u8])],
        m: usize,
    ) -> bool {
        let k = have.len();
        let got = have.iter().filter(|&&h| h).count();
        if got == k {
            return true;
        }
        if got + parity.len() < k {
            return false;
        }
        let s = shard_bytes(data.len(), k);
        if parity.iter().any(|(i, p)| p.len() != s || *i >= m) {
            return false;
        }
        if use_gf8(k, m) {
            let Some(rs) = self.gf8.get(k, m) else {
                return false;
            };
            let mut shards: Vec<Option<Vec<u8>>> = vec![None; k + m];
            for (i, h) in have.iter().enumerate() {
                if *h {
                    shards[i] = Some(data_shard(data, i, s, &mut self.pad).to_vec());
                }
            }
            for &(i, p) in parity {
                shards[k + i] = Some(p.to_vec());
            }
            if rs.reconstruct_data(&mut shards).is_err() {
                return false;
            }
            for (i, h) in have.iter().enumerate() {
                if !h {
                    let start = i * s;
                    let end = (start + s).min(data.len());
                    let shard = shards[i].as_ref().expect("reconstructed");
                    data[start..end].copy_from_slice(&shard[..end - start]);
                }
            }
            return true;
        }
        let rs = match self.leopard.as_mut() {
            Some(rs) => {
                if rs.reset(k, m, s).is_err() {
                    return false;
                }
                rs
            }
            None => match ReedSolomonDecoder::new(k, m, s) {
                Ok(rs) => self.leopard.insert(rs),
                Err(_) => return false,
            },
        };
        for (i, _) in have.iter().enumerate().filter(|(_, &h)| h) {
            rs.add_original_shard(i, data_shard(data, i, s, &mut self.pad))
                .unwrap();
        }
        for &(i, shard) in parity {
            if rs.add_recovery_shard(i, shard).is_err() {
                return false;
            }
        }
        let Ok(result) = rs.decode() else {
            return false;
        };
        for (i, shard) in result.restored_original_iter() {
            let start = i * s;
            let end = (start + s).min(data.len());
            data[start..end].copy_from_slice(&shard[..end - start]);
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn shard_sizing() {
        assert_eq!(data_shards(1), 1);
        assert_eq!(data_shards(MAX_PAYLOAD), 1);
        assert_eq!(data_shards(MAX_PAYLOAD + 1), 2);
        assert_eq!(shard_bytes(301, 1), 302);
        assert_eq!(shard_bytes(2 * MAX_PAYLOAD, 2), MAX_PAYLOAD);
        assert!(shard_bytes(10_000, data_shards(10_000)) <= MAX_PAYLOAD);
        assert_eq!(parity_shards(10, 0.0), 0);
        assert_eq!(parity_shards(10, 0.05), 1, "clean link: one shard");
        assert_eq!(parity_shards(1, 0.05), 1);
        assert_eq!(parity_shards(10, 0.15), 2, "a few % loss: burst floor");
        assert_eq!(parity_shards(10, 0.25), 3);
        assert_eq!(parity_shards(1, 0.3), 3, "heavy loss: tiny chunks get 3");
    }

    fn roundtrip(len: usize, ratio: f32, lose: &[usize]) -> bool {
        let data = chunk(len);
        let k = data_shards(len);
        let s = shard_bytes(len, k);
        let m = parity_shards(k, ratio);
        let mut parity = Vec::new();
        FecEncoder::new().encode(&data, m, |i, p| parity.push((i, p.to_vec())));
        assert_eq!(parity.len(), m);
        assert!(parity.iter().all(|(_, p)| p.len() == s));

        // Shard indices 0..k are data, k..k+m parity; drop the ones in `lose`.
        let mut buf = vec![0u8; len];
        let mut have = vec![false; k];
        for (i, h) in have.iter_mut().enumerate() {
            if !lose.contains(&i) {
                let (a, b) = (i * s, ((i + 1) * s).min(len));
                buf[a..b].copy_from_slice(&data[a..b]);
                *h = true;
            }
        }
        let kept: Vec<(usize, &[u8])> = parity
            .iter()
            .filter(|(i, _)| !lose.contains(&(k + i)))
            .map(|(i, p)| (*i, p.as_slice()))
            .collect();
        let ok = FecDecoder::default().recover(&mut buf, &have, &kept, m);
        ok && buf == data
    }

    #[test]
    fn recovers_lost_data_shards() {
        assert!(
            roundtrip(300, 0.5, &[0]),
            "single tiny shard rebuilt from parity"
        );
        assert!(roundtrip(10_000, 0.25, &[0, 5]), "two data shards");
        assert!(
            roundtrip(10_000, 0.25, &[3, 9, 10]),
            "data + parity mixed loss"
        );
        assert!(
            roundtrip(50_001, 0.2, &[0, 1, 2, 3, 4, 5, 6, 7, 8]),
            "odd length, 9 lost of 44+9"
        );
        // 250 data + 25 parity shards > 256: the Leopard fallback path.
        let k = 250;
        assert!(!use_gf8(k, parity_shards(k, 0.1)));
        assert!(
            roundtrip(k * MAX_PAYLOAD - 7, 0.1, &[0, 17, 249, k + 3]),
            "Leopard fallback"
        );
    }

    #[test]
    fn fails_cleanly_without_enough_shards() {
        assert!(
            !roundtrip(10_000, 0.1, &[0, 1, 2]),
            "2 parity cannot fix 3 losses"
        );
        assert!(roundtrip(10_000, 0.0, &[]), "no FEC, no loss");
    }
}

#[cfg(test)]
mod bench {
    use std::time::Instant;

    /// `cargo test -p net --release fec_decode_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fec_decode_cost() {
        use reed_solomon_erasure::galois_8::ReedSolomon;
        for (k, m, s) in [
            (1usize, 1usize, 300usize),
            (1, 3, 300),
            (7, 1, 1156),
            (9, 4, 1156),
            (27, 7, 1156),
            (78, 47, 1156),
        ] {
            let data: Vec<Vec<u8>> = (0..k).map(|i| vec![i as u8 ^ 0x5a; s]).collect();
            // GF(2^8) Cauchy/Vandermonde
            let rs = ReedSolomon::new(k, m).unwrap();
            let mut shards: Vec<Vec<u8>> = data
                .iter()
                .cloned()
                .chain((0..m).map(|_| vec![0u8; s]))
                .collect();
            rs.encode(&mut shards).unwrap();
            let n = 200;
            let t = Instant::now();
            for _ in 0..n {
                let mut opt: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
                opt[0] = None; // one data shard lost
                rs.reconstruct_data(&mut opt).unwrap();
            }
            let g8 = t.elapsed().as_secs_f64() * 1e6 / n as f64;
            // Leopard GF(2^16)
            let rec = reed_solomon_simd::encode(k, m, &data).unwrap();
            let t = Instant::now();
            for _ in 0..n {
                let originals = data
                    .iter()
                    .enumerate()
                    .skip(1)
                    .map(|(i, d)| (i, d.as_slice()));
                let recovery = rec
                    .iter()
                    .enumerate()
                    .take(1)
                    .map(|(i, d)| (i, d.as_slice()));
                reed_solomon_simd::decode(k, m, originals, recovery).unwrap();
            }
            let leo = t.elapsed().as_secs_f64() * 1e6 / n as f64;
            eprintln!("k={k:3} m={m:3} s={s:5}: GF(2^8) {g8:7.1} us | Leopard {leo:7.1} us (incl. clone of shards for GF8)");
        }
    }
}
