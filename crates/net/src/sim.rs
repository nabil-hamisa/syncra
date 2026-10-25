//! In-process network impairment for reproducible tests (no driver needed).
//!
//! Models one direction of a link: a bottleneck of `bw` with a drop-tail
//! queue of `queue` worth of transmission time, then `delay` of propagation
//! plus up to `jitter` extra, and Gilbert-Elliott bursty random loss with a
//! long-run rate `loss` and mean burst length `burst`.
//!
//! Spec string, comma separated, any subset:
//! `loss=2%,burst=3,delay=20ms,jitter=2ms,bw=20mbps,queue=50ms`

use std::collections::BinaryHeap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimConfig {
    pub loss: f64,
    pub burst: f64,
    pub delay: Duration,
    pub jitter: Duration,
    /// Bottleneck bandwidth in bits/s (0 = unlimited).
    pub bw_bps: f64,
    pub queue: Duration,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            loss: 0.0,
            burst: 1.0,
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            bw_bps: 0.0,
            queue: Duration::from_millis(50),
        }
    }
}

impl std::str::FromStr for SimConfig {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let mut c = SimConfig::default();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("expected key=value, got {part:?}"))?;
            let num = |v: &str, suffixes: &[(&str, f64)]| -> Result<f64, String> {
                let v = v.trim().to_ascii_lowercase();
                for (suf, mul) in suffixes {
                    if let Some(n) = v.strip_suffix(suf) {
                        return n
                            .trim()
                            .parse::<f64>()
                            .map(|n| n * mul)
                            .map_err(|e| format!("{part}: {e}"));
                    }
                }
                v.parse::<f64>().map_err(|e| format!("{part}: {e}"))
            };
            let dur = |v: &str| {
                num(v, &[("ms", 1e-3), ("us", 1e-6), ("s", 1.0)]).map(Duration::from_secs_f64)
            };
            match k.trim() {
                "loss" => c.loss = num(v, &[("%", 0.01)])?,
                "burst" => c.burst = num(v, &[])?.max(1.0),
                "delay" => c.delay = dur(v)?,
                "jitter" => c.jitter = dur(v)?,
                "bw" => {
                    c.bw_bps = num(
                        v,
                        &[("gbps", 1e9), ("mbps", 1e6), ("kbps", 1e3), ("bps", 1.0)],
                    )?
                }
                "queue" => c.queue = dur(v)?,
                other => return Err(format!("unknown sim key {other:?}")),
            }
        }
        if !(0.0..1.0).contains(&c.loss) {
            return Err("loss must be in [0, 100%)".into());
        }
        Ok(c)
    }
}

struct Pending {
    at: Instant,
    seq: u64,
    data: Vec<u8>,
    to: SocketAddr,
}

impl PartialEq for Pending {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Pending {
    // Min-heap on release time.
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

struct State {
    heap: BinaryHeap<Pending>,
    link_free: Instant,
    bad: bool,
    rng: u64,
    seq: u64,
    stats: SimStats,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SimStats {
    pub sent: u64,
    pub dropped_random: u64,
    pub dropped_queue: u64,
}

/// A simulated link. Datagrams go in with [`SimLink::send`] and come out of
/// `socket` at their arrival time, from a dedicated thread.
pub struct SimLink {
    cfg: SimConfig,
    state: Mutex<State>,
    wake: Condvar,
}

impl SimLink {
    pub fn new(cfg: SimConfig, socket: UdpSocket) -> Arc<Self> {
        let link = Arc::new(SimLink {
            cfg,
            state: Mutex::new(State {
                heap: BinaryHeap::new(),
                link_free: Instant::now(),
                bad: false,
                rng: 0x9e37_79b9_7f4a_7c15,
                seq: 0,
                stats: SimStats::default(),
            }),
            wake: Condvar::new(),
        });
        let l = link.clone();
        std::thread::Builder::new()
            .name("netsim".into())
            .spawn(move || l.run(socket))
            .expect("spawn netsim");
        tracing::info!("network simulation: {cfg:?}");
        link
    }

    pub fn stats(&self) -> SimStats {
        self.state.lock().unwrap().stats
    }

    pub fn send(&self, data: &[u8], to: SocketAddr) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        st.stats.sent += 1;

        // Gilbert-Elliott: P(good→bad) chosen so the long-run loss equals `loss`.
        if self.cfg.loss > 0.0 {
            let p_bg = 1.0 / self.cfg.burst;
            let p_gb = self.cfg.loss * p_bg / (1.0 - self.cfg.loss);
            let r = next_f64(&mut st.rng);
            st.bad = if st.bad { r >= p_bg } else { r < p_gb };
            if st.bad {
                st.stats.dropped_random += 1;
                return;
            }
        }

