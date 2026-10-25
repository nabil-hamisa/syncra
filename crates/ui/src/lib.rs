//! Minimal native Win32 forms for the host and client windows: labels,
//! buttons, edit boxes, fonts, DPI scaling and the clipboard. No UI
//! framework: these windows are a launcher and a status panel, nothing on
//! the stream path depends on them.
//!
//! Layout coordinates are in 96-DPI units and scaled to the window's DPI.

use anyhow::Result;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

pub use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
pub use windows::Win32::UI::WindowsAndMessaging::{
    WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY, WM_TIMER,
};

/// Control styles (winuser.h values; the bindings scatter them over modules
/// and integer types).
pub mod style {
    pub const SS_LEFT: u32 = 0x0;
    pub const SS_CENTER: u32 = 0x1;
    pub const BS_PUSHBUTTON: u32 = 0x0;
    pub const BS_DEFPUSHBUTTON: u32 = 0x1;
    pub const BS_OWNERDRAW: u32 = 0xB;
    pub const ES_CENTER: u32 = 0x1;
    pub const ES_AUTOHSCROLL: u32 = 0x80;
    pub const LBS_NOTIFY: u32 = 0x1;
    pub const LBS_OWNERDRAWFIXED: u32 = 0x10;
    pub const LBS_NOINTEGRALHEIGHT: u32 = 0x100;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
}

/// Accent colour for the big ID text.
pub const ACCENT: COLORREF = COLORREF(0x00C0_6A1F); // BGR: #1F6AC0
pub const MUTED: COLORREF = COLORREF(0x0070_7070);

pub use windows::Win32::Foundation::{COLORREF, RECT};
pub use windows::Win32::Graphics::Gdi::{HDC, HFONT};

/// `#rrggbb` as a GDI colour (which is BGR).
pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

pub fn instance() -> HINSTANCE {
    // SAFETY: the module handle of the running executable.
    unsafe { GetModuleHandleW(None).map(Into::into).unwrap_or_default() }
}

/// Register a window class with a white background.
pub fn register_class(name: PCWSTR, proc: WNDPROC) -> Result<()> {
    // SAFETY: stock objects need no cleanup.
    register_class_brush(name, proc, HBRUSH(unsafe { GetStockObject(WHITE_BRUSH) }.0))
}

/// Register a window class with a solid `background`.
pub fn register_class_color(name: PCWSTR, proc: WNDPROC, background: COLORREF) -> Result<()> {
    // SAFETY: the class owns the brush for the life of the process.
    register_class_brush(name, proc, unsafe { CreateSolidBrush(background) })
}

fn register_class_brush(name: PCWSTR, proc: WNDPROC, brush: HBRUSH) -> Result<()> {
    // SAFETY: plain class registration with static strings and a valid proc.
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: proc,
            hInstance: instance(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: brush,
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
    Ok(())
}

/// A fixed-size top-level window, `w` x `h` client area at 96 DPI (scaled),
/// centered on the primary monitor. Not shown yet.
pub fn create_window(class: PCWSTR, title: &str, w: i32, h: i32) -> Result<HWND> {
    // Clip children: painted backgrounds never cover the controls.
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    // SAFETY: standard window creation; strings outlive the calls.
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            &HSTRING::from(title),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w,
            h,
            None,
            None,
            Some(instance()),
            None,
        )?;
        // Now that the DPI is known, size the client area and center.
        let (cw, ch) = (scale(hwnd, w), scale(hwnd, h));
        let mut r = RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        let _ = AdjustWindowRect(&mut r, style, false);
        let (ww, wh) = (r.right - r.left, r.bottom - r.top);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let _ = SetWindowPos(
            hwnd,
            None,
            (sw - ww) / 2,
            (sh - wh) / 2,
            ww,
            wh,
            SWP_NOZORDER,
        );
        Ok(hwnd)
    }
}

