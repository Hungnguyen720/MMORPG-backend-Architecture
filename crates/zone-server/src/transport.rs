//! renet's netcode server transport, with batched socket I/O. Same protocol
//! and encryption as `renet_netcode::NetcodeServerTransport`; the only change
//! is that outgoing packets are collected per flush and handed to the kernel
//! with one `sendmmsg` call instead of one `sendto` per packet, and incoming
//! ones are read with `recvmmsg`. At 1,000 clients that is the difference
//! between thousands of syscalls per tick and a handful.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use renet::{ClientId, RenetServer};
use renetcode::{
    NETCODE_MAX_PACKET_BYTES, NETCODE_USER_DATA_BYTES, NetcodeServer, ServerConfig, ServerResult,
};

const BATCH: usize = 256;

pub struct BatchedServerTransport {
    socket: UdpSocket,
    netcode: NetcodeServer,
    out: Vec<(SocketAddr, Vec<u8>)>,
    rx: Vec<[u8; NETCODE_MAX_PACKET_BYTES]>,
    /// Packets that failed to send (kernel buffer full and the like).
    pub send_errors: u64,
    /// Microseconds spent in the last flush's syscall, and in building it.
    pub last_syscall_us: u32,
    pub last_build_us: u32,
}

impl BatchedServerTransport {
    pub fn new(config: ServerConfig, socket: UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let s2 = socket2::SockRef::from(&socket);
        // A tick's worth of snapshots for 1,000 clients must fit.
        let _ = s2.set_send_buffer_size(8 << 20);
        let _ = s2.set_recv_buffer_size(8 << 20);
        Ok(Self {
            socket,
            netcode: NetcodeServer::new(config),
            out: Vec::with_capacity(4096),
            rx: vec![[0u8; NETCODE_MAX_PACKET_BYTES]; BATCH],
            send_errors: 0,
            last_syscall_us: 0,
            last_build_us: 0,
        })
    }

    pub fn user_data(&self, client_id: ClientId) -> Option<[u8; NETCODE_USER_DATA_BYTES]> {
        self.netcode.user_data(client_id)
    }

    /// Receive everything waiting on the socket and advance netcode timers.
    pub fn update(&mut self, dt: Duration, server: &mut RenetServer) -> io::Result<()> {
        self.netcode.update(dt);
        loop {
            let batch = recv_batch(&self.socket, &mut self.rx)?;
            if batch.is_empty() {
                break;
            }
            let full = batch.len() == BATCH;
            for (i, (len, addr)) in batch.into_iter().enumerate() {
                // Copy out so `self` can be borrowed mutably by `handle`.
                let mut buf = self.rx[i];
                let result = self.netcode.process_packet(addr, &mut buf[..len]);
                let result = detach(result);
                self.handle_detached(result, server);
            }
            if !full {
                break;
            }
        }
        for client_id in self.netcode.clients_id() {
            let r = detach(self.netcode.update_client(client_id));
            self.handle_detached(r, server);
        }
        for id in server.disconnections_id() {
            let r = detach(self.netcode.disconnect(id));
            self.handle_detached(r, server);
        }
        self.flush();
        Ok(())
    }

    fn handle_detached(&mut self, r: Detached, server: &mut RenetServer) {
        match r {
            Detached::None => {}
            Detached::Send(addr, bytes) => self.out.push((addr, bytes)),
            Detached::Payload(client_id, bytes) => {
                let _ = server.process_packet_from(&bytes, client_id);
            }
            Detached::Connected(client_id, addr, bytes) => {
                server.add_connection(client_id);
                self.out.push((addr, bytes));
            }
            Detached::Disconnected(client_id, addr, bytes) => {
                server.remove_connection(client_id);
                if let Some(b) = bytes {
                    self.out.push((addr, b));
                }
            }
        }
    }

    /// Encrypt and send every client's pending renet packets.
    pub fn send_packets(&mut self, server: &mut RenetServer) {
        let t = std::time::Instant::now();
        for client_id in server.clients_id() {
            let Ok(packets) = server.get_packets_to_send(client_id) else {
                continue;
            };
            for packet in packets {
                match self.netcode.generate_payload_packet(client_id, &packet) {
                    Ok((addr, payload)) => self.out.push((addr, payload.to_vec())),
                    Err(_) => break,
                }
            }
        }
        self.last_build_us = t.elapsed().as_micros() as u32;
        self.flush();
    }

    fn flush(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let t = std::time::Instant::now();
        self.send_errors += send_batch(&self.socket, &self.out);
        self.last_syscall_us = t.elapsed().as_micros() as u32;
        self.out.clear();
    }
}

/// `ServerResult` borrows the netcode server's buffers; this owns its bytes.
enum Detached {
    None,
    Send(SocketAddr, Vec<u8>),
    Payload(ClientId, Vec<u8>),
    Connected(ClientId, SocketAddr, Vec<u8>),
    Disconnected(ClientId, SocketAddr, Option<Vec<u8>>),
}

