//! The host window (started when `host.exe` runs without arguments): the ID
//! to share with a Copy button, connection status and live stream stats.
//!
//! It runs on its own thread. Everything else only writes [`STATUS`]; the
//! window reads it on a 500 ms timer, so no other thread ever touches it.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use ui::style::*;
use ui::{HWND, LPARAM, LRESULT, WPARAM};
use windows::core::w;

pub struct Status {
    id: Mutex<String>,
    state: Mutex<String>,
    detail: Mutex<String>,
}

pub static STATUS: Status = Status {
    id: Mutex::new(String::new()),
    state: Mutex::new(String::new()),
    detail: Mutex::new(String::new()),
};

impl Status {
    pub fn set_id(&self, s: impl Into<String>) {
        *self.id.lock().unwrap() = s.into();
    }

    /// (id, state line, stats line) for display.
    pub fn read(&self) -> (String, String, String) {
        (
            self.id.lock().unwrap().clone(),
            self.state.lock().unwrap().clone(),
            self.detail.lock().unwrap().clone(),
        )
    }

    /// Main status line ("Waiting for a device…", "Connected: …").
    pub fn set_state(&self, s: impl Into<String>) {
        *self.state.lock().unwrap() = s.into();
    }

    /// Stats line (fps, resolution, bitrate); empty when not streaming.
    pub fn set_detail(&self, s: impl Into<String>) {
        *self.detail.lock().unwrap() = s.into();
    }
}

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static CLOSING: AtomicBool = AtomicBool::new(false);

/// The user closed the window: the encode loop stops and the host exits.
pub fn closing() -> bool {
    CLOSING.load(Ordering::Relaxed)
}

/// Stop sharing (the combined app's window closed).
pub fn request_close() {
    CLOSING.store(true, Ordering::Relaxed);
}

static HAS_CLIENT: AtomicBool = AtomicBool::new(false);
static KICK: AtomicBool = AtomicBool::new(false);

/// A device is streaming from this PC.
pub fn has_client() -> bool {
    HAS_CLIENT.load(Ordering::Relaxed)
}

pub(crate) fn set_has_client(on: bool) {
    HAS_CLIENT.store(on, Ordering::Relaxed);
}

/// End the current session (the user pressed Disconnect).
pub fn request_disconnect() {
    KICK.store(true, Ordering::Relaxed);
}

pub(crate) fn take_disconnect() -> bool {
    KICK.swap(false, Ordering::Relaxed)
}

/// Use another window as the owner of approval prompts (combined app).
pub fn set_owner(hwnd: HWND) {
    HWND_RAW.store(hwnd.0 as isize, Ordering::Release);
}
// Control handles. Atomics, not a lock: SetWindowText can re-enter the
// window proc (WM_CTLCOLORSTATIC) on this thread.
static CTL_STATE: AtomicIsize = AtomicIsize::new(0);
static CTL_DETAIL: AtomicIsize = AtomicIsize::new(0);
static CTL_ID: AtomicIsize = AtomicIsize::new(0);
static CTL_KICK: AtomicIsize = AtomicIsize::new(0);

fn ctl(a: &AtomicIsize) -> HWND {
    HWND(a.load(Ordering::Acquire) as *mut _)
}

const ID_COPY: usize = 1;
const ID_KICK: usize = 2;
const TIMER: usize = 1;

/// The panel window, if it exists (owner for the approval prompts).
pub fn hwnd() -> Option<HWND> {
    match HWND_RAW.load(Ordering::Acquire) {
        0 => None,
        h => Some(HWND(h as *mut _)),
    }
}

/// Open the window on a new thread. Closing it exits the host.
pub fn spawn(id: String, server: String) {
    let _ = std::thread::Builder::new()
        .name("panel".into())
        .spawn(move || {
            if let Err(e) = run(&id, &server) {
                // Never share the screen without a visible window.
                tracing::error!("host window failed: {e:#}");
                ui::message(
                    None,
                    "Syncra host",
                    &format!("Could not open the host window: {e:#}"),
                    true,
                );
                std::process::exit(1);
            }
        });
}