pub fn show(hwnd: HWND) {
    // SAFETY: valid window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 96-DPI units → pixels for this window.
pub fn scale(hwnd: HWND, v: i32) -> i32 {
    // SAFETY: valid window handle.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    v * dpi as i32 / 96
}

/// Segoe UI at `pt` points (scaled for the window's DPI).
pub fn font(hwnd: HWND, pt: i32, bold: bool) -> HFONT {
    font_face(hwnd, pt, bold, windows::core::w!("Segoe UI"))
}

/// The Windows icon font (Segoe MDL2 Assets, on every Windows 10/11) at
/// `pt` points.
pub fn icon_font(hwnd: HWND, pt: i32) -> HFONT {
    font_face(hwnd, pt, false, windows::core::w!("Segoe MDL2 Assets"))
}

fn font_face(hwnd: HWND, pt: i32, bold: bool, face: PCWSTR) -> HFONT {
    // SAFETY: font creation with a static face name.
    unsafe {
        CreateFontW(
            -scale(hwnd, pt * 96 / 72),
            0,
            0,
            0,
            if bold {
                FW_SEMIBOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            face,
        )
    }
}

/// Colour the title bar (Windows 11; older builds ignore it and keep the
/// default caption).
pub fn caption_color(hwnd: HWND, background: COLORREF, text: COLORREF) {
    use windows::Win32::Graphics::Dwm::*;
    for (attr, c) in [(DWMWA_CAPTION_COLOR, background), (DWMWA_TEXT_COLOR, text)] {
        // SAFETY: the attribute is a COLORREF read during the call.
        unsafe {
            let _ = DwmSetWindowAttribute(hwnd, attr, &c as *const COLORREF as *const _, 4);
        }
    }
}

/// A child control (`"STATIC"`, `"BUTTON"`, `"EDIT"`) at 96-DPI coordinates.
#[allow(clippy::too_many_arguments)]
pub fn child(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
    font: HFONT,
) -> Result<HWND> {
    let s = |v| scale(parent, v);
    // SAFETY: standard child window creation; the id is passed as the menu
    // handle as Win32 requires for child controls.
    unsafe {
        let ex = if class == windows::core::w!("EDIT") {
            WS_EX_CLIENTEDGE
        } else {
            WINDOW_EX_STYLE::default()
        };
        let hwnd = CreateWindowExW(
            ex,
            class,
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            s(x),
            s(y),
            s(w),
            s(h),
            Some(parent),
            Some(HMENU(id as *mut _)),
            Some(instance()),
            None,
        )?;
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        Ok(hwnd)
    }
}

pub fn set_text(hwnd: HWND, text: &str) {
    // SAFETY: valid window; the string outlives the call.
    unsafe {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(text));
    }
}

pub fn get_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: the buffer is writable for its length.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn enable(hwnd: HWND, on: bool) {
    // SAFETY: valid window.
    unsafe {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(hwnd, on);
    }
}

pub fn set_focus(hwnd: HWND) {
    // SAFETY: valid window on this thread.
    unsafe {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd));
    }
}

/// Labels on the white background, optionally coloured: handle
/// `WM_CTLCOLORSTATIC` by returning this.
pub fn static_color(wparam: WPARAM, color: Option<COLORREF>) -> LRESULT {
    let hdc = HDC(wparam.0 as *mut _);
    // SAFETY: the HDC comes from the WM_CTLCOLORSTATIC message.
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        if let Some(c) = color {
            SetTextColor(hdc, c);
        }
        LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
    }
}

/// Send a message to a control and return its result.
pub fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    // SAFETY: the caller passes parameters valid for `msg`.
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))).0 }
}

/// Repaint the whole window, or `r` (client pixels) of it, without erasing
/// first (the painting code covers the area).
pub fn redraw(hwnd: HWND, r: Option<RECT>) {
    // SAFETY: valid window; the rect outlives the call.
    unsafe {
        let _ = InvalidateRect(Some(hwnd), r.as_ref().map(|r| r as *const RECT), false);
    }
}

