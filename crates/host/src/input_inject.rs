//! Inject client input with `SendInput`.
//!
//! Events arrive with redundancy (each datagram repeats recent events), so
//! every sequence number is applied at most once, in order. All keyboard and
//! mouse events of one datagram go to a single `SendInput` call (cheaper, and
//! atomic with respect to other input).

use std::collections::HashSet;

use proto::input::{InputEvent, Sequenced};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub struct Injector {
    /// Captured monitor in virtual-desktop pixels.
    monitor: RECT,
    keys_down: HashSet<(u16, bool)>,
    buttons_down: [bool; 5],
    last_seq: Option<u32>,
    pending: Vec<Sequenced>,
    batch: Vec<INPUT>,
    /// SendInput failures since the last report (UIPI: elevated window or secure desktop).
    blocked: u32,
}

impl Injector {
    pub fn new(monitor: RECT) -> Self {
        Injector {
            monitor,
            keys_down: HashSet::new(),
            buttons_down: [false; 5],
            last_seq: None,
            pending: Vec::with_capacity(16),
            batch: Vec::with_capacity(32),
            blocked: 0,
        }
    }

    /// Apply the not-yet-seen events of one datagram and return them, oldest
    /// first. Gamepad events are returned but not injected here; the caller
    /// forwards them to the gamepad thread.
    pub fn apply(&mut self, events: &[Sequenced]) -> &[Sequenced] {
        self.pending.clear();
        let last = self.last_seq;
        self.pending.extend(events.iter().filter(|e| match last {
            None => true,
            Some(last) => (e.seq.wrapping_sub(last) as i32) > 0,
        }));
        let base = last.unwrap_or(0);
        self.pending.sort_by_key(|e| e.seq.wrapping_sub(base));
        for i in 0..self.pending.len() {
            let e = self.pending[i];
            self.inject(e.event);
            self.last_seq = Some(e.seq);
        }
        self.flush();
        &self.pending
    }

    fn inject(&mut self, event: InputEvent) {
        match event {
            InputEvent::MouseAbs { x, y } => self.mouse_abs(x, y),
            InputEvent::MouseRel { dx, dy } => {
                self.batch
                    .push(mouse(dx as i32, dy as i32, 0, MOUSEEVENTF_MOVE))
            }
            InputEvent::MouseButton { button, down } => {
                if let Some(b) = self.buttons_down.get_mut(button as usize) {
                    *b = down;
                }
                self.batch.extend(button_input(button, down));
            }
            InputEvent::MouseWheel { delta, horizontal } => {
                let flag = if horizontal {
                    MOUSEEVENTF_HWHEEL
                } else {
                    MOUSEEVENTF_WHEEL
                };
                self.batch.push(mouse(0, 0, delta as i32, flag));
            }
            InputEvent::Key {
                scancode,
                extended,
                down,
            } => {
                if down {
                    self.keys_down.insert((scancode, extended));
                } else {
                    self.keys_down.remove(&(scancode, extended));
                }
                self.batch.push(key(scancode, extended, down));
            }
            InputEvent::Gamepad(_) => {}
            InputEvent::ReleaseAll => self.queue_release_all(),
        }
    }

