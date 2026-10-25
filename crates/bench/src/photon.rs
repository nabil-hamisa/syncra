//! Click-to-photon, measured the same way for any streaming app.
//!
//! A borderless "target" square sits on the host's desktop and toggles
//! between black and white on every left click it receives. The bench
//! clicks it through the client window (move + down + up in one
//! `SendInput`, so the click lands on the client, which forwards it), then
//! watches the client window's pixels with desktop duplication until the
//! toggle shows up. The result runs from `SendInput` to the composed frame
//! (`LastPresentTime`), so it includes input forwarding, injection, the
//! target's repaint, capture, encode, network, decode and present, but not
//! the monitor's scanout. `--direct` clicks the target itself: the same
//! measurement with no streaming in between, the local baseline.
//!
//! Host and client must run on this machine with the target on the captured
//! monitor and the client window on another one. The target is
//! `WS_EX_NOACTIVATE`, so the client keeps the keyboard focus.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Opts {
    pub samples: u32,
    /// Click the target directly (no streaming): the local baseline.
    pub direct: bool,
    /// Target square on the host desktop: x, y, size in physical pixels.
    pub target: [i32; 3],
    /// Window class of the client's stream window.
    pub class: String,
}

/// White (true) or black.
static WHITE: AtomicBool = AtomicBool::new(false);
/// QPC when the target last received a click.
static CLICKED_AT: AtomicI64 = AtomicI64::new(0);
static TARGET: AtomicIsize = AtomicIsize::new(0);

fn qpc() -> i64 {
    let mut t = 0;
    // SAFETY: plain out-parameter.
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
    }
    t
}

fn qpc_freq() -> i64 {
    let mut f = 0;
    // SAFETY: plain out-parameter.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f
}

