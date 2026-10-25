//! Syncra: one app for both sides, like Parsec. The window shows this
//! PC's ID (shared through the signal server as soon as the app starts; you
//! approve every device), a box to connect to someone else's ID and the
//! PCs you connected to before (star one to keep it).
//!
//! Sharing runs the host pipeline on a background thread; a connection
//! opens the stream window on its own thread. The two are independent:
//! you can share and watch another PC at the same time.

mod recent;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use net::config::Settings;
use net::Socket;
use recent::Recent;
use ui::style::*;
use ui::{rgb, Align, ButtonKind, COLORREF, HFONT, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::core::w;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::WindowsAndMessaging::{
    LBN_DBLCLK, LBN_SELCHANGE, LB_ADDSTRING, LB_GETCOUNT, LB_GETCURSEL, LB_GETITEMRECT,
    LB_ITEMFROMPOINT, LB_RESETCONTENT, LB_SETCURSEL, LB_SETITEMHEIGHT, WM_CONTEXTMENU, WM_DRAWITEM,
    WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN,
    WM_SETCURSOR,
};

/// IDOK, so Enter in the ID box connects (the button is owner-drawn and
/// can't be the default push button).
const ID_CONNECT: usize = 1;
const ID_COPY: usize = 2;
const ID_KICK: usize = 3;
const ID_LIST: usize = 4;
const MENU_CONNECT: usize = 100;
const MENU_STAR: usize = 101;
const MENU_COPY: usize = 102;
const MENU_REMOVE: usize = 103;
const TIMER: usize = 1;
/// A connect attempt finished (result in `CONNECT_RESULT`).
const WM_CONNECTED: u32 = ui::WM_APP + 10;
/// A stream window was closed.
const WM_SESSION_END: u32 = ui::WM_APP + 11;

// Layout, 96-DPI units.
const WIDTH: i32 = 720;
const HEIGHT: i32 = 488;
const HEADER: i32 = 64;
const LEFT_CARD: [i32; 4] = [16, 80, 356, 472];
const RIGHT_CARD: [i32; 4] = [368, 80, 704, 472];
/// The status dot left of the sharing state.
const DOT: (i32, i32, i32) = (44, 269, 5);
const ROW_HEIGHT: i32 = 44;
/// Star column and remove button width in a recent row.
const ROW_STAR: i32 = 40;
const ROW_REMOVE: i32 = 36;

const BACKGROUND: COLORREF = rgb(0xF3, 0xF5, 0xF8);
const HEADER_BG: COLORREF = rgb(0x13, 0x2F, 0x52);
const HEADER_SUB: COLORREF = rgb(0xA9, 0xBE, 0xD8);
const CARD: COLORREF = rgb(0xFF, 0xFF, 0xFF);
const CARD_BORDER: COLORREF = rgb(0xE1, 0xE5, 0xEA);
const SECTION: COLORREF = rgb(0x5B, 0x66, 0x75);
const TEXT: COLORREF = rgb(0x1F, 0x24, 0x2B);
const WHITE: COLORREF = rgb(0xFF, 0xFF, 0xFF);
const STAR_ON: COLORREF = rgb(0xE8, 0xA3, 0x17);
const STAR_OFF: COLORREF = rgb(0xA8, 0xB0, 0xBA);
const ICON_HOT: COLORREF = rgb(0x4B, 0x55, 0x63);
const ROW_HOVER: COLORREF = rgb(0xF3, 0xF6, 0xFA);
const ROW_SELECTED: COLORREF = rgb(0xE5, 0xEE, 0xF9);
const GREEN: COLORREF = rgb(0x2E, 0xA0, 0x43);
const AMBER: COLORREF = rgb(0xD2, 0x99, 0x22);
const RED: COLORREF = rgb(0xCF, 0x22, 0x2E);
const GRAY: COLORREF = rgb(0xA0, 0xA7, 0xB0);

static CTL_ID: AtomicIsize = AtomicIsize::new(0);
static CTL_STATE: AtomicIsize = AtomicIsize::new(0);
static CTL_DETAIL: AtomicIsize = AtomicIsize::new(0);
static CTL_PEER: AtomicIsize = AtomicIsize::new(0);
static CTL_SERVER: AtomicIsize = AtomicIsize::new(0);
static CTL_CONNECT: AtomicIsize = AtomicIsize::new(0);
static CTL_CSTATUS: AtomicIsize = AtomicIsize::new(0);
static CTL_KICK: AtomicIsize = AtomicIsize::new(0);
static CTL_NOTE: AtomicIsize = AtomicIsize::new(0);
static CTL_LIST: AtomicIsize = AtomicIsize::new(0);
static CTL_EMPTY: AtomicIsize = AtomicIsize::new(0);
/// The list box's own window procedure (it is subclassed).
static LIST_PROC: AtomicIsize = AtomicIsize::new(0);
/// Row and part under the mouse in the recent list (-1: none).
static HOVER: AtomicIsize = AtomicIsize::new(-1);
static HOVER_PART: AtomicU32 = AtomicU32::new(Part::None as u32);
/// Colour of the status dot, to repaint it only when it changes.
static DOT_COLOR: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);
/// Connecting or watching: one outgoing session at a time.
static BUSY: AtomicBool = AtomicBool::new(false);
/// A stream window is open (the Connect button reads "Disconnect").
static WATCHING: AtomicBool = AtomicBool::new(false);
/// Socket, host address, ID and signal server of a finished connect.
type Connected = (Socket, SocketAddr, u32, String);
static CONNECT_RESULT: Mutex<Option<Result<Connected>>> = Mutex::new(None);
static RECENT: Mutex<Recent> = Mutex::new(Recent::new());
static FONTS: OnceLock<Fonts> = OnceLock::new();

