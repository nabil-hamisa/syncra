//! XInput gamepads: polled at 1 kHz on their own thread. A state is sent the
//! moment it changes, plus a full snapshot every 50 ms so a lost datagram
//! heals quickly. Rumble from the host is applied with `XInputSetState`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use proto::input::{GamepadState, InputEvent};
use windows::Win32::UI::Input::XboxController::*;

use crate::{input, window};

const PADS: u32 = 4;
const SNAPSHOT: Duration = Duration::from_millis(50);
/// Polling an empty XInput slot is expensive; probe them only this often.
const PROBE_EMPTY: Duration = Duration::from_secs(1);

pub fn spawn(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("gamepad".into())
        .spawn(move || poll_loop(&stop))
        .expect("spawn gamepad thread")
}

fn poll_loop(stop: &AtomicBool) {
    crate::realtime_thread();
    let mut last_packet = [None::<u32>; PADS as usize];
    let mut last_sent = [Instant::now(); PADS as usize];
    let mut next_probe = [Instant::now(); PADS as usize];
    const PERIOD: Duration = Duration::from_millis(1);
    let mut deadline = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let focused = window::is_focused();
        for i in 0..PADS {
            let idx = i as usize;
            if last_packet[idx].is_none() && Instant::now() < next_probe[idx] {
                continue;
            }
            let mut st = XINPUT_STATE::default();
            if unsafe { XInputGetState(i, &mut st) } != 0 {
                if last_packet[idx].take().is_some() {
                    tracing::info!("gamepad {i} disconnected");
                }
                next_probe[idx] = Instant::now() + PROBE_EMPTY;
                continue;
            }
            if last_packet[idx].is_none() {
                tracing::info!("gamepad {i} connected");
            }
            let changed = last_packet[idx] != Some(st.dwPacketNumber);
            if focused && (changed || last_sent[idx].elapsed() >= SNAPSHOT) {
                let g = st.Gamepad;
                input::send(InputEvent::Gamepad(GamepadState {
                    index: i as u8,
                    buttons: g.wButtons.0,
                    left_trigger: g.bLeftTrigger,
                    right_trigger: g.bRightTrigger,
                    lx: g.sThumbLX,
                    ly: g.sThumbLY,
                    rx: g.sThumbRX,
                    ry: g.sThumbRY,
                }));
                last_sent[idx] = Instant::now();
            }
            last_packet[idx] = Some(st.dwPacketNumber);
        }
        // Absolute deadlines: a late wake-up does not push every later poll back.
        // (std's sleep uses a high-resolution waitable timer on Windows.)
        deadline += PERIOD;
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        } else {
            deadline = now;
        }
    }
}

/// Apply host rumble to a local pad.
pub fn rumble(index: u8, large: u8, small: u8) {
    let v = XINPUT_VIBRATION {
        wLeftMotorSpeed: large as u16 * 257,
        wRightMotorSpeed: small as u16 * 257,
    };
    unsafe {
        let _ = XInputSetState(index as u32, &v);
    }
}