pub fn show_control(hwnd: HWND, on: bool) {
    // SAFETY: valid window.
    unsafe {
        let _ = ShowWindow(hwnd, if on { SW_SHOW } else { SW_HIDE });
    }
}

pub fn client_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: valid window, writable rect.
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    r
}

/// Replace a control's window procedure. Returns the previous one, for
/// [`call_proc`].
pub fn subclass(hwnd: HWND, proc: WNDPROC) -> isize {
    let new = proc.map_or(0, |p| p as usize as isize);
    // SAFETY: `proc` is a valid window procedure for the program's life.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new) }
}

/// Forward a message to the procedure [`subclass`] returned.
pub fn call_proc(old: isize, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: `old` came from GWLP_WNDPROC, so it is a window procedure
    // (or a handle CallWindowProc knows how to call).
    unsafe {
        let p: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
        CallWindowProcW(p, hwnd, msg, wparam, lparam)
    }
}

/// Ask Windows for WM_MOUSELEAVE once the cursor leaves `hwnd`.
pub fn track_leave(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    };
    let mut t = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: the struct is initialized and outlives the call.
    unsafe {
        let _ = TrackMouseEvent(&mut t);
    }
}

/// Show the hand cursor (for clickable parts of a control).
pub fn hand_cursor() {
    // SAFETY: shared system cursor, no cleanup.
    unsafe {
        if let Ok(c) = LoadCursorW(None, IDC_HAND) {
            SetCursor(Some(c));
        }
    }
}

/// Cursor position in `hwnd`'s client coordinates.
pub fn cursor_in(hwnd: HWND) -> POINT {
    let mut p = POINT::default();
    // SAFETY: writable point; valid window.
    unsafe {
        let _ = GetCursorPos(&mut p);
        let _ = ScreenToClient(hwnd, &mut p);
    }
    p
}

pub use windows::Win32::Foundation::POINT;

/// A popup menu at screen point (`x`, `y`). Items are `(id, text)`; id 0 is a
/// separator. Returns the chosen id, or 0 if dismissed.
pub fn popup_menu(owner: HWND, items: &[(usize, &str)], x: i32, y: i32) -> usize {
    // SAFETY: the menu is destroyed on every path; strings outlive the calls.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        for &(id, text) in items {
            let _ = if id == 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())
            } else {
                AppendMenuW(menu, MF_STRING, id, &HSTRING::from(text))
            };
        }
        let r = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            x,
            y,
            None,
            owner,
            None,
        );
        let _ = DestroyMenu(menu);
        r.0.max(0) as usize
    }
}

/// Window to screen coordinates.
pub fn to_screen(hwnd: HWND, mut p: POINT) -> POINT {
    // SAFETY: valid window, writable point.
    unsafe {
        let _ = ClientToScreen(hwnd, &mut p);
    }
    p
}

/// Painting: `f` draws into the window's DC between BeginPaint/EndPaint.
pub fn paint(hwnd: HWND, f: impl FnOnce(HDC)) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: paired BeginPaint/EndPaint on a valid window.
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        f(hdc);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Fill `r` with a solid colour.
pub fn fill(hdc: HDC, r: RECT, color: COLORREF) {
    // SAFETY: the brush is created and deleted here.
    unsafe {
        let b = CreateSolidBrush(color);
        FillRect(hdc, &r, b);
        let _ = DeleteObject(b.into());
    }
}