/// GDI fonts, stored as handles so they can live in a static.
struct Fonts {
    small: isize,
    button: isize,
    section: isize,
    title: isize,
    row: isize,
    icon: isize,
    icon_small: isize,
}

fn fonts() -> &'static Fonts {
    FONTS
        .get()
        .expect("fonts are created before the window shows")
}

fn f(h: isize) -> HFONT {
    HFONT(h as *mut _)
}

fn ctl(a: &AtomicIsize) -> HWND {
    HWND(a.load(Ordering::Acquire) as *mut _)
}

/// A rect in 96-DPI units, scaled for `hwnd`.
fn rect(hwnd: HWND, [l, t, r, b]: [i32; 4]) -> RECT {
    let s = |v| ui::scale(hwnd, v);
    RECT {
        left: s(l),
        top: s(t),
        right: s(r),
        bottom: s(b),
    }
}

fn main() -> Result<()> {
    ui::init_logging(true, net::crypto::data_dir().ok(), "syncra");
    let r = run();
    if let Err(e) = &r {
        ui::message(None, "Syncra", &format!("{e:#}"), true);
    }
    r
}

fn run() -> Result<()> {
    // DPI mode, timer resolution, priority: before the first window.
    host::rt::init_process();
    let settings = Settings::load();
    *RECENT.lock().unwrap() = Recent::load();

    let class = w!("SyncraApp");
    ui::register_class_color(class, Some(proc), BACKGROUND)?;
    let hwnd = ui::create_window(class, "Syncra", WIDTH, HEIGHT)?;
    ui::caption_color(hwnd, HEADER_BG, WHITE);
    let small = ui::font(hwnd, 9, false);
    let normal = ui::font(hwnd, 10, false);
    let button = ui::font(hwnd, 10, true);
    let big = ui::font(hwnd, 26, true);
    let entry = ui::font(hwnd, 16, true);
    let _ = FONTS.set(Fonts {
        small: small.0 as isize,
        button: button.0 as isize,
        section: ui::font(hwnd, 9, true).0 as isize,
        title: ui::font(hwnd, 16, true).0 as isize,
        row: ui::font(hwnd, 11, true).0 as isize,
        icon: ui::icon_font(hwnd, 12).0 as isize,
        icon_small: ui::icon_font(hwnd, 8).0 as isize,
    });

    // --- This PC: share it. Headings, the dot and the divider are painted.
    let id = ui::child(hwnd, w!("STATIC"), "…", SS_CENTER, 24, 140, 324, 50, 0, big)?;
    ui::child(
        hwnd,
        w!("BUTTON"),
        "Copy ID",
        BS_OWNERDRAW | WS_TABSTOP,
        60,
        204,
        120,
        34,
        ID_COPY,
        button,
    )?;
    // Ends the session of whoever is watching this PC.
    let kick = ui::child(
        hwnd,
        w!("BUTTON"),
        "Disconnect",
        BS_OWNERDRAW | WS_TABSTOP,
        192,
        204,
        120,
        34,
        ID_KICK,
        button,
    )?;
    ui::enable(kick, false);
    let state = ui::child(
        hwnd,
        w!("STATIC"),
        "Starting…",
        SS_LEFT,
        58,
        259,
        278,
        40,
        0,
        normal,
    )?;
    let detail = ui::child(hwnd, w!("STATIC"), "", SS_LEFT, 58, 300, 278, 36, 0, small)?;
    let server = ui::child(
        hwnd,
        w!("EDIT"),
        &settings.signal,
        ES_AUTOHSCROLL | WS_TABSTOP,
        36,
        384,
        300,
        26,
        0,
        normal,
    )?;
    // Kept as a control so connect/recent code reads one place, but not
    // shown: the server is plumbing (settings.txt overrides it).
    ui::show_control(server, false);
    let note = ui::child(
        hwnd,
        w!("STATIC"),
        "Anyone with your ID can ask to connect; nothing streams until you approve.",
        SS_LEFT,
        36,
        366,
        300,
        40,
        0,
        small,
    )?;

    // --- Connect to a PC.
    let peer = ui::child(
        hwnd,
        w!("EDIT"),
        "",
        ES_CENTER | ES_AUTOHSCROLL | WS_TABSTOP,
        388,
        122,
        180,
        42,
        0,
        entry,
    )?;
    let connect = ui::child(
        hwnd,
        w!("BUTTON"),
        "Connect",
        BS_OWNERDRAW | WS_TABSTOP,
        578,
        122,
        106,
        42,
        ID_CONNECT,
        button,
    )?;
    let cstatus = ui::child(
        hwnd,
        w!("STATIC"),
        "Enter the ID shown on the other PC.",
        SS_LEFT,
        388,
        172,
        296,
        34,
        0,
        small,
    )?;
    let list = ui::child(
        hwnd,
        w!("LISTBOX"),
        "",
        LBS_NOTIFY | LBS_OWNERDRAWFIXED | LBS_NOINTEGRALHEIGHT | WS_VSCROLL | WS_TABSTOP,
        380,
        240,
        312,
        222,
        ID_LIST,
        normal,
    )?;
    ui::send(
        list,
        LB_SETITEMHEIGHT,
        0,
        ui::scale(hwnd, ROW_HEIGHT) as isize,
    );
    LIST_PROC.store(ui::subclass(list, Some(list_proc)), Ordering::Release);
    let empty = ui::child(
        hwnd,
        w!("STATIC"),
        "No recent connections yet.\nPCs you connect to show up here.",
        SS_CENTER,
        388,
        300,
        296,
        40,
        0,
        small,
    )?;

    for (a, h) in [
        (&CTL_ID, id),
        (&CTL_STATE, state),
        (&CTL_DETAIL, detail),
        (&CTL_PEER, peer),
        (&CTL_SERVER, server),
        (&CTL_CONNECT, connect),
        (&CTL_CSTATUS, cstatus),
        (&CTL_KICK, kick),
        (&CTL_NOTE, note),
        (&CTL_LIST, list),
        (&CTL_EMPTY, empty),
    ] {
        a.store(h.0 as isize, Ordering::Release);
    }
    rebuild_list(None);
    host::panel::set_owner(hwnd);

    // Share this PC from the start, through the saved server.
    let signal = settings.signal.clone();
    let sharing = std::thread::Builder::new()
        .name("share".into())
        .spawn(move || {
            if let Err(e) = host::share(signal) {
                tracing::error!("sharing stopped: {e:#}");
                host::panel::STATUS.set_state(format!("Sharing stopped: {e}"));
            }
        })?;

    ui::set_timer(hwnd, TIMER, 500);
    ui::show(hwnd);
    ui::set_focus(peer);
    ui::message_loop(hwnd);

    // Window closed: stop sharing and give the host a moment to tear down.
    host::panel::request_close();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !sharing.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::process::exit(0);
}

