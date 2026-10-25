//! Client → host input. Each event goes out immediately in its own datagram
//! together with the previous few events, so one lost datagram costs nothing.
//! Callable from any thread (window procedure, gamepad poller).
//!
//! The datagram is built under the lock (to keep sequence numbers and history
//! consistent) and sent after the lock is released, so the UI and gamepad
//! threads never wait on each other's syscalls. Two datagrams may then leave
//! out of order; the host still applies events in sequence order because each
//! datagram repeats its predecessors.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use net::Socket;
use proto::input::{encode_input, InputEvent, Sequenced, INPUT_REDUNDANCY};
use proto::{clock, Channel, Header, HEADER_LEN};

/// Largest input datagram: header + count + 8 × (seq + largest event).
const MAX_INPUT_DATAGRAM: usize = 256;

struct Sender {
    socket: Arc<Socket>,
    host: SocketAddr,
    seq: u32,
    history: VecDeque<Sequenced>,
    newest_first: Vec<Sequenced>,
    buf: Vec<u8>,
}

static SENDER: Mutex<Option<Sender>> = Mutex::new(None);

/// Start sending input to `host` through (a clone of) the streaming socket.
pub fn start(socket: Socket, host: SocketAddr) {
    *SENDER.lock().unwrap() = Some(Sender {
        socket: Arc::new(socket),
        host,
        seq: 0,
        history: VecDeque::with_capacity(INPUT_REDUNDANCY),
        newest_first: Vec::with_capacity(INPUT_REDUNDANCY),
        buf: Vec::with_capacity(MAX_INPUT_DATAGRAM),
    });
}

pub fn stop() {
    *SENDER.lock().unwrap() = None;
}

/// Send one event now (no-op until `start`).
pub fn send(event: InputEvent) {
    let mut packet = [0u8; MAX_INPUT_DATAGRAM];
    let (socket, host, len) = {
        let mut guard = SENDER.lock().unwrap();
        let Some(s) = guard.as_mut() else { return };

        s.seq = s.seq.wrapping_add(1);
        if s.history.len() == INPUT_REDUNDANCY {
            s.history.pop_front();
        }
        s.history.push_back(Sequenced { seq: s.seq, event });
        s.newest_first.clear();
        s.newest_first.extend(s.history.iter().rev());

        s.buf.clear();
        s.buf.resize(HEADER_LEN, 0);
        Header {
            channel: Channel::Input as u8,
            sequence: s.seq,
            send_us: clock::now_us(),
            ..Default::default()
        }
        .write(&mut s.buf[..HEADER_LEN]);
        encode_input(&s.newest_first, &mut s.buf);
        let len = s.buf.len().min(MAX_INPUT_DATAGRAM);
        packet[..len].copy_from_slice(&s.buf[..len]);
        (s.socket.clone(), s.host, len)
    };
    let _ = socket.send_to(&packet[..len], host);
}
