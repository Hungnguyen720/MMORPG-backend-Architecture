//! renet's netcode client transport, plus a fake-lag stage below it. Delay
//! and loss are applied to raw UDP packets in both directions, so renet's
//! reliability, acks and RTT estimate all see a genuinely bad connection.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use renet::RenetClient;
use renetcode::{ClientAuthentication, NETCODE_MAX_PACKET_BYTES, NetcodeClient};

pub struct LagTransport {
    socket: UdpSocket,
    netcode: NetcodeClient,
    buffer: [u8; NETCODE_MAX_PACKET_BYTES],
    outgoing: VecDeque<(Instant, Vec<u8>, SocketAddr)>,
    incoming: VecDeque<(Instant, Vec<u8>)>,
    /// Added round-trip time; half is applied in each direction.
    pub latency_ms: u32,
    /// Packet loss probability per packet, per direction, 0..1.
    pub loss: f32,
    rng: u64,
}

impl LagTransport {
    pub fn new(
        current_time: Duration,
        auth: ClientAuthentication,
        socket: UdpSocket,
    ) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let netcode =
            NetcodeClient::new(current_time, auth).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self {
            socket,
            netcode,
            buffer: [0; NETCODE_MAX_PACKET_BYTES],
            outgoing: VecDeque::new(),
            incoming: VecDeque::new(),
            latency_ms: 0,
            loss: 0.0,
            rng: current_time.as_nanos() as u64 | 1,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.netcode.is_connected()
    }

    pub fn disconnect_reason(&self) -> Option<String> {
        self.netcode.disconnect_reason().map(|r| format!("{r:?}"))
    }

    fn one_way(&self) -> Duration {
        Duration::from_micros(self.latency_ms as u64 * 500)
    }

    fn dropped(&mut self) -> bool {
        if self.loss <= 0.0 {
            return false;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        ((self.rng >> 40) as f32 / (1u64 << 24) as f32) < self.loss
    }

    fn enqueue_out(&mut self, bytes: Vec<u8>, addr: SocketAddr) {
        if !self.dropped() {
            let at = Instant::now() + self.one_way();
            self.outgoing.push_back((at, bytes, addr));
        }
        self.flush_out();
    }

    fn flush_out(&mut self) {
        let now = Instant::now();
        while self.outgoing.front().is_some_and(|(at, _, _)| *at <= now) {
            let (_, bytes, addr) = self.outgoing.pop_front().unwrap();
            let _ = self.socket.send_to(&bytes, addr);
        }
    }

    pub fn update(&mut self, dt: Duration, client: &mut RenetClient) -> Result<(), String> {
        if let Some(reason) = self.netcode.disconnect_reason() {
            client.disconnect_due_to_transport();
            return Err(format!("disconnected: {reason:?}"));
        }
        if client.disconnect_reason().is_some() {
            if let Ok((addr, packet)) = self.netcode.disconnect() {
                let _ = self.socket.send_to(packet, addr);
            }
            return Err("renet disconnected".into());
        }
        if self.netcode.is_connected() {
            client.set_connected();
        } else if self.netcode.is_connecting() {
            client.set_connecting();
        }

        let server = self.netcode.server_addr();
        loop {
            match self.socket.recv_from(&mut self.buffer) {
                Ok((len, addr)) => {
                    if addr != server || self.dropped() {
                        continue;
                    }
                    let at = Instant::now() + self.one_way();
                    self.incoming.push_back((at, self.buffer[..len].to_vec()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
                // Windows reports ICMP port unreachable as a reset; keep polling.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        let now = Instant::now();
        while self.incoming.front().is_some_and(|(at, _)| *at <= now) {
            let (_, mut bytes) = self.incoming.pop_front().unwrap();
            if let Some(payload) = self.netcode.process_packet(&mut bytes) {
                client.process_packet(payload);
            }
        }

        if let Some((packet, addr)) = self.netcode.update(dt) {
            let bytes = packet.to_vec();
            self.enqueue_out(bytes, addr);
        }
        self.flush_out();
        Ok(())
    }

    pub fn send_packets(&mut self, client: &mut RenetClient) -> Result<(), String> {
        if !self.netcode.is_connected() {
            self.flush_out();
            return Ok(());
        }
        for packet in client.get_packets_to_send() {
            let (addr, payload) = self
                .netcode
                .generate_payload_packet(&packet)
                .map_err(|e| e.to_string())?;
            let bytes = payload.to_vec();
            self.enqueue_out(bytes, addr);
        }
        self.flush_out();
        Ok(())
    }

    pub fn disconnect(&mut self) {
        if let Ok((addr, packet)) = self.netcode.disconnect() {
            let _ = self.socket.send_to(packet, addr);
        }
    }
}
