//! Non-Windows socket specifics: buffer sizes only. Segmentation offload
//! (Linux `UDP_SEGMENT`) is not used; the Linux tools send one datagram at a
//! time.

use std::io;
use std::net::{SocketAddr, UdpSocket};

pub fn set_buffers(sock: &UdpSocket, send: i32, recv: i32) -> io::Result<()> {
    let s = socket2::SockRef::from(sock);
    s.set_send_buffer_size(send as usize)?;
    s.set_recv_buffer_size(recv as usize)
}

pub fn probe_uso(_sock: &UdpSocket) -> io::Result<bool> {
    Ok(false)
}

pub fn uso_rejected(_e: &io::Error) -> bool {
    true
}

pub fn send_uso(
    _sock: &UdpSocket,
    _buf: &[u8],
    _segment: usize,
    _to: SocketAddr,
) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