pub fn run(o: Opts) -> Result<()> {
    // SAFETY: before any window; desktop duplication needs per-monitor v2.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let [tx, ty, ts] = o.target;
    std::thread::Builder::new()
        .name("target".into())
        .spawn(move || {
            if let Err(e) = target_window(tx, ty, ts) {
                tracing::error!("target window: {e:#}");
            }
        })?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while TARGET.load(Ordering::Acquire) == 0 {
        if Instant::now() > deadline {
            bail!("target window did not open");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Park the cursor on the captured monitor: with it on the client's
    // monitor the host reports a hidden cursor and the client switches to
    // relative mouse mode, where clicks land wherever the host cursor is.
    // SAFETY: no preconditions.
    unsafe {
        let _ = SetCursorPos(tx + ts / 2, ty + ts / 2);
    }
    std::thread::sleep(Duration::from_millis(500));

    // Click at the target's center; watch its upper-left quarter, clear of
    // the cursor arrow (the client draws its cursor into the video).
    let (click, watch) = if o.direct {
        (
            POINT {
                x: tx + ts / 2,
                y: ty + ts / 2,
            },
            POINT {
                x: tx + ts / 4,
                y: ty + ts / 4,
            },
        )
    } else {
        let to = |fx: f32, fy: f32| map_to_client(&o.class, tx as f32 + fx, ty as f32 + fy);
        let half = ts as f32 / 2.0;
        let quarter = ts as f32 / 4.0;
        (to(half, half)?, to(quarter, quarter)?)
    };
    tracing::info!(
        "click at ({}, {}), watching ({}, {}){}",
        click.x,
        click.y,
        watch.x,
        watch.y,
        if o.direct { " (direct)" } else { "" }
    );

    let mut dup = Watcher::open(watch)?;
    let freq = qpc_freq();
    let us = |ticks: i64| (ticks * 1_000_000 / freq) as u32;
    let mut photon = Vec::with_capacity(o.samples as usize);
    let mut input = Vec::with_capacity(o.samples as usize);
    let mut missed = 0;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;

    // The first click can be eaten (focus); settle the state first.
    dup.drain();
    WHITE.store(dup.read_white()?.unwrap_or(false), Ordering::Release);

    for k in 0..o.samples {
        // Random phase against the display and capture cadence.
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(40 + rng % 80));
        dup.drain();

        let want = !WHITE.load(Ordering::Acquire);
        let before = CLICKED_AT.load(Ordering::Acquire);
        let t0 = qpc();
        click_at(click)?;
        match dup.wait_for(want, Duration::from_millis(1000))? {
            Some(present) => {
                photon.push(us(present - t0));
                let at = CLICKED_AT.load(Ordering::Acquire);
                if at != before {
                    input.push(us(at - t0));
                }
            }
            None => {
                missed += 1;
                tracing::warn!("sample {k}: no change seen within 1 s");
                // Resync with whatever the screen shows now.
                if let Some(white) = dup.read_white()? {
                    WHITE.store(white, Ordering::Release);
                }
                if missed > 10 {
                    bail!("too many missed samples: is the client window showing the target?");
                }
            }
        }
    }
    let mode = if o.direct { "direct" } else { "streamed" };
    crate::report(&format!("click → photon ({mode})"), &mut photon);
    if !input.is_empty() {
        crate::report(&format!("click → target window input ({mode})"), &mut input);
    }
    if missed > 0 {
        tracing::warn!("{missed} samples missed");
    }
    // SAFETY: closing our own window from another thread is a posted message.
    unsafe {
        let _ = PostMessageW(
            Some(HWND(TARGET.load(Ordering::Acquire) as *mut _)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
    Ok(())
}

/// Screen point inside the client window that shows host desktop point
/// (hx, hy), given the client letterboxes the captured monitor.
fn map_to_client(class: &str, hx: f32, hy: f32) -> Result<POINT> {
    // SAFETY: Win32 queries with valid out-parameters.
    unsafe {
        let hwnd = FindWindowW(&HSTRING::from(class), PCWSTR::null()).with_context(|| {
            format!("no client window of class {class} (start the client first)")
        })?;
        let mon = MonitorFromPoint(
            POINT {
                x: hx as i32,
                y: hy as i32,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let m = mi.rcMonitor;
        let (vw, vh) = ((m.right - m.left) as f32, (m.bottom - m.top) as f32);
        let mut cr = RECT::default();
        GetClientRect(hwnd, &mut cr)?;
        let mut origin = POINT::default();
        let _ = ClientToScreen(hwnd, &mut origin);
        let (cw, ch) = (cr.right as f32, cr.bottom as f32);
        let scale = (cw / vw).min(ch / vh);
        let (w, h) = (vw * scale, vh * scale);
        let (ox, oy) = ((cw - w) / 2.0, (ch - h) / 2.0);
        Ok(POINT {
            x: origin.x + (ox + (hx - m.left as f32) * scale) as i32,
            y: origin.y + (oy + (hy - m.top as f32) * scale) as i32,
        })
    }
}

/// Move to `p` and click, as one input so nothing can move the cursor in
/// between (the host's injected moves would otherwise race the click).
fn click_at(p: POINT) -> Result<()> {
    // SAFETY: GetSystemMetrics has no preconditions; INPUTs are fully
    // initialized and cbSize matches.
    unsafe {
        let (vx, vy) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        );
        let (vw, vh) = (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        let nx = ((p.x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
        let ny = ((p.y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
        let m = |flags: MOUSE_EVENT_FLAGS| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: nx,
                    dy: ny,
                    dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    ..Default::default()
                },
            },
        };
        let inputs = [
            m(MOUSEEVENTF_MOVE),
            m(MOUSEEVENTF_LEFTDOWN),
            m(MOUSEEVENTF_LEFTUP),
        ];
        if SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) != inputs.len() as u32 {
            bail!("SendInput failed: {}", windows::core::Error::from_thread());
        }
    }
    Ok(())
}

fn target_window(x: i32, y: i32, size: i32) -> Result<()> {
    // SAFETY: standard class registration and window creation on this
    // thread; the message loop below owns the window.
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("SyncraBenchTarget");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(target_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class,
            w!("bench target"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            size,
            size,
            None,
            None,
            Some(instance),
            None,
        )?;
        TARGET.store(hwnd.0 as isize, Ordering::Release);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Fill the window with the current state, right away (no WM_PAINT round trip).
unsafe fn paint(hwnd: HWND, hdc: HDC) {
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    let color = if WHITE.load(Ordering::Acquire) {
        WHITE_BRUSH
    } else {
        BLACK_BRUSH
    };
    FillRect(hdc, &r, HBRUSH(GetStockObject(color).0));
}

extern "system" fn target_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by the system on the window's thread with valid handles.
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_LBUTTONDOWN => {
                CLICKED_AT.store(qpc(), Ordering::Release);
                WHITE.fetch_xor(true, Ordering::AcqRel);
                let hdc = GetDC(Some(hwnd));
                paint(hwnd, hdc);
                ReleaseDC(Some(hwnd), hdc);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Desktop duplication of the monitor holding one screen point, reading a
/// 4x4 block there.
struct Watcher {
    dup: IDXGIOutputDuplication,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    region: D3D11_BOX,
}

impl Watcher {
    fn open(p: POINT) -> Result<Self> {
        // SAFETY: DXGI/D3D11 calls with valid out-parameters; every COM
        // object is owned by a smart pointer and released on drop.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    o += 1;
                    let r = output.GetDesc()?.DesktopCoordinates;
                    if p.x < r.left || p.x >= r.right || p.y < r.top || p.y >= r.bottom {
                        continue;
                    }
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_FLAG(0),
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                    .context("D3D11CreateDevice")?;
                    let device: ID3D11Device = device.unwrap();
                    let dup = output
                        .cast::<IDXGIOutput1>()?
                        .DuplicateOutput(&device)
                        .context("DuplicateOutput")?;
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: 4,
                        Height: 4,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        ..Default::default()
                    };
                    let mut staging = None;
                    device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                    let (lx, ly) = ((p.x - r.left) as u32, (p.y - r.top) as u32);
                    return Ok(Watcher {
                        dup,
                        context: context.unwrap(),
                        staging: staging.unwrap(),
                        region: D3D11_BOX {
                            left: lx,
                            top: ly,
                            front: 0,
                            right: lx + 4,
                            bottom: ly + 4,
                            back: 1,
                        },
                    });
                }
            }
            bail!("no monitor contains ({}, {})", p.x, p.y)
        }
    }

    /// Next composed frame within `timeout`: (LastPresentTime, white?), or
    /// None. Frames with only a pointer update are skipped.
    fn next(&mut self, timeout: Duration) -> Result<Option<(i64, Option<bool>)>> {
        // SAFETY: each successful AcquireNextFrame is paired with ReleaseFrame;
        // the staging texture is mapped and unmapped on this thread.
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            match self
                .dup
                .AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut res)
            {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
            let r = (|| -> Result<Option<(i64, Option<bool>)>> {
                if info.LastPresentTime == 0 {
                    return Ok(Some((0, None)));
                }
                let tex: ID3D11Texture2D = res.unwrap().cast()?;
                self.context.CopySubresourceRegion(
                    &self.staging,
                    0,
                    0,
                    0,
                    0,
                    &tex,
                    0,
                    Some(&self.region),
                );
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                let mut sum = 0u32;
                for row in 0..4 {
                    let line = std::slice::from_raw_parts(
                        (m.pData as *const u8).add(row * m.RowPitch as usize),
                        16,
                    );
                    for px in line.as_chunks::<4>().0 {
                        // BGRA → approximate luma.
                        sum += (px[0] as u32 + 5 * px[1] as u32 + 2 * px[2] as u32) / 8;
                    }
                }
                self.context.Unmap(&self.staging, 0);
                let luma = sum / 16;
                let state = if luma > 160 {
                    Some(true)
                } else if luma < 96 {
                    Some(false)
                } else {
                    None // mid-transition or not our square
                };
                Ok(Some((info.LastPresentTime, state)))
            })();
            let _ = self.dup.ReleaseFrame();
            r
        }
    }

    /// Discard frames already queued so the next one is fresh.
    fn drain(&mut self) {
        while let Ok(Some(_)) = self.next(Duration::ZERO) {}
    }

    /// The current state, from the next frame (forcing one with a repaint).
    fn read_white(&mut self) -> Result<Option<bool>> {
        // SAFETY: invalidating our own window from any thread is allowed.
        unsafe {
            let _ = InvalidateRect(
                Some(HWND(TARGET.load(Ordering::Acquire) as *mut _)),
                None,
                true,
            );
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Some((t, s)) = self.next(Duration::from_millis(100))? {
                if t != 0 && s.is_some() {
                    return Ok(s);
                }
            }
        }
        Ok(None)
    }

    /// LastPresentTime of the first frame showing `white`.
    fn wait_for(&mut self, white: bool, timeout: Duration) -> Result<Option<i64>> {
        let deadline = Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if let Some((t, Some(s))) = self.next(left)? {
                if s == white && t != 0 {
                    return Ok(Some(t));
                }
            }
        }
        Ok(None)
    }
}
