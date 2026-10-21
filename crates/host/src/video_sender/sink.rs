//! Where video datagrams go: a trait seam plus the real non-blocking UDP socket.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Default kernel send buffer requested for the video socket (the kernel may clamp it to
/// `net.core.wmem_max`).
pub const DEFAULT_SEND_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// DSCP class marked on video datagrams. Wi-Fi WMM maps both AF41 and EF to the video queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dscp {
    /// Assured Forwarding 41, the usual class for interactive video.
    Af41,
    /// Expedited Forwarding.
    Ef,
    /// Leave the default marking.
    Off,
}

impl Dscp {
    /// The IPv4 TOS / IPv6 traffic-class byte (DSCP in the upper six bits).
    pub fn tos(self) -> u8 {
        match self {
            Dscp::Af41 => 34 << 2,
            Dscp::Ef => 46 << 2,
            Dscp::Off => 0,
        }
    }
}

/// Destination for finished datagrams. `send` must not block: if the socket cannot take the
/// datagram right now it returns `WouldBlock` and the datagram is dropped, because a late video
/// packet is worth less than no packet and waiting would add latency to every later one.
pub trait DatagramSink: Send + 'static {
    fn send(&mut self, datagram: &[u8], dest: SocketAddr) -> io::Result<()>;
}

/// Keeps the existing UDP path and sends datagrams over an active negotiated USB tunnel when
/// the link tracker selects its loopback peer.
pub struct HybridSink {
    udp: UdpSink,
    tunnel: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
}

impl HybridSink {
    pub fn new(
        tunnel: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
        send_buffer: usize,
        dscp: Dscp,
    ) -> io::Result<Self> {
        Ok(Self {
            udp: UdpSink::new(send_buffer, dscp)?,
            tunnel,
        })
    }
}

impl DatagramSink for HybridSink {
    fn send(&mut self, datagram: &[u8], dest: SocketAddr) -> io::Result<()> {
        if dest.ip().is_loopback() {
            return self
                .tunnel
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "USB tunnel is unavailable")
                })?
                .send(datagram.to_vec())
                .map(|_| ())
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "USB tunnel has no active client")
                });
        }
        self.udp.send(datagram, dest)
    }
}

/// Non-blocking UDP sender with a large send buffer and a low-latency DSCP mark.
/// The socket is unbound (the kernel picks an ephemeral source port on first send); the client
/// filters by source IP only.
pub struct UdpSink {
    v4: Socket,
    v6: Option<Socket>,
    send_buffer: usize,
    dscp: Dscp,
}

impl UdpSink {
    /// Creates the IPv4 socket. Buffer size and DSCP are best effort and never fail the call.
    pub fn new(send_buffer: usize, dscp: Dscp) -> io::Result<Self> {
        let v4 = new_socket(Domain::IPV4, send_buffer, dscp)?;
        Ok(Self {
            v4,
            v6: None,
            send_buffer,
            dscp,
        })
    }

    /// The send buffer the kernel actually granted for the IPv4 socket (Linux reports double
    /// the requested value for bookkeeping), or `None` if it cannot be read.
    pub fn granted_send_buffer(&self) -> Option<usize> {
        self.v4.send_buffer_size().ok()
    }
}

fn new_socket(domain: Domain, send_buffer: usize, dscp: Dscp) -> io::Result<Socket> {
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    // Best effort: an unprivileged process may be clamped, and some networks strip the mark.
    let _ = socket.set_send_buffer_size(send_buffer);
    if dscp != Dscp::Off {
        if domain == Domain::IPV4 {
            let _ = socket.set_tos_v4(u32::from(dscp.tos()));
        } else {
            set_tclass_v6(&socket, dscp.tos());
        }
    }
    Ok(socket)
}

/// socket2 has no IPv6 traffic class setter in this version, so set it directly. Best effort.
fn set_tclass_v6(socket: &Socket, tclass: u8) {
    let value = libc::c_int::from(tclass);
    // SAFETY: the fd is valid for the lifetime of `socket`, and `value` is a live c_int whose
    // size is passed alongside it.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_TCLASS,
            std::ptr::addr_of!(value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

impl DatagramSink for UdpSink {
    fn send(&mut self, datagram: &[u8], dest: SocketAddr) -> io::Result<()> {
        // A dual-stack control listener reports IPv4 peers as `::ffff:a.b.c.d`; send those over v4.
        let dest = match dest.ip().to_canonical() {
            IpAddr::V4(v4) => SocketAddr::new(IpAddr::V4(v4), dest.port()),
            IpAddr::V6(_) => dest,
        };
        let socket = if dest.is_ipv4() {
            &self.v4
        } else {
            if self.v6.is_none() {
                self.v6 = Some(new_socket(Domain::IPV6, self.send_buffer, self.dscp)?);
            }
            self.v6.as_ref().expect("created above")
        };
        socket.send_to(datagram, &SockAddr::from(dest)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dscp_bytes() {
        assert_eq!(Dscp::Af41.tos(), 0x88);
        assert_eq!(Dscp::Ef.tos(), 0xB8);
        assert_eq!(Dscp::Off.tos(), 0);
    }

    #[test]
    fn sends_to_loopback_and_marks_the_socket() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut sink = UdpSink::new(DEFAULT_SEND_BUFFER_BYTES, Dscp::Af41).unwrap();
        sink.send(b"hello", rx.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        // The buffer request is best effort, but Linux always grants at least a few KiB.
        assert!(sink.granted_send_buffer().unwrap_or(0) >= 4096);
        // IP_TOS reads back what was set (best effort: skip if the platform refuses).
        if let Ok(tos) = sink.v4.tos_v4() {
            assert_eq!(tos, u32::from(Dscp::Af41.tos()));
        }
    }

    #[test]
    fn mapped_ipv4_destination_uses_the_v4_socket() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let port = rx.local_addr().unwrap().port();
        let mut sink = UdpSink::new(65536, Dscp::Off).unwrap();
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        sink.send(b"mapped", SocketAddr::new(mapped, port)).unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"mapped");
        assert!(sink.v6.is_none(), "no IPv6 socket needed");
    }
}
