//! Microsecond timestamps.
//!
//! On Windows this is the performance counter: QPC is system-wide, so host
//! and client on the same machine share one timebase and can compute exact
//! cross-process latency. Elsewhere (the Linux probe) it is the monotonic
//! clock, which is only meaningful within one process. Across machines a
//! clock offset estimate is needed (later phase).

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    fn frequency() -> u64 {
        static FREQ: OnceLock<u64> = OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: writes one i64 through a valid pointer.
            unsafe {
                let _ = QueryPerformanceFrequency(&mut f);
            }
            f as u64
        })
    }

    pub fn now_us() -> u64 {
        let mut c = 0i64;
        // SAFETY: writes one i64 through a valid pointer.
        unsafe {
            let _ = QueryPerformanceCounter(&mut c);
        }
        qpc_to_us(c)
    }

    pub fn qpc_to_us(ticks: i64) -> u64 {
        let f = frequency();
        let t = ticks as u64;
        (t / f) * 1_000_000 + (t % f) * 1_000_000 / f
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::OnceLock;
    use std::time::Instant;

    pub fn now_us() -> u64 {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        // Offset by 1 s so no timestamp is 0 (0 means "unset" in places).
        EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1_000_000
    }
}

/// Current time in microseconds.
pub fn now_us() -> u64 {
    imp::now_us()
}

/// Convert a raw QPC tick value (e.g. DXGI `LastPresentTime`) to microseconds.
#[cfg(windows)]
pub fn qpc_to_us(ticks: i64) -> u64 {
    imp::qpc_to_us(ticks)
}