        // Bottleneck: serialize behind earlier packets, drop when the queue is full.
        let mut depart = now;
        if self.cfg.bw_bps > 0.0 {
            let start = st.link_free.max(now);
            if start.duration_since(now) > self.cfg.queue {
                st.stats.dropped_queue += 1;
                return;
            }
            depart =
                start + Duration::from_secs_f64((data.len() + 28) as f64 * 8.0 / self.cfg.bw_bps);
            st.link_free = depart;
        }
        let jitter = if self.cfg.jitter.is_zero() {
            Duration::ZERO
        } else {
            self.cfg.jitter.mul_f64(next_f64(&mut st.rng))
        };
        let at = depart + self.cfg.delay + jitter;
        st.seq += 1;
        let seq = st.seq;
        st.heap.push(Pending {
            at,
            seq,
            data: data.to_vec(),
            to,
        });
        drop(st);
        self.wake.notify_one();
    }

    fn run(&self, socket: UdpSocket) {
        let mut st = self.state.lock().unwrap();
        loop {
            let Some(next) = st.heap.peek().map(|p| p.at) else {
                st = self.wake.wait(st).unwrap();
                continue;
            };
            let now = Instant::now();
            if next > now {
                let wait = next - now;
                if wait > Duration::from_micros(1500) {
                    st = self
                        .wake
                        .wait_timeout(st, wait - Duration::from_millis(1))
                        .unwrap()
                        .0;
                    continue;
                }
                // Spin the last stretch for sub-millisecond release accuracy.
                drop(st);
                while Instant::now() < next {
                    std::hint::spin_loop();
                }
                st = self.state.lock().unwrap();
                continue;
            }
            let p = st.heap.pop().unwrap();
            drop(st);
            let _ = socket.send_to(&p.data, p.to);
            st = self.state.lock().unwrap();
        }
    }
}

fn next_f64(s: &mut u64) -> f64 {
    // xorshift64*
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    (s.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec() {
        let c: SimConfig = "loss=2%,burst=3,delay=20ms,jitter=500us,bw=20mbps,queue=40ms"
            .parse()
            .unwrap();
        assert!((c.loss - 0.02).abs() < 1e-12);
        assert_eq!(c.burst, 3.0);
        assert_eq!(c.delay, Duration::from_millis(20));
        assert_eq!(c.jitter, Duration::from_micros(500));
        assert_eq!(c.bw_bps, 20e6);
        assert_eq!(c.queue, Duration::from_millis(40));
        assert!("loss=150%".parse::<SimConfig>().is_err());
        assert!("nope=1".parse::<SimConfig>().is_err());
    }

    #[test]
    fn loss_rate_and_delay_are_applied() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let link = SimLink::new("loss=10%,burst=2,delay=15ms".parse().unwrap(), tx);
        let to = rx.local_addr().unwrap();
        let t0 = Instant::now();
        let n = 4000;
        for i in 0..n as u32 {
            link.send(&i.to_le_bytes(), to);
        }
        let mut got = 0;
        let mut first = None;
        let mut buf = [0u8; 16];
        while rx.recv_from(&mut buf).is_ok() {
            first.get_or_insert(t0.elapsed());
            got += 1;
        }
        let loss = 1.0 - got as f64 / n as f64;
        eprintln!("sim loss {:.3}, first arrival after {:?}", loss, first);
        assert!((0.07..0.13).contains(&loss), "loss {loss}");
        assert!(first.unwrap() >= Duration::from_millis(14));
    }

    #[test]
    fn bottleneck_queues_then_drops() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        // 10 Mbps, 20 ms queue: ~25 x 1000-byte packets fit, the rest drop.
        let link = SimLink::new("bw=10mbps,queue=20ms".parse().unwrap(), tx);
        let to = rx.local_addr().unwrap();
        let t0 = Instant::now();
        for _ in 0..100 {
            link.send(&[0u8; 1000], to);
        }
        let mut got = 0;
        let mut last = Duration::ZERO;
        let mut buf = [0u8; 2000];
        while rx.recv_from(&mut buf).is_ok() {
            got += 1;
            last = t0.elapsed();
        }
        let st = link.stats();
        eprintln!(
            "bottleneck: got {got}, queue drops {}, last arrival {last:?}",
            st.dropped_queue
        );
        assert!((20..=30).contains(&got), "got {got}");
        assert_eq!(st.dropped_queue as usize + got, 100);
        assert!(
            last >= Duration::from_millis(18),
            "queue drained at line rate"
        );
    }
}
