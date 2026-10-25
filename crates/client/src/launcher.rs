//! The client launcher (started when `client.exe` runs without arguments):
//! paste a host ID, press Connect, wait for the host to approve. The whole
//! connect flow (signal server, approval, hole punch, handshake) runs on a
//! worker thread while the window shows progress; the stream window opens
//! only once the session is up.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use net::config::Settings;
use net::Socket;
use ui::style::*;
use ui::{HWND, LPARAM, LRESULT, WPARAM};
use windows::core::w;

const ID_CONNECT: usize = 1;
/// Posted by the worker when the connect attempt finished.
const WM_CONNECTED: u32 = ui::WM_APP + 10;

static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// A connect attempt is running (Enter and the button both send ID_CONNECT).
static CONNECTING: AtomicBool = AtomicBool::new(false);
static CTL_ID: AtomicIsize = AtomicIsize::new(0);
static CTL_SERVER: AtomicIsize = AtomicIsize::new(0);
static CTL_STATUS: AtomicIsize = AtomicIsize::new(0);
static CTL_BUTTON: AtomicIsize = AtomicIsize::new(0);
static RESULT: Mutex<Option<Result<(Socket, SocketAddr, String)>>> = Mutex::new(None);
static DONE: Mutex<Option<(Socket, SocketAddr, String)>> = Mutex::new(None);

fn ctl(a: &AtomicIsize) -> HWND {
    HWND(a.load(Ordering::Acquire) as *mut _)
}

/// Show the launcher until a session is established. Returns the connected
/// socket, the address to stream with and a label for the window title, or
/// None if the user closed the launcher.
pub fn run() -> Result<Option<(Socket, SocketAddr, String)>> {
    let settings = Settings::load();
    let class = w!("SyncraLauncher");
    ui::register_class(class, Some(proc))?;
    let hwnd = ui::create_window(class, "Syncra: connect", 400, 270)?;
    let small = ui::font(hwnd, 10, false);
    let normal = ui::font(hwnd, 11, false);
    let big = ui::font(hwnd, 20, true);
    ui::child(
        hwnd,
        w!("STATIC"),
        "Host ID",
        SS_LEFT,
        30,
        20,
        340,
        20,
        0,
        small,
    )?;
    let id = ui::child(
        hwnd,
        w!("EDIT"),
        "",
        ES_CENTER | ES_AUTOHSCROLL | WS_TABSTOP,
        30,
        42,
        340,
        44,
        0,
        big,
    )?;
    ui::child(
        hwnd,
        w!("STATIC"),
        "Server",
        SS_LEFT,
        30,
        98,
        340,
        20,
        0,
        small,
    )?;
    let server = ui::child(
        hwnd,
        w!("EDIT"),
        &settings.signal,
        ES_AUTOHSCROLL | WS_TABSTOP,
        30,
        118,
        340,
        26,
        0,
        normal,
    )?;
    let button = ui::child(
        hwnd,
        w!("BUTTON"),
        "Connect",
        BS_DEFPUSHBUTTON | WS_TABSTOP,
        130,
        160,
        140,
        34,
        ID_CONNECT,
        normal,
    )?;
    let status = ui::child(
        hwnd,
        w!("STATIC"),
        "Paste the ID shown on the host.",
        SS_CENTER,
        20,
        210,
        360,
        44,
        0,
        small,
    )?;
    WINDOW.store(hwnd.0 as isize, Ordering::Release);
    CTL_ID.store(id.0 as isize, Ordering::Release);
    CTL_SERVER.store(server.0 as isize, Ordering::Release);
    CTL_STATUS.store(status.0 as isize, Ordering::Release);
    CTL_BUTTON.store(button.0 as isize, Ordering::Release);
    ui::show(hwnd);
    ui::set_focus(id);
    ui::message_loop(hwnd);
    for f in [small, normal, big] {
        // SAFETY: the window that used the fonts is destroyed.
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(f.into());
        }
    }
    Ok(DONE.lock().unwrap().take())
}

fn start_connect(hwnd: HWND) {
    if CONNECTING.swap(true, Ordering::AcqRel) {
        return;
    }
    let id_text = ui::get_text(ctl(&CTL_ID));
    let server = ui::get_text(ctl(&CTL_SERVER)).trim().to_string();
    let Some(id) = net::crypto::parse_id(&id_text) else {
        ui::set_text(ctl(&CTL_STATUS), "The ID is 9 digits, like 482 913 077.");
        CONNECTING.store(false, Ordering::Release);
        return;
    };
    if server.is_empty() {
        ui::set_text(ctl(&CTL_STATUS), "Enter the signal server (HOST:PORT).");
        CONNECTING.store(false, Ordering::Release);
        return;
    }
    for c in [&CTL_ID, &CTL_SERVER, &CTL_BUTTON] {
        ui::enable(ctl(c), false);
    }
    ui::set_text(
        ctl(&CTL_STATUS),
        "Connecting… the host has to approve this device.",
    );
    let owner = hwnd.0 as isize;
    std::thread::spawn(move || {
        let r = (|| -> Result<(Socket, SocketAddr, String)> {
            let addr = net::addr::resolve_v4(&server)?;
            let socket = Socket::bind("0.0.0.0:0".parse().unwrap())?;
            socket.set_nonblocking(true)?;
            let host = net::connect::connect(&socket, addr, id, false).context("connect")?;
            Settings { signal: server }.save();
            Ok((socket, host, net::crypto::format_id(id)))
        })();
        *RESULT.lock().unwrap() = Some(r);
        ui::post(HWND(owner as *mut _), WM_CONNECTED);
    });
}

extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        ui::WM_COMMAND if wparam.0 & 0xffff == ID_CONNECT => {
            start_connect(hwnd);
            LRESULT(0)
        }
        WM_CONNECTED => {
            CONNECTING.store(false, Ordering::Release);
            let result = RESULT.lock().unwrap().take();
            match result {
                Some(Ok(done)) => {
                    *DONE.lock().unwrap() = Some(done);
                    // SAFETY: destroying our own window on its thread.
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                    }
                }
                Some(Err(e)) => {
                    // The root cause reads best ("the host denied …").
                    let msg = e.root_cause().to_string();
                    let mut c = msg.chars();
                    let msg = c
                        .next()
                        .map_or(String::new(), |f| f.to_uppercase().chain(c).collect());
                    ui::set_text(ctl(&CTL_STATUS), &msg);
                    for c in [&CTL_ID, &CTL_SERVER, &CTL_BUTTON] {
                        ui::enable(ctl(c), true);
                    }
                    ui::set_focus(ctl(&CTL_ID));
                }
                None => {}
            }
            LRESULT(0)
        }
        ui::WM_CTLCOLORSTATIC => {
            let color = (lparam.0 == CTL_STATUS.load(Ordering::Acquire)).then_some(ui::MUTED);
            ui::static_color(wparam, color)
        }
        ui::WM_DESTROY => {
            ui::quit();
            LRESULT(0)
        }
        _ => ui::default_proc(hwnd, msg, wparam, lparam),
    }
}