fn start_connect(hwnd: HWND) {
    if WATCHING.load(Ordering::Acquire) {
        // The button reads "Disconnect": close the stream window; the
        // session thread then posts WM_SESSION_END.
        ui::enable(ctl(&CTL_CONNECT), false);
        client::disconnect();
        return;
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(id) = net::crypto::parse_id(&ui::get_text(ctl(&CTL_PEER))) else {
        ui::set_text(ctl(&CTL_CSTATUS), "The ID is 9 digits, like 482 913 077.");
        BUSY.store(false, Ordering::Release);
        return;
    };
    let server = ui::get_text(ctl(&CTL_SERVER)).trim().to_string();
    if server.is_empty() {
        ui::set_text(
            ctl(&CTL_CSTATUS),
            "No signal server set: add signal=HOST:PORT to settings.txt.",
        );
        BUSY.store(false, Ordering::Release);
        return;
    }
    set_connect_enabled(false);
    ui::set_text(
        ctl(&CTL_CSTATUS),
        "Connecting… the other PC has to approve this device.",
    );
    let owner = hwnd.0 as isize;
    std::thread::spawn(move || {
        let r = (|| -> Result<Connected> {
            let addr = net::addr::resolve_v4(&server)?;
            let socket = Socket::bind("0.0.0.0:0".parse().unwrap())?;
            socket.set_nonblocking(true)?;
            let host = net::connect::connect(&socket, addr, id, false).context("connect")?;
            Settings {
                signal: server.clone(),
            }
            .save();
            Ok((socket, host, id, server))
        })();
        *CONNECT_RESULT.lock().unwrap() = Some(r);
        ui::post(HWND(owner as *mut _), WM_CONNECTED);
    });
}

fn set_connect_enabled(on: bool) {
    for c in [&CTL_PEER, &CTL_SERVER, &CTL_CONNECT] {
        ui::enable(ctl(c), on);
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// Set a label only when it changed (every set repaints it).
fn update_text(hwnd: HWND, text: &str) {
    if ui::get_text(hwnd) != text {
        ui::set_text(hwnd, text);
    }
}

/// Status dot colour for the sharing state line.
fn state_color(state: &str) -> COLORREF {
    if state.starts_with("Ready") {
        GREEN
    } else if state.starts_with("Connected") {
        ui::ACCENT
    } else if state.contains("wants to connect") {
        AMBER
    } else if state.starts_with("Can't") || state.contains("stopped") {
        RED
    } else {
        GRAY
    }
}

// --- Recent connections list.

/// Part of a recent row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    None = 0,
    Body = 1,
    Star = 2,
    Remove = 3,
}

/// Reload the list box from `RECENT`, selecting `select` if it's there.
fn rebuild_list(select: Option<u32>) {
    let list = ctl(&CTL_LIST);
    let (n, sel) = {
        let r = RECENT.lock().unwrap();
        (r.len(), select.and_then(|id| r.position(id)))
    };
    ui::send(list, LB_RESETCONTENT, 0, 0);
    for _ in 0..n {
        ui::send(list, LB_ADDSTRING, 0, 0);
    }
    if let Some(i) = sel {
        ui::send(list, LB_SETCURSEL, i, 0);
    }
    ui::show_control(list, n > 0);
    ui::show_control(ctl(&CTL_EMPTY), n == 0);
    let p = ui::cursor_in(list);
    set_hover(list, hit(list, p.x, p.y));
}

/// Row and part at client point (`x`, `y`) of the list.
fn hit(list: HWND, x: i32, y: i32) -> (isize, Part) {
    let r = ui::client_rect(list);
    if x < 0 || y < 0 || x >= r.right || y >= r.bottom {
        return (-1, Part::None);
    }
    let lp = (x as u16 as u32 | (y as u16 as u32) << 16) as isize;
    let v = ui::send(list, LB_ITEMFROMPOINT, 0, lp);
    let row = v & 0xffff;
    if v >> 16 & 1 != 0 || row >= ui::send(list, LB_GETCOUNT, 0, 0) {
        return (-1, Part::None);
    }
    let part = if x < ui::scale(list, ROW_STAR) {
        Part::Star
    } else if x >= r.right - ui::scale(list, ROW_REMOVE) {
        Part::Remove
    } else {
        Part::Body
    };
    (row, part)
}

fn item_rect(list: HWND, row: isize) -> Option<RECT> {
    let mut r = RECT::default();
    (row >= 0
        && ui::send(
            list,
            LB_GETITEMRECT,
            row as usize,
            &mut r as *mut RECT as isize,
        ) != -1)
        .then_some(r)
}

fn set_hover(list: HWND, (row, part): (isize, Part)) {
    let old = HOVER.swap(row, Ordering::AcqRel);
    let old_part = HOVER_PART.swap(part as u32, Ordering::AcqRel);
    if old == row && old_part == part as u32 {
        return;
    }
    for r in [old, row] {
        if let Some(rc) = item_rect(list, r) {
            ui::redraw(list, Some(rc));
        }
    }
}

fn selected() -> Option<usize> {
    usize::try_from(ui::send(ctl(&CTL_LIST), LB_GETCURSEL, 0, 0)).ok()
}

/// Put a recent entry's ID and server in the connect fields.
fn pick(i: usize) {
    if BUSY.load(Ordering::Acquire) {
        return;
    }
    let Some(e) = RECENT.lock().unwrap().get(i).cloned() else {
        return;
    };
    ui::set_text(ctl(&CTL_PEER), &net::crypto::format_id(e.id));
    if !e.server.is_empty() {
        ui::set_text(ctl(&CTL_SERVER), &e.server);
    }
    ui::set_text(
        ctl(&CTL_CSTATUS),
        "Press Connect (or double-click the row).",
    );
}

fn toggle_star(i: usize) {
    let id = {
        let mut r = RECENT.lock().unwrap();
        let Some(n) = r.toggle_star(i) else { return };
        r.save();
        r.get(n).map(|e| e.id)
    };
    rebuild_list(id);
}

fn remove(i: usize) {
    {
        let mut r = RECENT.lock().unwrap();
        r.remove(i);
        r.save();
    }
    rebuild_list(None);
}

fn list_menu(hwnd: HWND, i: usize, x: i32, y: i32) {
    let Some(e) = RECENT.lock().unwrap().get(i).cloned() else {
        return;
    };
    let busy = BUSY.load(Ordering::Acquire);
    let mut items = Vec::new();
    if !busy {
        items.push((MENU_CONNECT, "Connect"));
    }
    items.push((MENU_STAR, if e.starred { "Unstar" } else { "Star" }));
    items.push((MENU_COPY, "Copy ID"));
    items.push((0, ""));
    items.push((MENU_REMOVE, "Remove from list"));
    match ui::popup_menu(hwnd, &items, x, y) {
        MENU_CONNECT => {
            pick(i);
            start_connect(hwnd);
        }
        MENU_STAR => toggle_star(i),
        MENU_COPY => {
            let _ = ui::copy_text(hwnd, &net::crypto::format_id(e.id));
        }
        MENU_REMOVE => remove(i),
        _ => {}
    }
}

fn draw_row(d: &ui::DRAWITEMSTRUCT) {
    let Ok(i) = usize::try_from(d.itemID as i32) else {
        return;
    };
    let Some(e) = RECENT.lock().unwrap().get(i).cloned() else {
        return;
    };
    let fo = fonts();
    let s = |v| ui::scale(d.hwndItem, v);
    let r = d.rcItem;
    let hover = HOVER.load(Ordering::Acquire) == i as isize;
    let part = HOVER_PART.load(Ordering::Acquire);
    let selected = ui::item_selected(d);
    ui::fill(d.hDC, r, CARD);
    if selected || hover {
        let bg = RECT {
            left: r.left + s(2),
            top: r.top + s(2),
            right: r.right - s(2),
            bottom: r.bottom - s(2),
        };
        let c = if selected { ROW_SELECTED } else { ROW_HOVER };
        ui::round_rect(d.hDC, bg, s(6), c, None);
    }
    let star_color = if e.starred {
        STAR_ON
    } else if hover && part == Part::Star as u32 {
        ICON_HOT
    } else {
        STAR_OFF
    };
    ui::text(
        d.hDC,
        if e.starred { "\u{E735}" } else { "\u{E734}" },
        RECT {
            right: r.left + s(ROW_STAR),
            ..r
        },
        f(fo.icon),
        star_color,
        Align::Center,
    );
    let text_right = r.right - s(ROW_REMOVE);
    ui::text(
        d.hDC,
        &net::crypto::format_id(e.id),
        RECT {
            left: r.left + s(ROW_STAR + 4),
            top: r.top + s(4),
            right: text_right,
            bottom: r.top + s(24),
        },
        f(fo.row),
        TEXT,
        Align::Left,
    );
    let mut sub = recent::ago(recent::now(), e.last);
    if e.starred {
        sub = format!("Starred · {sub}");
    }
    ui::text(
        d.hDC,
        &sub,
        RECT {
            left: r.left + s(ROW_STAR + 4),
            top: r.top + s(23),
            right: text_right,
            bottom: r.top + s(41),
        },
        f(fo.small),
        ui::MUTED,
        Align::Left,
    );
    if hover || selected {
        let c = if hover && part == Part::Remove as u32 {
            RED
        } else {
            STAR_OFF
        };
        ui::text(
            d.hDC,
            "\u{E711}",
            RECT {
                left: text_right,
                right: r.right - s(4),
                ..r
            },
            f(fo.icon_small),
            c,
            Align::Center,
        );
    }
}

fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xffff) as i16 as i32,
        (lparam.0 >> 16 & 0xffff) as i16 as i32,
    )
}

