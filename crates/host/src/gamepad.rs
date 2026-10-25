//! Virtual Xbox 360 controllers via the ViGEmBus driver.
//!
//! Report updates to a plugged pad are a short IOCTL (~10-20 µs) and run
//! directly on `net-rx`, so a gamepad state reaches the virtual pad with no
//! thread hop. Plugging is different: it waits on PnP (seconds on the first
//! plug after a driver install), so it runs on a worker thread and `net-rx`
//! never blocks on it; states for a pad that is still being plugged are
//! dropped (the client re-sends full state every 50 ms).
//!
//! Rumble from games is forwarded to the current peer as `Control::Rumble`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use net::Socket;
use proto::input::GamepadState;
use proto::Control;
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

use crate::session::Shared;

const MAX_PADS: usize = 4;
/// Wait this long before re-plugging a pad whose plug or updates failed.
const REPLUG_COOLDOWN: Duration = Duration::from_secs(2);
/// Report errors right after plugging are expected while Windows enumerates
/// the device (first plug after installing ViGEmBus).
const READY_GRACE: Duration = Duration::from_secs(10);

struct Pad {
    target: Xbox360Wired<Arc<Client>>,
    ready_by: Option<Instant>,
    _rumble: Option<std::thread::JoinHandle<()>>,
}

struct Slots {
    pads: [Mutex<Option<Pad>>; MAX_PADS],
    plugging: [AtomicBool; MAX_PADS],
    retry_at: [Mutex<Option<Instant>>; MAX_PADS],
}

/// Owned by `net-rx`.
pub struct Gamepads {
    slots: Arc<Slots>,
    plug_tx: mpsc::Sender<u8>,
}

impl Gamepads {
    pub fn spawn(socket: Option<Socket>, shared: Arc<Shared>) -> Self {
        let slots = Arc::new(Slots {
            pads: Default::default(),
            plugging: Default::default(),
            retry_at: Default::default(),
        });
        let (plug_tx, plug_rx) = mpsc::channel::<u8>();
        let worker_slots = slots.clone();
        std::thread::Builder::new()
            .name("gamepad-plug".into())
            .spawn(move || {
                Plugger {
                    client: None,
                    unavailable: false,
                    socket,
                    shared,
                    slots: worker_slots,
                }
                .run(plug_rx)
            })
            .expect("spawn gamepad plug thread");
        Gamepads { slots, plug_tx }
    }

    /// Apply a pad state now, or request a plug if the pad isn't there yet.
    pub fn update(&self, state: GamepadState) {
        let i = state.index as usize;
        if i >= MAX_PADS {
            return;
        }
        let mut slot = self.slots.pads[i].lock().unwrap();
        let Some(pad) = slot.as_mut() else {
            drop(slot);
            self.request_plug(state.index);
            return;
        };
        let report = XGamepad {
            buttons: XButtons::from(state.buttons),
            left_trigger: state.left_trigger,
            right_trigger: state.right_trigger,
            thumb_lx: state.lx,
            thumb_ly: state.ly,
            thumb_rx: state.rx,
            thumb_ry: state.ry,
        };
        match pad.target.update(&report) {
            Ok(()) => {
                if pad.ready_by.take().is_some() {
                    tracing::info!("gamepad {i} ready");
                }
            }
            Err(_) if pad.ready_by.is_some_and(|t| Instant::now() < t) => {}
            Err(e) => {
                tracing::warn!(
                    "gamepad {i} update failed: {e:?}; replugging in {REPLUG_COOLDOWN:?}"
                );
                // Unplugging is a quick IOCTL; the replug happens on the worker.
                *slot = None;
                *self.slots.retry_at[i].lock().unwrap() = Some(Instant::now() + REPLUG_COOLDOWN);
            }
        }
    }

    /// Center sticks, release buttons and triggers on every plugged pad.
    pub fn neutral(&self) {
        for i in 0..MAX_PADS {
            if self.slots.pads[i].lock().unwrap().is_some() {
                self.update(GamepadState {
                    index: i as u8,
                    ..Default::default()
                });
            }
        }
    }

    pub fn disconnect_all(&self) {
        for i in 0..MAX_PADS {
            *self.slots.pads[i].lock().unwrap() = None; // Xbox360Wired unplugs on drop
            *self.slots.retry_at[i].lock().unwrap() = None;
        }
    }

    fn request_plug(&self, index: u8) {
        let i = index as usize;
        if self.slots.retry_at[i]
            .lock()
            .unwrap()
            .is_some_and(|t| Instant::now() < t)
        {
            return;
        }
        if !self.slots.plugging[i].swap(true, Ordering::AcqRel) {
            let _ = self.plug_tx.send(index);
        }
    }
}

struct Plugger {
    client: Option<Arc<Client>>,
    unavailable: bool,
    socket: Option<Socket>,
    shared: Arc<Shared>,
    slots: Arc<Slots>,
}

impl Plugger {
    fn run(mut self, rx: mpsc::Receiver<u8>) {
        while let Ok(index) = rx.recv() {
            let i = index as usize;
            match self.plug(index) {
                Some(pad) => *self.slots.pads[i].lock().unwrap() = Some(pad),
                None => {
                    *self.slots.retry_at[i].lock().unwrap() = Some(Instant::now() + REPLUG_COOLDOWN)
                }
            }
            self.slots.plugging[i].store(false, Ordering::Release);
        }
    }

    fn plug(&mut self, index: u8) -> Option<Pad> {
        if self.unavailable {
            return None;
        }
        if self.client.is_none() {
            match Client::connect() {
                Ok(c) => self.client = Some(Arc::new(c)),
                Err(e) => {
                    tracing::warn!("ViGEmBus not available ({e:?}); gamepad input disabled. Install ViGEmBus on the host.");
                    self.unavailable = true;
                    return None;
                }
            }
        }
        let mut target = Xbox360Wired::new(self.client.clone().unwrap(), TargetId::XBOX360_WIRED);
        if let Err(e) = target.plugin() {
            tracing::warn!("gamepad {index} plugin failed: {e:?}");
            return None;
        }
        // Blocking is fine here (worker thread). Early report errors are still
        // tolerated for READY_GRACE in case PnP is slower than this.
        if let Err(e) = target.wait_ready() {
            tracing::debug!("gamepad {index} not ready yet: {e:?}");
        }
        tracing::info!("gamepad {index} plugged in (virtual Xbox 360)");

        let sock = self.socket.as_ref().and_then(|s| s.try_clone().ok());
        let rumble = match (target.request_notification(), sock) {
            (Ok(req), Some(sock)) => {
                let shared = self.shared.clone();
                Some(req.spawn_thread(move |_, n| {
                    // Read the peer now, not at plug time: it may have reconnected.
                    if let Some(peer) = shared.peer() {
                        let msg = Control::Rumble {
                            index,
                            large: n.large_motor,
                            small: n.small_motor,
                        };
                        let _ = sock.send_to(&msg.datagram(0), peer);
                    }
                }))
            }
            (Err(e), _) => {
                tracing::warn!("gamepad {index}: rumble notifications unavailable: {e:?}");
                None
            }
            _ => None,
        };
        Some(Pad {
            target,
            ready_by: Some(Instant::now() + READY_GRACE),
            _rumble: rumble,
        })
    }
}
