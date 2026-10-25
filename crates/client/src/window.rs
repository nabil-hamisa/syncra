//! The application window, its message loop and input capture. This runs on
//! the main (UI) thread only: Windows can block a window's owning thread
//! inside message dispatch (moves, focus changes, DWM), so streaming, decoding
//! and presenting live on a separate thread and talk to the window through
//! atomics and posted messages.
//!
//! Input is captured here and sent immediately (`input::send`):
//! - absolute mouse: window position mapped through the letterbox to video coordinates;
//! - relative mouse: raw input deltas, with the cursor clipped and hidden, used
//!   automatically while the host cursor is hidden (a game captured the mouse);
//! - keyboard: scancodes with the extended bit; Alt/F10 do not open the system menu.
//!
//! Ctrl+Alt releases a captured mouse (like a VM); clicking the window recaptures it.
//! Ctrl+Shift+F toggles borderless fullscreen.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use proto::input::InputEvent;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::cursor::{self, NativeCursor};
use crate::input;
use proto::input::CursorShape;

/// Posted to the window to apply a title set from another thread.
const WM_APP_TITLE: u32 = WM_APP + 1;
/// Posted when the host cursor shape or visibility changed.
const WM_APP_CURSOR: u32 = WM_APP + 2;

static CLOSED: AtomicBool = AtomicBool::new(false);
static FOCUSED: AtomicBool = AtomicBool::new(false);
/// Latest client size as (w << 32 | h); 0 = no pending resize.
static RESIZE: AtomicU64 = AtomicU64::new(0);
/// Current client size and decoded video size, (w << 32 | h).
static CLIENT_SIZE: AtomicU64 = AtomicU64::new(0);
static VIDEO_SIZE: AtomicU64 = AtomicU64::new(0);
static TITLE: Mutex<String> = Mutex::new(String::new());
static HWND_RAW: AtomicU64 = AtomicU64::new(0);

/// Host cursor as reported over the network.
static HOST_CURSOR_KNOWN: AtomicBool = AtomicBool::new(false);
static HOST_CURSOR_VISIBLE: AtomicBool = AtomicBool::new(true);
static CURSOR: Mutex<Option<NativeCursor>> = Mutex::new(None);
/// Shape received by the stream thread, not yet turned into `CURSOR`.
static PENDING_SHAPE: Mutex<Option<CursorShape>> = Mutex::new(None);
/// Relative (captured) mouse mode is active.
static RELATIVE: AtomicBool = AtomicBool::new(false);
/// The user released the captured mouse with Ctrl+Alt.
static RELEASED: AtomicBool = AtomicBool::new(false);
/// Windowed placement to restore when leaving fullscreen (None: windowed).
static WINDOWED: Mutex<Option<WINDOWPLACEMENT>> = Mutex::new(None);

/// Window handle that can be sent to the render thread.
#[derive(Clone, Copy)]
pub struct WindowHandle(isize);

impl WindowHandle {
    pub fn hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

fn pack(w: u32, h: u32) -> u64 {
    (w as u64) << 32 | h as u64
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

fn hwnd() -> HWND {
    HWND(HWND_RAW.load(Ordering::Acquire) as *mut _)
}

pub fn create(
    title: &str,
    width: u32,
    height: u32,
    pos: Option<(i32, i32)>,
    fullscreen: bool,
) -> Result<WindowHandle> {
    // A process can open several sessions one after another (combined app).
    CLOSED.store(false, Ordering::Release);
    RESIZE.store(0, Ordering::Release);
    VIDEO_SIZE.store(0, Ordering::Release);
    HOST_CURSOR_KNOWN.store(false, Ordering::Release);
    RELATIVE.store(false, Ordering::Release);
    RELEASED.store(false, Ordering::Release);
    *WINDOWED.lock().unwrap() = None;
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("SyncraClient");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        let (x, y) = pos.unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            windows::core::PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        HWND_RAW.store(hwnd.0 as u64, Ordering::Release);
        CLIENT_SIZE.store(pack(width, height), Ordering::Release);

        // Raw mouse input for relative mode (delivered only while focused).
        let rid = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RAWINPUTDEVICE_FLAGS(0),
            hwndTarget: hwnd,
        };
        RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32)?;

        let _ = ShowWindow(hwnd, SW_SHOW);
        if fullscreen {
            toggle_fullscreen(hwnd);
        }
        Ok(WindowHandle(hwnd.0 as isize))
    }
}