    /// Send the batched input. UIPI makes `SendInput` fail when the foreground
    /// window is elevated or the secure desktop is active; controlling those
    /// needs an elevated host (or service).
    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        // SAFETY: `batch` holds fully initialized INPUT values and cbSize matches the type.
        let sent = unsafe { SendInput(&self.batch, std::mem::size_of::<INPUT>() as i32) };
        if (sent as usize) < self.batch.len() {
            if self.blocked == 0 {
                tracing::warn!(
                    "SendInput blocked ({}): the foreground window is elevated or on the secure desktop; run the host elevated to control it",
                    windows::core::Error::from_thread()
                );
            }
            self.blocked += 1;
        } else if self.blocked > 0 {
            tracing::info!(
                "SendInput working again ({} batches were blocked)",
                self.blocked
            );
            self.blocked = 0;
        }
        self.batch.clear();
    }

    /// Release everything this injector has pressed (client lost focus or left).
    pub fn release_all(&mut self) {
        self.queue_release_all();
        self.flush();
    }

    fn queue_release_all(&mut self) {
        for (sc, ext) in self.keys_down.drain() {
            self.batch.push(key(sc, ext, false));
        }
        for (i, down) in self.buttons_down.iter_mut().enumerate() {
            if std::mem::take(down) {
                self.batch.extend(button_input(i as u8, false));
            }
        }
    }

    /// Forget sequence state (new client session).
    pub fn reset(&mut self) {
        self.release_all();
        self.last_seq = None;
    }

    fn mouse_abs(&mut self, x: u16, y: u16) {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let virt = RECT {
            left: vx,
            top: vy,
            right: vx + vw,
            bottom: vy + vh,
        };
        let (nx, ny) = to_virtual_desktop(x, y, &self.monitor, &virt);
        self.batch.push(mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ));
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Video-normalized (0..=65535 across the captured monitor) → monitor pixel →
/// `MOUSEEVENTF_VIRTUALDESK` coordinates (0..=65535 across the virtual desktop).
fn to_virtual_desktop(x: u16, y: u16, monitor: &RECT, virt: &RECT) -> (i32, i32) {
    let m = monitor;
    let px = m.left as i64 + x as i64 * (m.right - m.left - 1) as i64 / 65535;
    let py = m.top as i64 + y as i64 * (m.bottom - m.top - 1) as i64 / 65535;
    let (vw, vh) = (
        (virt.right - virt.left) as i64,
        (virt.bottom - virt.top) as i64,
    );
    // Round to nearest so the OS maps back to exactly `px`/`py`.
    let nx = ((px - virt.left as i64) * 65535 + (vw - 1) / 2) / (vw - 1).max(1);
    let ny = ((py - virt.top as i64) * 65535 + (vh - 1) / 2) / (vh - 1).max(1);
    (nx as i32, ny as i32)
}

fn button_input(button: u8, down: bool) -> Option<INPUT> {
    let (flag, data) = match (button, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3 | 4, true) => (MOUSEEVENTF_XDOWN, button as i32 - 2),
        (3 | 4, false) => (MOUSEEVENTF_XUP, button as i32 - 2),
        _ => return None,
    };
    Some(mouse(0, 0, data, flag))
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(scancode: u16, extended: bool, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_mapping_hits_exact_pixels_on_either_monitor() {
        // This machine: two 1920x1080 monitors side by side.
        let virt = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 1080,
        };
        // How Windows maps VIRTUALDESK coordinates back to pixels.
        let back = |n: i32, extent: i64| (n as i64 * (extent - 1) + 32767) / 65535;
        let monitors = [
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            RECT {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1080,
            },
        ];
        for monitor in monitors {
            // Corners land on the monitor's first and last pixel.
            let first = to_virtual_desktop(0, 0, &monitor, &virt);
            let last = to_virtual_desktop(65535, 65535, &monitor, &virt);
            assert_eq!(
                (back(first.0, 3840), back(first.1, 1080)),
                (monitor.left as i64, 0)
            );
            assert_eq!(
                (back(last.0, 3840), back(last.1, 1080)),
                (monitor.right as i64 - 1, 1079)
            );
            // Every video coordinate lands exactly on its intended monitor pixel.
            for v in (0..=65535u32).step_by(7) {
                let v = v as u16;
                let want_x = monitor.left as i64 + v as i64 * 1919 / 65535;
                let want_y = v as i64 * 1079 / 65535;
                let (nx, ny) = to_virtual_desktop(v, v, &monitor, &virt);
                assert_eq!(
                    (back(nx, 3840), back(ny, 1080)),
                    (want_x, want_y),
                    "video {v} on {monitor:?}"
                );
            }
        }
    }

    #[test]
    fn applies_each_sequence_once_in_order() {
        // Only ReleaseAll with nothing pressed: the test injects nothing real.
        let mut inj = Injector::new(RECT::default());
        let ev = |seq| Sequenced {
            seq,
            event: InputEvent::ReleaseAll,
        };
        let seqs = |v: &[Sequenced]| v.iter().map(|e| e.seq).collect::<Vec<_>>();
        assert_eq!(
            seqs(inj.apply(&[ev(3), ev(2), ev(1)])),
            [1, 2, 3],
            "oldest first"
        );
        assert_eq!(inj.last_seq, Some(3));
        assert_eq!(
            seqs(inj.apply(&[ev(4), ev(3), ev(2)])),
            [4],
            "redundant copies are skipped"
        );
        assert!(inj.apply(&[ev(2)]).is_empty(), "old packet ignored");
        assert_eq!(
            seqs(inj.apply(&[ev(7), ev(6), ev(5)])),
            [5, 6, 7],
            "gap recovered from redundancy"
        );
        assert_eq!(inj.last_seq, Some(7));
    }
}