/// A rounded rectangle (`radius` in pixels) with an optional 1-px border.
pub fn round_rect(hdc: HDC, r: RECT, radius: i32, fill: COLORREF, border: Option<COLORREF>) {
    // SAFETY: GDI objects are created, selected, restored and deleted here.
    unsafe {
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(PS_SOLID, 1, border.unwrap_or(fill));
        let ob = SelectObject(hdc, brush.into());
        let op = SelectObject(hdc, pen.into());
        let _ = RoundRect(
            hdc,
            r.left,
            r.top,
            r.right,
            r.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(pen.into());
    }
}

/// A filled circle centred on (`x`, `y`).
pub fn dot(hdc: HDC, x: i32, y: i32, radius: i32, color: COLORREF) {
    round_rect(
        hdc,
        RECT {
            left: x - radius,
            top: y - radius,
            right: x + radius + 1,
            bottom: y + radius + 1,
        },
        radius + 1,
        color,
        None,
    );
}

/// How [`text`] places a string in its rect.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// One line of text, vertically centred in `r`, ellipsized if too long.
pub fn text(hdc: HDC, s: &str, r: RECT, font: HFONT, color: COLORREF, align: Align) {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut r = r;
    let h = match align {
        Align::Left => DT_LEFT,
        Align::Center => DT_CENTER,
        Align::Right => DT_RIGHT,
    };
    // SAFETY: the font is restored before returning; the buffer and rect
    // outlive the call.
    unsafe {
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, color);
        DrawTextW(
            hdc,
            &mut wide,
            &mut r,
            h | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    }
}

pub use windows::Win32::UI::Controls::DRAWITEMSTRUCT;

/// Owner-drawn item state.
pub fn item_selected(d: &DRAWITEMSTRUCT) -> bool {
    d.itemState.0 & windows::Win32::UI::Controls::ODS_SELECTED.0 != 0
}

/// Look of an owner-drawn (`BS_OWNERDRAW`) push button.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Filled with the accent colour: the main action.
    Primary,
    /// Outlined.
    Secondary,
    /// Outlined in red: ends something.
    Danger,
}

/// Draw an owner-drawn push button on a `background`-coloured surface.
pub fn draw_button(d: &DRAWITEMSTRUCT, kind: ButtonKind, font: HFONT, background: COLORREF) {
    use windows::Win32::UI::Controls::{ODS_DISABLED, ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED};
    let state = d.itemState.0;
    let disabled = state & ODS_DISABLED.0 != 0;
    let pressed = state & ODS_SELECTED.0 != 0;
    let (fill_c, border, text_c) = match (kind, disabled) {
        (_, true) => (
            rgb(0xF1, 0xF3, 0xF5),
            rgb(0xE1, 0xE4, 0xE8),
            rgb(0xA0, 0xA7, 0xB0),
        ),
        (ButtonKind::Primary, _) if pressed => {
            (rgb(0x17, 0x52, 0x96), rgb(0x17, 0x52, 0x96), WHITE)
        }
        (ButtonKind::Primary, _) => (ACCENT, ACCENT, WHITE),
        (ButtonKind::Secondary, _) => (
            if pressed {
                rgb(0xE6, 0xEA, 0xEF)
            } else {
                WHITE
            },
            rgb(0xC9, 0xCF, 0xD6),
            rgb(0x24, 0x29, 0x2F),
        ),
        (ButtonKind::Danger, _) => (
            if pressed {
                rgb(0xFB, 0xE4, 0xE4)
            } else {
                WHITE
            },
            rgb(0xE5, 0x9A, 0x9A),
            rgb(0xC0, 0x2B, 0x2B),
        ),
    };
    let r = d.rcItem;
    fill(d.hDC, r, background);
    round_rect(d.hDC, r, scale(d.hwndItem, 6), fill_c, Some(border));
    text(d.hDC, &get_text(d.hwndItem), r, font, text_c, Align::Center);
    if state & ODS_FOCUS.0 != 0 && state & ODS_NOFOCUSRECT.0 == 0 {
        let inset = scale(d.hwndItem, 3);
        let f = RECT {
            left: r.left + inset,
            top: r.top + inset,
            right: r.right - inset,
            bottom: r.bottom - inset,
        };
        // SAFETY: the HDC comes from WM_DRAWITEM.
        unsafe {
            let _ = DrawFocusRect(d.hDC, &f);
        }
    }
}