/// Run the message loop until the window is destroyed or [`close`] is called.
pub fn run_message_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    CLOSED.store(true, Ordering::Release);
}

pub fn is_closed() -> bool {
    CLOSED.load(Ordering::Acquire)
}

pub fn is_focused() -> bool {
    FOCUSED.load(Ordering::Acquire)
}

/// Close the window from any thread.
pub fn close() {
    CLOSED.store(true, Ordering::Release);
    unsafe {
        let _ = PostMessageW(Some(hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// Take a pending resize, if any.
pub fn take_resize() -> Option<(u32, u32)> {
    match RESIZE.swap(0, Ordering::AcqRel) {
        0 => None,
        v => Some(unpack(v)),
    }
}

/// Set the title from any thread without blocking on the UI thread.
pub fn set_title(title: &str) {
    *TITLE.lock().unwrap() = title.to_owned();
    unsafe {
        let _ = PostMessageW(Some(hwnd()), WM_APP_TITLE, WPARAM(0), LPARAM(0));
    }
}

/// Size of the decoded video, used to map mouse positions.
pub fn set_video_size(w: u32, h: u32) {
    VIDEO_SIZE.store(pack(w, h), Ordering::Release);
}

/// Host cursor visibility changed (from the stream thread).
pub fn set_host_cursor_visible(visible: bool) {
    let was_known = HOST_CURSOR_KNOWN.swap(true, Ordering::AcqRel);
    if HOST_CURSOR_VISIBLE.swap(visible, Ordering::AcqRel) != visible || !was_known {
        unsafe {
            let _ = PostMessageW(Some(hwnd()), WM_APP_CURSOR, WPARAM(0), LPARAM(0));
        }
    }
}

/// New host cursor shape (from the stream thread). The native cursor is
/// built on the UI thread so GDI work never delays a video frame.
pub fn set_cursor_shape(shape: CursorShape) {
    *PENDING_SHAPE.lock().unwrap() = Some(shape);
    unsafe {
        let _ = PostMessageW(Some(hwnd()), WM_APP_CURSOR, WPARAM(0), LPARAM(0));
    }
}

/// UI thread: turn a pending shape into the current native cursor.
fn build_pending_cursor() {
    let Some(shape) = PENDING_SHAPE.lock().unwrap().take() else {
        return;
    };
    match cursor::create(&shape) {
        Ok(c) => *CURSOR.lock().unwrap() = Some(c),
        Err(e) => tracing::warn!("cursor shape {}: {e}", shape.shape_id),
    }
}

/// Switch between the normal window and a borderless window covering its
/// monitor.
unsafe fn toggle_fullscreen(hwnd: HWND) {
    let mut saved = WINDOWED.lock().unwrap();
    if let Some(p) = saved.take() {
        SetWindowLongPtrW(
            hwnd,
            GWL_STYLE,
            (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as isize,
        );
        let _ = SetWindowPlacement(hwnd, &p);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        return;
    }
    let mut p = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetWindowPlacement(hwnd, &mut p).is_err()
        || !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool()
    {
        return;
    }
    *saved = Some(p);
    let m = mi.rcMonitor;
    SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOP),
        m.left,
        m.top,
        m.right - m.left,
        m.bottom - m.top,
        SWP_FRAMECHANGED,
    );
}

/// Letterbox viewport of the video inside the client area: (x, y, w, h).
fn viewport() -> Option<(f32, f32, f32, f32)> {
    let (cw, ch) = unpack(CLIENT_SIZE.load(Ordering::Acquire));
    let (vw, vh) = unpack(VIDEO_SIZE.load(Ordering::Acquire));
    if cw == 0 || ch == 0 || vw == 0 || vh == 0 {
        return None;
    }
    let scale = (cw as f32 / vw as f32).min(ch as f32 / vh as f32);
    let (w, h) = (vw as f32 * scale, vh as f32 * scale);
    Some(((cw as f32 - w) / 2.0, (ch as f32 - h) / 2.0, w, h))
}

fn send_mouse_abs(lparam: LPARAM) {
    let x = (lparam.0 & 0xffff) as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
    if let Some((vx, vy, vw, vh)) = viewport() {
        let nx = ((x + 0.5 - vx) / vw).clamp(0.0, 1.0);
        let ny = ((y + 0.5 - vy) / vh).clamp(0.0, 1.0);
        input::send(InputEvent::MouseAbs {
            x: (nx * 65535.0) as u16,
            y: (ny * 65535.0) as u16,
        });
    }
}

fn mouse_button(hwnd: HWND, button: u8, down: bool) {
    unsafe {
        if down {
            SetCapture(hwnd);
        } else if GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
    }
    input::send(InputEvent::MouseButton { button, down });
}

/// Enter or leave relative mode according to focus and host cursor visibility.
unsafe fn update_mouse_mode(hwnd: HWND) {
    let want = FOCUSED.load(Ordering::Acquire)
        && !RELEASED.load(Ordering::Acquire)
        && HOST_CURSOR_KNOWN.load(Ordering::Acquire)
        && !HOST_CURSOR_VISIBLE.load(Ordering::Acquire);
    if RELATIVE.swap(want, Ordering::AcqRel) != want {
        if want {
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            let mut tl = POINT {
                x: r.left,
                y: r.top,
            };
            let mut br = POINT {
                x: r.right,
                y: r.bottom,
            };
            let _ = ClientToScreen(hwnd, &mut tl);
            let _ = ClientToScreen(hwnd, &mut br);
            let _ = ClipCursor(Some(&RECT {
                left: tl.x,
                top: tl.y,
                right: br.x,
                bottom: br.y,
            }));
            tracing::info!("host cursor hidden: relative mouse mode");
        } else {
            let _ = ClipCursor(None);
            tracing::info!("absolute mouse mode");
        }
    }
    apply_cursor();
}

/// Re-apply the cursor clip after the client area moved or resized.
unsafe fn reclip(hwnd: HWND) {
    if RELATIVE.swap(false, Ordering::AcqRel) {
        update_mouse_mode(hwnd);
    }
}

/// Show the host's cursor shape (or nothing) while over the client area.
unsafe fn apply_cursor() {
    let hide = RELATIVE.load(Ordering::Acquire) || !HOST_CURSOR_VISIBLE.load(Ordering::Acquire);
    if hide {
        SetCursor(None);
    } else if let Some(c) = CURSOR.lock().unwrap().as_ref() {
        SetCursor(Some(c.0));
    } else if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
        SetCursor(Some(arrow));
    }
}

unsafe fn cursor_in_client(hwnd: HWND) -> bool {
    let mut p = POINT::default();
    let mut r = RECT::default();
    if GetCursorPos(&mut p).is_err() || GetClientRect(hwnd, &mut r).is_err() {
        return false;
    }
    let mut tl = POINT {
        x: r.left,
        y: r.top,
    };
    let _ = ClientToScreen(hwnd, &mut tl);
    let (x, y) = (p.x - tl.x, p.y - tl.y);
    x >= 0 && y >= 0 && x < r.right && y < r.bottom
}

unsafe fn raw_mouse_delta(lparam: LPARAM) -> Option<(i32, i32)> {
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let n = GetRawInputData(
        HRAWINPUT(lparam.0 as *mut _),
        RID_INPUT,
        Some(&mut raw as *mut _ as *mut _),
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    if n == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    let m = raw.data.mouse;
    if m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        return None; // tablets / RDP: no relative deltas
    }
    Some((m.lLastX, m.lLastY))
}

/// Sum the relative motion of every raw mouse report still queued.
unsafe fn drain_raw_mouse(dx: &mut i32, dy: &mut i32) {
    // u64 storage: buffered RAWINPUT blocks are 8-byte aligned (NEXTRAWINPUTBLOCK).
    let mut buf = [0u64; 1024];
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    loop {
        let mut size = std::mem::size_of_val(&buf) as u32;
        let n = GetRawInputBuffer(Some(buf.as_mut_ptr() as *mut RAWINPUT), &mut size, header);
        if n == 0 || n == u32::MAX {
            return;
        }
        let base = buf.as_ptr() as *const u8;
        let end = base.add(std::mem::size_of_val(&buf));
        let mut p = base;
        for _ in 0..n {
            // SAFETY: GetRawInputBuffer wrote `n` consecutive blocks starting at
            // `base`, each `dwSize` bytes and 8-byte aligned; bounds-checked below.
            if p.add(header as usize) > end {
                return;
            }
            let ri = &*(p as *const RAWINPUT);
            let size = ri.header.dwSize as usize;
            if size < header as usize || p.add(size) > end {
                return;
            }
            if ri.header.dwType == RIM_TYPEMOUSE.0
                && ri.data.mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0
            {
                *dx += ri.data.mouse.lLastX;
                *dy += ri.data.mouse.lLastY;
            }
            p = p.add((size + 7) & !7);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            if !RELATIVE.load(Ordering::Acquire) {
                send_mouse_abs(lparam);
            }
            LRESULT(0)
        }
        WM_INPUT => {
            if RELATIVE.load(Ordering::Acquire) {
                // Coalesce: this message plus everything already queued
                // becomes one MouseRel, so a 4-8 kHz mouse doesn't cost one
                // datagram and one SendInput per report.
                let (mut dx, mut dy) = raw_mouse_delta(lparam).unwrap_or((0, 0));
                drain_raw_mouse(&mut dx, &mut dy);
                if dx != 0 || dy != 0 {
                    let c = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    input::send(InputEvent::MouseRel {
                        dx: c(dx),
                        dy: c(dy),
                    });
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            if msg == WM_LBUTTONDOWN && RELEASED.swap(false, Ordering::AcqRel) {
                update_mouse_mode(hwnd); // recapture; this click is not forwarded
                return LRESULT(0);
            }
            mouse_button(hwnd, 0, msg == WM_LBUTTONDOWN);
            LRESULT(0)
        }
        WM_RBUTTONDOWN | WM_RBUTTONUP => {
            mouse_button(hwnd, 1, msg == WM_RBUTTONDOWN);
            LRESULT(0)
        }
        WM_MBUTTONDOWN | WM_MBUTTONUP => {
            mouse_button(hwnd, 2, msg == WM_MBUTTONDOWN);
            LRESULT(0)
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let which = ((wparam.0 >> 16) & 0xffff) as u8; // XBUTTON1 = 1, XBUTTON2 = 2
            mouse_button(hwnd, 2 + which, msg == WM_XBUTTONDOWN);
            LRESULT(1)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as i16;
            input::send(InputEvent::MouseWheel {
                delta,
                horizontal: msg == WM_MOUSEHWHEEL,
            });
            LRESULT(0)
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let scancode = ((lparam.0 >> 16) & 0xff) as u16;
            let extended = (lparam.0 >> 24) & 1 != 0;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let held = |vk: u16| GetKeyState(vk as i32) < 0;
            if down && scancode == 0x21 && held(VK_CONTROL.0) && held(VK_SHIFT.0) {
                // Ctrl+Shift+F: the F itself is not forwarded.
                toggle_fullscreen(hwnd);
                return LRESULT(0);
            }
            if scancode != 0 {
                input::send(InputEvent::Key {
                    scancode,
                    extended,
                    down,
                });
            }
            if down && RELATIVE.load(Ordering::Acquire) && held(VK_CONTROL.0) && held(VK_MENU.0) {
                RELEASED.store(true, Ordering::Release);
                update_mouse_mode(hwnd);
                tracing::info!("mouse released (Ctrl+Alt); click the window to capture again");
            }
            // Swallow so Alt/F10 don't trigger the window menu; everything goes to the host.
            LRESULT(0)
        }
        WM_SYSCOMMAND if (wparam.0 & 0xfff0) as u32 == SC_KEYMENU => LRESULT(0),
        WM_SETCURSOR if (lparam.0 & 0xffff) as u32 == HTCLIENT => {
            apply_cursor();
            LRESULT(1)
        }
        WM_SETFOCUS => {
            FOCUSED.store(true, Ordering::Release);
            update_mouse_mode(hwnd);
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            FOCUSED.store(false, Ordering::Release);
            RELEASED.store(false, Ordering::Release);
            input::send(InputEvent::ReleaseAll);
            update_mouse_mode(hwnd);
            LRESULT(0)
        }
        WM_APP_CURSOR => {
            build_pending_cursor();
            update_mouse_mode(hwnd);
            if cursor_in_client(hwnd) {
                apply_cursor();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let w = (lparam.0 & 0xffff) as u32;
            let h = ((lparam.0 >> 16) & 0xffff) as u32;
            if w > 0 && h > 0 {
                CLIENT_SIZE.store(pack(w, h), Ordering::Release);
                RESIZE.store(pack(w, h), Ordering::Release);
                reclip(hwnd);
            }
            LRESULT(0)
        }
        WM_MOVE | WM_DISPLAYCHANGE => {
            reclip(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_TITLE => {
            let t: Vec<u16> = TITLE
                .lock()
                .unwrap()
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(t.as_ptr()));
            LRESULT(0)
        }
        WM_DESTROY => {
            CLOSED.store(true, Ordering::Release);
            let _ = ClipCursor(None);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