fn run(id: &str, server: &str) -> anyhow::Result<()> {
    let class = w!("SyncraHostPanel");
    ui::register_class(class, Some(proc))?;
    let hwnd = ui::create_window(class, "Syncra host", 420, 250)?;
    let small = ui::font(hwnd, 10, false);
    let big = ui::font(hwnd, 28, true);
    let normal = ui::font(hwnd, 11, false);
    let center = SS_CENTER;
    ui::child(
        hwnd,
        w!("STATIC"),
        "Your host ID",
        center,
        20,
        18,
        380,
        22,
        0,
        small,
    )?;
    let id_label = ui::child(hwnd, w!("STATIC"), id, center, 20, 42, 380, 50, 0, big)?;
    let copy = ui::child(
        hwnd,
        w!("BUTTON"),
        "Copy ID",
        BS_DEFPUSHBUTTON | WS_TABSTOP,
        85,
        100,
        120,
        32,
        ID_COPY,
        normal,
    )?;
    let kick = ui::child(
        hwnd,
        w!("BUTTON"),
        "Disconnect",
        BS_PUSHBUTTON | WS_TABSTOP,
        215,
        100,
        120,
        32,
        ID_KICK,
        normal,
    )?;
    ui::enable(kick, false);
    let state = ui::child(
        hwnd,
        w!("STATIC"),
        "Starting…",
        center,
        20,
        148,
        380,
        24,
        0,
        normal,
    )?;
    let detail = ui::child(hwnd, w!("STATIC"), "", center, 20, 174, 380, 22, 0, small)?;
    ui::child(
        hwnd,
        w!("STATIC"),
        &format!("You approve every device  ·  server {server}"),
        center,
        20,
        212,
        380,
        20,
        0,
        small,
    )?;
    CTL_STATE.store(state.0 as isize, Ordering::Release);
    CTL_DETAIL.store(detail.0 as isize, Ordering::Release);
    CTL_ID.store(id_label.0 as isize, Ordering::Release);
    CTL_KICK.store(kick.0 as isize, Ordering::Release);
    HWND_RAW.store(hwnd.0 as isize, Ordering::Release);
    ui::set_timer(hwnd, TIMER, 500);
    ui::show(hwnd);
    ui::set_focus(copy);
    ui::message_loop(hwnd);
    Ok(())
}

extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        ui::WM_TIMER => {
            let state = STATUS.state.lock().unwrap().clone();
            let detail = STATUS.detail.lock().unwrap().clone();
            ui::set_text(ctl(&CTL_STATE), &state);
            ui::set_text(ctl(&CTL_DETAIL), &detail);
            ui::enable(ctl(&CTL_KICK), has_client());
            LRESULT(0)
        }
        ui::WM_COMMAND if wparam.0 & 0xffff == ID_KICK => {
            request_disconnect();
            LRESULT(0)
        }
        ui::WM_COMMAND if wparam.0 & 0xffff == ID_COPY => {
            let id = ui::get_text(ctl(&CTL_ID));
            match ui::copy_text(hwnd, &id) {
                Ok(()) => STATUS.set_detail("ID copied to the clipboard"),
                Err(e) => tracing::warn!("copy failed: {e}"),
            }
            LRESULT(0)
        }
        ui::WM_CTLCOLORSTATIC => {
            let c = lparam.0;
            let color = if c == CTL_ID.load(Ordering::Acquire) {
                Some(ui::ACCENT)
            } else if c == CTL_DETAIL.load(Ordering::Acquire) {
                Some(ui::MUTED)
            } else {
                None
            };
            ui::static_color(wparam, color)
        }
        ui::WM_DESTROY => {
            // Closing the window stops sharing: the encode loop sees the flag
            // and the host shuts down normally. Force it if that stalls.
            CLOSING.store(true, Ordering::Relaxed);
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(3));
                std::process::exit(0);
            });
            ui::quit();
            LRESULT(0)
        }
        _ => ui::default_proc(hwnd, msg, wparam, lparam),
    }
}