/// The list box with hover, a clickable star and a remove button per row.
extern "system" fn list_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let old = LIST_PROC.load(Ordering::Acquire);
    match msg {
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lparam);
            if HOVER.load(Ordering::Acquire) < 0 {
                ui::track_leave(hwnd);
            }
            set_hover(hwnd, hit(hwnd, x, y));
        }
        WM_MOUSELEAVE => set_hover(hwnd, (-1, Part::None)),
        WM_SETCURSOR => {
            let p = ui::cursor_in(hwnd);
            if matches!(hit(hwnd, p.x, p.y).1, Part::Star | Part::Remove) {
                ui::hand_cursor();
                return LRESULT(1);
            }
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (x, y) = mouse_xy(lparam);
            match hit(hwnd, x, y) {
                (row, Part::Star) => {
                    toggle_star(row as usize);
                    return LRESULT(0);
                }
                (row, Part::Remove) => {
                    // A double click's second press lands on the next row.
                    if msg == WM_LBUTTONDOWN {
                        remove(row as usize);
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        WM_RBUTTONDOWN => {
            let (x, y) = mouse_xy(lparam);
            if let (row @ 0.., _) = hit(hwnd, x, y) {
                ui::send(hwnd, LB_SETCURSEL, row as usize, 0);
                pick(row as usize);
            }
        }
        WM_KEYDOWN if wparam.0 == 0x2E => {
            // Delete.
            if let Some(i) = selected() {
                remove(i);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    ui::call_proc(old, hwnd, msg, wparam, lparam)
}

// --- Main window.

fn paint(hwnd: HWND, hdc: ui::HDC) {
    let fo = fonts();
    let s = |v| ui::scale(hwnd, v);
    let client = ui::client_rect(hwnd);
    ui::fill(
        hdc,
        RECT {
            bottom: s(HEADER),
            ..client
        },
        HEADER_BG,
    );
    ui::text(
        hdc,
        "Syncra",
        rect(hwnd, [24, 8, 400, 38]),
        f(fo.title),
        WHITE,
        Align::Left,
    );
    ui::text(
        hdc,
        "Low-latency remote desktop and game streaming",
        rect(hwnd, [24, 36, 500, 56]),
        f(fo.small),
        HEADER_SUB,
        Align::Left,
    );
    for c in [LEFT_CARD, RIGHT_CARD] {
        ui::round_rect(hdc, rect(hwnd, c), s(10), CARD, Some(CARD_BORDER));
    }
    let heading = |t: &str, r: [i32; 4], color: COLORREF, font: isize, align: Align| {
        ui::text(hdc, t, rect(hwnd, r), f(font), color, align)
    };
    heading(
        "THIS PC",
        [36, 96, 336, 114],
        SECTION,
        fo.section,
        Align::Left,
    );
    heading(
        "Your ID",
        [36, 120, 336, 138],
        ui::MUTED,
        fo.small,
        Align::Center,
    );
    heading(
        "CONNECT TO A PC",
        [388, 96, 684, 114],
        SECTION,
        fo.section,
        Align::Left,
    );
    heading(
        "RECENT",
        [388, 214, 684, 232],
        SECTION,
        fo.section,
        Align::Left,
    );
    heading(
        "\u{2605} keeps a PC on top",
        [388, 214, 684, 232],
        ui::MUTED,
        fo.small,
        Align::Right,
    );
    let (x, y, r) = DOT;
    ui::dot(
        hdc,
        s(x),
        s(y),
        s(r),
        COLORREF(DOT_COLOR.load(Ordering::Acquire)),
    );
}

extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            ui::paint(hwnd, |hdc| paint(hwnd, hdc));
            LRESULT(0)
        }
        ui::WM_TIMER => {
            let (id, state, detail) = host::panel::STATUS.read();
            if !id.is_empty() {
                update_text(ctl(&CTL_ID), &id);
            }
            update_text(ctl(&CTL_STATE), &state);
            update_text(ctl(&CTL_DETAIL), &detail);
            ui::enable(ctl(&CTL_KICK), host::panel::has_client());
            let c = state_color(&state).0;
            if DOT_COLOR.swap(c, Ordering::AcqRel) != c {
                let (x, y, r) = DOT;
                ui::redraw(hwnd, Some(rect(hwnd, [x - r, y - r, x + r + 1, y + r + 1])));
            }
            // Refresh "5 min ago" now and then.
            if TICKS.fetch_add(1, Ordering::AcqRel) % 60 == 59 {
                ui::redraw(ctl(&CTL_LIST), None);
            }
            LRESULT(0)
        }
        ui::WM_COMMAND => {
            let code = (wparam.0 >> 16) as u32;
            match wparam.0 & 0xffff {
                ID_COPY => {
                    let id = ui::get_text(ctl(&CTL_ID));
                    if ui::copy_text(hwnd, &id).is_ok() {
                        host::panel::STATUS.set_detail("ID copied to the clipboard");
                    }
                }
                ID_CONNECT => start_connect(hwnd),
                ID_KICK => host::panel::request_disconnect(),
                ID_LIST if code == LBN_SELCHANGE => {
                    if let Some(i) = selected() {
                        pick(i);
                    }
                }
                ID_LIST if code == LBN_DBLCLK => {
                    if let Some(i) = selected() {
                        pick(i);
                        start_connect(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            // SAFETY: WM_DRAWITEM's lparam points to a DRAWITEMSTRUCT for
            // the duration of the message.
            let d = unsafe { &*(lparam.0 as *const ui::DRAWITEMSTRUCT) };
            let fo = fonts();
            match d.CtlID as usize {
                ID_LIST => draw_row(d),
                ID_CONNECT => {
                    let kind = if WATCHING.load(Ordering::Acquire) {
                        ButtonKind::Danger
                    } else {
                        ButtonKind::Primary
                    };
                    ui::draw_button(d, kind, f(fo.button), CARD);
                }
                ID_KICK => ui::draw_button(d, ButtonKind::Danger, f(fo.button), CARD),
                _ => ui::draw_button(d, ButtonKind::Secondary, f(fo.button), CARD),
            }
            LRESULT(1)
        }
        WM_CONTEXTMENU if wparam.0 as isize == CTL_LIST.load(Ordering::Acquire) => {
            let list = ctl(&CTL_LIST);
            if let Some(i) = selected() {
                let (mut x, mut y) = mouse_xy(lparam);
                if lparam.0 as i32 == -1 {
                    // From the keyboard: under the selected row.
                    let r = item_rect(list, i as isize).unwrap_or_default();
                    let p = ui::to_screen(
                        list,
                        ui::POINT {
                            x: r.left + ui::scale(list, ROW_STAR),
                            y: r.bottom,
                        },
                    );
                    (x, y) = (p.x, p.y);
                }
                list_menu(hwnd, i, x, y);
            }
            LRESULT(0)
        }
        WM_CONNECTED => {
            let result = CONNECT_RESULT.lock().unwrap().take();
            match result {
                Some(Ok((socket, host, id, server))) => {
                    let label = net::crypto::format_id(id);
                    ui::set_text(ctl(&CTL_CSTATUS), &format!("Connected to {label}."));
                    {
                        let mut r = RECENT.lock().unwrap();
                        r.touch(id, &server, recent::now());
                        r.save();
                    }
                    rebuild_list(Some(id));
                    WATCHING.store(true, Ordering::Release);
                    ui::set_text(ctl(&CTL_CONNECT), "Disconnect");
                    ui::enable(ctl(&CTL_CONNECT), true);
                    let owner = hwnd.0 as isize;
                    // The stream window gets its own thread and message loop.
                    let _ = std::thread::Builder::new()
                        .name("session".into())
                        .spawn(move || {
                            if let Err(e) = client::stream_session(socket, host, &label) {
                                tracing::error!("session ended: {e:#}");
                            }
                            ui::post(HWND(owner as *mut _), WM_SESSION_END);
                        });
                }
                Some(Err(e)) => {
                    ui::set_text(ctl(&CTL_CSTATUS), &capitalized(&e.root_cause().to_string()));
                    set_connect_enabled(true);
                    BUSY.store(false, Ordering::Release);
                }
                None => {}
            }
            LRESULT(0)
        }
        WM_SESSION_END => {
            WATCHING.store(false, Ordering::Release);
            ui::set_text(ctl(&CTL_CONNECT), "Connect");
            ui::set_text(
                ctl(&CTL_CSTATUS),
                "Disconnected. Enter an ID to connect again.",
            );
            set_connect_enabled(true);
            BUSY.store(false, Ordering::Release);
            LRESULT(0)
        }
        ui::WM_CTLCOLORSTATIC => {
            let c = lparam.0;
            let is = |a: &AtomicIsize| c == a.load(Ordering::Acquire);
            let color = if is(&CTL_ID) {
                Some(ui::ACCENT)
            } else if is(&CTL_DETAIL) || is(&CTL_CSTATUS) || is(&CTL_NOTE) || is(&CTL_EMPTY) {
                Some(ui::MUTED)
            } else {
                Some(TEXT)
            };
            ui::static_color(wparam, color)
        }
        ui::WM_DESTROY => {
            ui::quit();
            LRESULT(0)
        }
        _ => ui::default_proc(hwnd, msg, wparam, lparam),
    }
}