const WHITE: COLORREF = rgb(0xFF, 0xFF, 0xFF);

pub fn set_timer(hwnd: HWND, id: usize, ms: u32) {
    // SAFETY: valid window; WM_TIMER is delivered to its proc.
    unsafe {
        SetTimer(Some(hwnd), id, ms, None);
    }
}

/// Put `text` on the clipboard as Unicode text.
pub fn copy_text(owner: HWND, text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the clipboard is opened and closed on every path; the global
    // block is sized for `wide` and ownership passes to the clipboard on
    // success.
    unsafe {
        OpenClipboard(Some(owner))?;
        let r = (|| -> Result<()> {
            EmptyClipboard()?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
            let p = GlobalLock(mem) as *mut u16;
            if p.is_null() {
                let _ = GlobalFree(Some(mem));
                anyhow::bail!("GlobalLock failed");
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            let _ = GlobalUnlock(mem);
            // The clipboard owns the block only if this succeeds.
            if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))) {
                let _ = GlobalFree(Some(mem));
                return Err(e.into());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        r
    }
}

/// Modal message box owned by `owner`.
pub fn message(owner: Option<HWND>, title: &str, text: &str, error: bool) {
    let icon = if error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: strings outlive the call.
    unsafe {
        MessageBoxW(
            owner,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OK | icon,
        );
    }
}

/// Dispatch messages (with dialog-style Tab/Enter navigation for `dialog`)
/// until WM_QUIT.
pub fn message_loop(dialog: HWND) {
    // SAFETY: standard message loop on the thread that owns the windows.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if IsDialogMessageW(dialog, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Stop [`message_loop`] on this thread.
pub fn quit() {
    // SAFETY: posts WM_QUIT to the calling thread's queue.
    unsafe { PostQuitMessage(0) }
}

pub fn default_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: forwarding the message unchanged.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Post a message to a window from any thread.
pub fn post(hwnd: HWND, msg: u32) {
    // SAFETY: PostMessage is thread-safe; a stale handle just fails.
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
    }
}

pub const WM_APP: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// True when this process is the only one attached to its console: it was
/// started by double-click (Windows made a console just for it), not from a
/// terminal.
pub fn owns_console() -> bool {
    let mut list = [0u32; 4];
    // SAFETY: the buffer is writable for its length.
    unsafe { windows::Win32::System::Console::GetConsoleProcessList(&mut list) == 1 }
}

/// Close the console window Windows opened for a double-clicked program.
pub fn hide_console() {
    // SAFETY: detaches from the console; stdout writes afterwards fail
    // harmlessly, so log to a file first.
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }
}

/// Logging for a windowed app: a double-clicked program logs to
/// `<dir>/<name>.log` and drops its console window; from a terminal it logs
/// to the terminal as usual.
pub fn init_logging(gui: bool, dir: Option<std::path::PathBuf>, name: &str) {
    let filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if gui && owns_console() {
        if let Some(file) = dir.and_then(|d| {
            std::fs::create_dir_all(&d).ok()?;
            std::fs::File::create(d.join(format!("{name}.log"))).ok()
        }) {
            hide_console();
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(LogWriter::spawn(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter()).init();
}

/// Log lines go to a writer thread through a bounded queue, so a slow disk
/// (or an antivirus scan of the file) never stalls the thread that logs;
/// when the queue is full, lines are dropped.
#[derive(Clone)]
struct LogWriter(std::sync::mpsc::SyncSender<Vec<u8>>);

impl LogWriter {
    fn spawn(mut file: std::fs::File) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
        let _ = std::thread::Builder::new()
            .name("log".into())
            .spawn(move || {
                use std::io::Write;
                for line in rx {
                    let _ = file.write_all(&line);
                }
            });
        LogWriter(tx)
    }
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.try_send(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
