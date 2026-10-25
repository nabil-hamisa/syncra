//! Windows socket specifics: buffer sizes and per-message USO.
//!
//! USO is requested per message (`WSASendMsg` with a `UDP_SEND_MSG_SIZE`
//! control message), not per socket: clones of a socket share OS state, so a
//! socket-wide segment size set for one chunk could split another thread's
//! unrelated datagram.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;

use windows::core::PSTR;
use windows::Win32::Networking::WinSock::*;

const IPPROTO_UDP_LEVEL: i32 = 17;
/// `UDP_SEND_MSG_SIZE` from ws2ipdef.h: segment size for USO.
const UDP_SEND_MSG_SIZE_OPT: i32 = 2;

fn raw(sock: &UdpSocket) -> SOCKET {
    SOCKET(sock.as_raw_socket() as usize)
}

fn set_int(sock: &UdpSocket, level: i32, opt: i32, value: i32) -> io::Result<()> {
    // SAFETY: valid socket handle; the option value is a 4-byte int.
    let r = unsafe { setsockopt(raw(sock), level, opt, Some(&value.to_ne_bytes())) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn set_buffers(sock: &UdpSocket, send: i32, recv: i32) -> io::Result<()> {
    set_int(sock, SOL_SOCKET, SO_SNDBUF, send)?;
    set_int(sock, SOL_SOCKET, SO_RCVBUF, recv)
}

/// Whether USO works (probed with the socket option, which is then reset to
/// 0 so it never applies to plain sends).
pub fn probe_uso(sock: &UdpSocket) -> io::Result<bool> {
    if let Err(e) = set_int(sock, IPPROTO_UDP_LEVEL, UDP_SEND_MSG_SIZE_OPT, 1200) {
        tracing::warn!("USO unavailable ({e}); using per-datagram sends");
        return Ok(false);
    }
    // The probe must be undone: a socket-wide segment size left in place
    // would make the kernel split every plain datagram above it, on this
    // socket and all its clones.
    set_int(sock, IPPROTO_UDP_LEVEL, UDP_SEND_MSG_SIZE_OPT, 0)
        .or_else(|_| set_int(sock, IPPROTO_UDP_LEVEL, UDP_SEND_MSG_SIZE_OPT, 0))
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("could not reset UDP_SEND_MSG_SIZE after probing: {e}"),
            )
        })?;
    Ok(true)
}

/// The OS refused per-message USO for this send.
pub fn uso_rejected(e: &io::Error) -> bool {
    e.raw_os_error() == Some(WSAEINVAL.0) || e.raw_os_error() == Some(WSAEOPNOTSUPP.0)
}

/// `WSA_CMSG_SPACE(sizeof(DWORD))`: header + u32, padded to 8 bytes.
#[repr(C, align(8))]
struct UsoControl {
    hdr: CMSGHDR,
    segment: u32,
    _pad: u32,
}

pub fn send_uso(sock: &UdpSocket, buf: &[u8], segment: usize, to: SocketAddr) -> io::Result<()> {
    let (mut addr, addr_len) = sockaddr(to);
    let mut data = WSABUF {
        len: buf.len() as u32,
        buf: PSTR(buf.as_ptr() as *mut u8),
    };
    let mut control = UsoControl {
        hdr: CMSGHDR {
            // WSA_CMSG_LEN(sizeof(DWORD)): header + data, unpadded.
            cmsg_len: std::mem::size_of::<CMSGHDR>() + std::mem::size_of::<u32>(),
            cmsg_level: IPPROTO_UDP_LEVEL,
            cmsg_type: UDP_SEND_MSG_SIZE_OPT,
        },
        segment: segment as u32,
        _pad: 0,
    };
    let msg = WSAMSG {
        name: &mut addr as *mut _ as *mut SOCKADDR,
        namelen: addr_len,
        lpBuffers: &mut data,
        dwBufferCount: 1,
        Control: WSABUF {
            len: std::mem::size_of::<UsoControl>() as u32,
            buf: PSTR(&mut control as *mut _ as *mut u8),
        },
        dwFlags: 0,
    };
    let mut sent = 0u32;
    // SAFETY: every pointer in `msg` refers to a local that outlives this
    // synchronous (non-overlapped) call; WSASendMsg only reads them.
    let r = unsafe { WSASendMsg(raw(sock), &msg, 0, Some(&mut sent), None, None) };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0));
    }
    Ok(())
}

/// Storage big enough for IPv4 or IPv6 socket addresses.
#[repr(C)]
union SockAddrStorage {
    v4: SOCKADDR_IN,
    v6: SOCKADDR_IN6,
}

fn sockaddr(to: SocketAddr) -> (SockAddrStorage, i32) {
    match to {
        SocketAddr::V4(a) => {
            let v4 = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: a.port().to_be(),
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                },
                sin_zero: [0; 8],
            };
            (
                SockAddrStorage { v4 },
                std::mem::size_of::<SOCKADDR_IN>() as i32,
            )
        }
        SocketAddr::V6(a) => {
            let v6 = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: a.ip().octets(),
                    },
                },
                Anonymous: SOCKADDR_IN6_0 {
                    sin6_scope_id: a.scope_id(),
                },
            };
            (
                SockAddrStorage { v6 },
                std::mem::size_of::<SOCKADDR_IN6>() as i32,
            )
        }
    }
}