fn detach(r: ServerResult) -> Detached {
    match r {
        ServerResult::None => Detached::None,
        ServerResult::PacketToSend { payload, addr } => Detached::Send(addr, payload.to_vec()),
        ServerResult::Payload { client_id, payload } => {
            Detached::Payload(client_id, payload.to_vec())
        }
        ServerResult::ClientConnected {
            client_id,
            addr,
            payload,
            ..
        } => Detached::Connected(client_id, addr, payload.to_vec()),
        ServerResult::ClientDisconnected {
            client_id,
            addr,
            payload,
        } => Detached::Disconnected(client_id, addr, payload.map(|p| p.to_vec())),
    }
}

#[cfg(target_os = "linux")]
fn send_batch(socket: &UdpSocket, out: &[(SocketAddr, Vec<u8>)]) -> u64 {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let mut errors = 0;
    for chunk in out.chunks(BATCH) {
        let addrs: Vec<socket2::SockAddr> = chunk
            .iter()
            .map(|(a, _)| socket2::SockAddr::from(*a))
            .collect();
        let mut iovs: Vec<libc::iovec> = chunk
            .iter()
            .map(|(_, b)| libc::iovec {
                iov_base: b.as_ptr() as *mut libc::c_void,
                iov_len: b.len(),
            })
            .collect();
        let mut msgs: Vec<libc::mmsghdr> = (0..chunk.len())
            .map(|i| {
                // SAFETY: zeroed msghdr is a valid empty header.
                let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
                h.msg_name = addrs[i].as_ptr() as *mut libc::c_void;
                h.msg_namelen = addrs[i].len();
                h.msg_iov = &mut iovs[i];
                h.msg_iovlen = 1;
                libc::mmsghdr {
                    msg_hdr: h,
                    msg_len: 0,
                }
            })
            .collect();
        let mut sent = 0usize;
        while sent < msgs.len() {
            // SAFETY: msgs/iovs/addrs outlive the call; counts match.
            let n = unsafe {
                libc::sendmmsg(
                    fd,
                    msgs[sent..].as_mut_ptr(),
                    (msgs.len() - sent) as libc::c_uint,
                    0,
                )
            };
            if n <= 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Skip the packet that failed and carry on with the rest.
                errors += 1;
                sent += 1;
            } else {
                sent += n as usize;
            }
        }
    }
    errors
}

#[cfg(not(target_os = "linux"))]
fn send_batch(socket: &UdpSocket, out: &[(SocketAddr, Vec<u8>)]) -> u64 {
    let mut errors = 0;
    for (addr, bytes) in out {
        if socket.send_to(bytes, addr).is_err() {
            errors += 1;
        }
    }
    errors
}

#[cfg(target_os = "linux")]
fn recv_batch(
    socket: &UdpSocket,
    bufs: &mut [[u8; NETCODE_MAX_PACKET_BYTES]],
) -> io::Result<Vec<(usize, SocketAddr)>> {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let n = bufs.len();
    // SAFETY: sockaddr_storage is plain data; zeroed is valid.
    let mut names: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; n];
    let mut iovs: Vec<libc::iovec> = bufs
        .iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.as_mut_ptr() as *mut libc::c_void,
            iov_len: b.len(),
        })
        .collect();
    let mut msgs: Vec<libc::mmsghdr> = (0..n)
        .map(|i| {
            // SAFETY: zeroed msghdr is a valid empty header.
            let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
            h.msg_name = &mut names[i] as *mut _ as *mut libc::c_void;
            h.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as u32;
            h.msg_iov = &mut iovs[i];
            h.msg_iovlen = 1;
            libc::mmsghdr {
                msg_hdr: h,
                msg_len: 0,
            }
        })
        .collect();
    // SAFETY: all buffers live until the call returns.
    let got = unsafe {
        libc::recvmmsg(
            fd,
            msgs.as_mut_ptr(),
            n as libc::c_uint,
            libc::MSG_DONTWAIT,
            std::ptr::null_mut(),
        )
    };
    if got < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(Vec::new()),
            io::ErrorKind::ConnectionReset => Ok(Vec::new()),
            _ => Err(err),
        };
    }
    let mut out = Vec::with_capacity(got as usize);
    for i in 0..got as usize {
        // SAFETY: the kernel filled names[i] with msg_namelen bytes.
        let addr = unsafe { socket2::SockAddr::new(names[i], msgs[i].msg_hdr.msg_namelen) };
        if let Some(a) = addr.as_socket() {
            out.push((msgs[i].msg_len as usize, a));
        }
    }
    Ok(out)
}

#[cfg(not(target_os = "linux"))]
fn recv_batch(
    socket: &UdpSocket,
    bufs: &mut [[u8; NETCODE_MAX_PACKET_BYTES]],
) -> io::Result<Vec<(usize, SocketAddr)>> {
    let mut out = Vec::new();
    for b in bufs.iter_mut() {
        match socket.recv_from(b) {
            Ok((len, addr)) => out.push((len, addr)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}
