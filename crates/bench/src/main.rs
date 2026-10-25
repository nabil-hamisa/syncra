//! Latency benchmarks that need no human in the loop.
//!
//! `bench photon` measures click-to-photon through any streaming client
//! (see [`photon`]).
//!
//! `bench gamepad --host ADDR` acts as a client: it sends gamepad states to a
//! running host and polls XInput until the host's virtual pad (ViGEmBus)
//! reports each value, then sets vibration on that pad and waits for the
//! host to forward the rumble back. Host and bench must run on the same
//! machine (the virtual pad is local).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use net::Socket;
use proto::input::{encode_input, GamepadState, InputEvent, Sequenced};
use proto::{clock, Channel, Control, Header, HEADER_LEN};
use windows::Win32::UI::Input::XboxController::*;

mod photon;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Gamepad send → virtual pad visible in XInput, and rumble round trip.
    Gamepad {
        #[arg(long, default_value = "127.0.0.1:9000")]
        host: SocketAddr,
        #[arg(long, default_value_t = 300)]
        samples: u32,
    },
    /// Click → target toggles on the host desktop → seen in the client window.
    Photon {
        #[arg(long, default_value_t = 200)]
        samples: u32,
        /// Click the target itself, without streaming (local baseline).
        #[arg(long)]
        direct: bool,
        /// Target square on the captured monitor: x,y,size (pixels).
        #[arg(long, value_delimiter = ',', num_args = 3, default_values_t = [200, 200, 400])]
        target: Vec<i32>,
        /// Window class of the stream window.
        #[arg(long, default_value = "SyncraClient")]
        class: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
    match Args::parse().cmd {
        Cmd::Gamepad { host, samples } => gamepad(host, samples),
        Cmd::Photon {
            samples,
            direct,
            target,
            class,
        } => photon::run(photon::Opts {
            samples,
            direct,
            target: [target[0], target[1], target[2]],
            class,
        }),
    }
}

struct FakeClient {
    socket: Socket,
    host: SocketAddr,
    seq: u32,
}

impl FakeClient {
    fn connect(host: SocketAddr) -> Result<Self> {
        let socket = Socket::bind("127.0.0.1:0".parse().unwrap())?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let c = FakeClient {
            socket,
            host,
            seq: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 65536];
        loop {
            c.socket.send_to(&Control::Hello.datagram(0), host)?;
            if c.socket.recv_from(&mut buf).is_ok() {
                return Ok(c); // host is streaming to us
            }
            if Instant::now() > deadline {
                bail!("no response from host at {host}");
            }
        }
    }

    fn send(&mut self, event: InputEvent) -> Result<()> {
        self.seq += 1;
        let mut d = vec![0u8; HEADER_LEN];
        Header {
            channel: Channel::Input as u8,
            sequence: self.seq,
            send_us: clock::now_us(),
            ..Default::default()
        }
        .write(&mut d);
        encode_input(
            &[Sequenced {
                seq: self.seq,
                event,
            }],
            &mut d,
        );
        self.socket.send_to(&d, self.host)?;
        Ok(())
    }
}

impl Drop for FakeClient {
    fn drop(&mut self) {
        let _ = self.socket.send_to(&Control::Bye.datagram(0), self.host);
    }
}

fn xinput(i: u32) -> Option<XINPUT_GAMEPAD> {
    let mut st = XINPUT_STATE::default();
    (unsafe { XInputGetState(i, &mut st) } == 0).then_some(st.Gamepad)
}

fn pad(lx: i16) -> InputEvent {
    InputEvent::Gamepad(GamepadState {
        index: 0,
        buttons: 0x1000,
        lx,
        ..Default::default()
    })
}

fn gamepad(host: SocketAddr, samples: u32) -> Result<()> {
    let before: Vec<bool> = (0..4).map(|i| xinput(i).is_some()).collect();
    let mut client = FakeClient::connect(host)?;
    tracing::info!("connected to host {host}");

    // First state plugs the virtual pad; find which XInput slot it got.
    client.send(pad(1))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let slot = loop {
        if let Some(i) =
            (0..4u32).find(|&i| !before[i as usize] && xinput(i).is_some_and(|g| g.sThumbLX == 1))
        {
            break i;
        }
        if Instant::now() > deadline {
            bail!("virtual pad never appeared in XInput (is ViGEmBus installed and the host running?)");
        }
        client.send(pad(1))?;
        std::thread::sleep(Duration::from_millis(10));
    };
    tracing::info!("virtual pad is XInput slot {slot}");

    let mut lat = Vec::with_capacity(samples as usize);
    for k in 0..samples {
        let value = 100 + k as i16;
        let t = Instant::now();
        client.send(pad(value))?;
        loop {
            if xinput(slot).is_some_and(|g| g.sThumbLX == value) {
                lat.push(t.elapsed().as_micros() as u32);
                break;
            }
            if t.elapsed() > Duration::from_millis(500) {
                bail!("sample {k}: value {value} never reached the virtual pad");
            }
            std::hint::spin_loop();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    report("gamepad: send → visible in XInput", &mut lat);

    // Rumble: a "game" vibrates the virtual pad; the host must forward it.
    let mut rumble_lat = Vec::new();
    let mut buf = [0u8; 65536];
    for k in 0..20u16 {
        let speed = 0x1000 + k * 0x0800;
        let v = XINPUT_VIBRATION {
            wLeftMotorSpeed: speed,
            wRightMotorSpeed: speed / 2,
        };
        let t = Instant::now();
        unsafe {
            XInputSetState(slot, &v);
        }
        let got = loop {
            if t.elapsed() > Duration::from_millis(500) {
                break false;
            }
            let Ok((n, _)) = client.socket.recv_from(&mut buf) else {
                continue;
            };
            let Some((h, p)) = Header::read(&buf[..n]) else {
                continue;
            };
            if h.channel() == Some(Channel::Control) {
                if let Some(Control::Rumble { large, .. }) = Control::decode(p) {
                    if large == (speed >> 8) as u8 {
                        break true;
                    }
                }
            }
        };
        if !got {
            bail!("rumble {k} was not forwarded by the host");
        }
        rumble_lat.push(t.elapsed().as_micros() as u32);
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        XInputSetState(slot, &XINPUT_VIBRATION::default());
    }
    report(
        "rumble: XInputSetState → host forwards to client",
        &mut rumble_lat,
    );
    Ok(())
}

fn report(name: &str, v: &mut [u32]) {
    v.sort_unstable();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1000.0;
    tracing::info!(
        "{name}: n={} p50 {:.3} ms p95 {:.3} ms p99 {:.3} ms max {:.3} ms",
        v.len(),
        p(0.5),
        p(0.95),
        p(0.99),
        p(1.0)
    );
}
