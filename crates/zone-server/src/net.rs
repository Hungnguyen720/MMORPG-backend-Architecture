//! The network thread of one zone. It owns the UDP socket, renet and the
//! netcode encryption, and talks to the simulation thread through two
//! channels. The world tick therefore never waits on syscalls or crypto,
//! and an incoming cast wakes the simulation at once instead of at its next
//! poll.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use combat_core::codec::MoveInput;
use combat_core::protocol::{self, ClientMsg};
use combat_core::{PROTOCOL_ID, channel, connection_config};
use crossbeam_channel::{Receiver, Sender};
use renet::{ClientId, RenetServer, ServerEvent};
use renet_netcode::{ServerAuthentication, ServerConfig};

use crate::metrics::{Shared, TickWindow};
use crate::transport::BatchedServerTransport;
use crate::zone::Outgoing;

pub enum NetIn {
    Connected {
        client: ClientId,
        user_data: Option<[u8; 256]>,
    },
    Disconnected {
        client: ClientId,
    },
    Move {
        client: ClientId,
        input: MoveInput,
    },
    Msg {
        client: ClientId,
        msg: ClientMsg,
        rtt_ms: u64,
    },
    /// An answer or event from the services thread.
    Svc(crate::svc::SvcEvent),
}

pub enum NetOut {
    /// Messages to queue; `flush` sends packets right away.
    Batch {
        msgs: Vec<Outgoing>,
        flush: bool,
    },
    Kick(ClientId),
}

pub struct NetConfig {
    pub public: SocketAddr,
    pub max_clients: usize,
    pub auth: ServerAuthentication,
    pub slot: usize,
}

/// Bind the socket and start the network thread.
pub fn spawn(
    cfg: NetConfig,
    socket: UdpSocket,
    to_sim: Sender<NetIn>,
    from_sim: Receiver<NetOut>,
    shared: Shared,
    name: String,
) -> Arc<AtomicBool> {
    let alive = Arc::new(AtomicBool::new(true));
    let alive2 = alive.clone();
    std::thread::Builder::new()
        .name(format!("net-{name}"))
        .spawn(move || run(cfg, socket, to_sim, from_sim, shared, alive2))
        .unwrap();
    alive
}

fn run(
    cfg: NetConfig,
    socket: UdpSocket,
    to_sim: Sender<NetIn>,
    from_sim: Receiver<NetOut>,
    shared: Shared,
    alive: Arc<AtomicBool>,
) {
    let unix_now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let mut transport = BatchedServerTransport::new(
        ServerConfig {
            current_time: unix_now,
            max_clients: cfg.max_clients,
            protocol_id: PROTOCOL_ID,
            public_addresses: vec![cfg.public],
            authentication: cfg.auth,
        },
        socket,
    )
    .expect("creating transport");
    let mut server = RenetServer::new(connection_config());
    let mut last = Instant::now();
    let mut busy = TickWindow::default();
    let mut next_report = Instant::now() + Duration::from_secs(10);

    while alive.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let dt = t0 - last;
        last = t0;
        server.update(dt);
        if let Err(e) = transport.update(dt, &mut server) {
            eprintln!("transport error: {e}");
        }
        while let Some(event) = server.get_event() {
            let ev = match event {
                ServerEvent::ClientConnected { client_id } => NetIn::Connected {
                    client: client_id,
                    user_data: transport.user_data(client_id),
                },
                ServerEvent::ClientDisconnected { client_id, .. } => {
                    NetIn::Disconnected { client: client_id }
                }
            };
            if to_sim.send(ev).is_err() {
                return;
            }
        }
        for client in server.clients_id() {
            while let Some(bytes) = server.receive_message(client, channel::STATE) {
                if let Some(input) = MoveInput::decode(&bytes) {
                    let _ = to_sim.send(NetIn::Move { client, input });
                }
            }
            let rtt_ms = (server.rtt(client) * 1000.0) as u64;
            for ch in [channel::COMBAT, channel::BULK] {
                while let Some(bytes) = server.receive_message(client, ch) {
                    match protocol::decode::<ClientMsg>(&bytes) {
                        Some(msg) => {
                            let _ = to_sim.send(NetIn::Msg {
                                client,
                                msg,
                                rtt_ms,
                            });
                        }
                        None => eprintln!("client {client}: undecodable message"),
                    }
                }
            }
        }
        let mut flush = false;
        let drain = |out: NetOut, server: &mut RenetServer, flush: &mut bool| match out {
            NetOut::Batch { msgs, flush: f } => {
                *flush |= f;
                for o in msgs {
                    if server.is_connected(o.client) {
                        server.send_message(o.client, o.channel, o.bytes);
                    }
                }
            }
            NetOut::Kick(c) => server.disconnect(c),
        };
        while let Ok(out) = from_sim.try_recv() {
            drain(out, &mut server, &mut flush);
        }
        // renet emits an ack packet on every flush, so flush only when the
        // simulation has news: a world tick or combat messages.
        if flush {
            transport.send_packets(&mut server);
        }
        busy.push(t0.elapsed().as_micros() as u32);

        if Instant::now() >= next_report {
            next_report += Duration::from_secs(10);
            let ids = server.clients_id();
            let (mut out_sum, mut out_max, mut in_sum) = (0.0f64, 0.0f64, 0.0f64);
            for &c in &ids {
                let o = server.bytes_sent_per_sec(c);
                out_sum += o;
                out_max = out_max.max(o);
                in_sum += server.bytes_received_per_sec(c);
            }
            let n = ids.len().max(1) as f64;
            let (_, p99, max, _) = busy.take();
            let mut m = shared.lock().unwrap();
            let m = &mut m[cfg.slot];
            m.bytes_out_avg = out_sum / n;
            m.bytes_out_max = out_max;
            m.bytes_in_avg = in_sum / n;
            m.net_loop_p99 = p99;
            m.net_loop_max = max;
            m.send_errors = transport.send_errors;
        }

        // Sleep until the simulation sends something, or 1 ms to poll the
        // socket again.
        match from_sim.recv_timeout(Duration::from_micros(900)) {
            Ok(out) => {
                let mut flush = false;
                drain(out, &mut server, &mut flush);
                while let Ok(out) = from_sim.try_recv() {
                    drain(out, &mut server, &mut flush);
                }
                if flush {
                    transport.send_packets(&mut server);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}
