//! Phase 0 zone server: one flat arena, one training dummy, any number of
//! players. Network I/O and snapshots run at 20 Hz; combat timers are
//! processed the moment they fall due, so queued spells never wait for a tick.

mod zone;

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

use combat_core::protocol::{self, ClientMsg};
use combat_core::{ContentPack, DEFAULT_PORT, PROTOCOL_ID, channel, connection_config};
use renet::{RenetServer, ServerEvent};
use renet_netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig};

use zone::{Dest, Zone};

const SNAPSHOT_MS: u64 = 50;

fn main() {
    let mut port = DEFAULT_PORT;
    let mut content_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|p| p.parse().ok())
                    .expect("--port needs a number")
            }
            "--content" => content_path = args.next(),
            "-h" | "--help" => {
                println!("zone-server [--port 5000] [--content godot/content/abilities.yaml]");
                return;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let content_path = content_path.unwrap_or_else(find_content);
    let text = std::fs::read_to_string(&content_path)
        .unwrap_or_else(|e| panic!("reading {content_path}: {e}"));
    let pack = ContentPack::from_yaml(&text)
        .unwrap_or_else(|e| panic!("invalid content pack {content_path}: {e}"));
    println!(
        "content: {content_path} ({} abilities, hash {:016x})",
        pack.abilities.len(),
        pack.hash
    );

    let bind: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let socket = UdpSocket::bind(bind).unwrap_or_else(|e| panic!("binding UDP {bind}: {e}"));
    let unix_now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let mut transport = NetcodeServerTransport::new(
        ServerConfig {
            current_time: unix_now,
            max_clients: 64,
            protocol_id: PROTOCOL_ID,
            public_addresses: vec![bind],
            // Phase 0 only. Phase 2 switches to connect tokens from the login service.
            authentication: ServerAuthentication::Unsecure,
        },
        socket,
    )
    .expect("creating transport");
    let mut server = RenetServer::new(connection_config());
    println!("zone-server listening on UDP {bind}");

    let mut zone = Zone::new(pack);
    let start = Instant::now();
    let mut last = start;
    let mut next_snapshot = 0u64;

    loop {
        let now_instant = Instant::now();
        let dt = now_instant - last;
        last = now_instant;
        let now_us = (now_instant - start).as_micros() as u64;
        let now = now_us / 1000;

        server.update(dt);
        if let Err(e) = transport.update(dt, &mut server) {
            eprintln!("transport error: {e}");
        }

        while let Some(event) = server.get_event() {
            match event {
                ServerEvent::ClientConnected { client_id } => zone.add_player(client_id, now),
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    println!("client {client_id} disconnected: {reason}");
                    zone.remove_player(client_id);
                }
            }
        }

        for client_id in server.clients_id() {
            let rtt_ms = (server.rtt(client_id) * 1000.0) as u64;
            for ch in [channel::STATE, channel::COMBAT] {
                while let Some(bytes) = server.receive_message(client_id, ch) {
                    match protocol::decode::<ClientMsg>(&bytes) {
                        Some(msg) => zone.handle(client_id, msg, now, now_us, rtt_ms),
                        None => eprintln!("client {client_id}: undecodable message"),
                    }
                }
            }
        }

        zone.advance(now, now_us);
        if now >= next_snapshot {
            zone.snapshot(now);
            next_snapshot = now - now % SNAPSHOT_MS + SNAPSHOT_MS;
        }

        for (dest, ch, msg) in zone.drain_outbox() {
            let bytes = protocol::encode(&msg);
            match dest {
                Dest::All => server.broadcast_message(ch, bytes),
                Dest::One(id) => {
                    if server.is_connected(id) {
                        server.send_message(id, ch, bytes)
                    }
                }
            }
        }
        transport.send_packets(&mut server);

        // Wake for whichever comes first: the next combat deadline, the next
        // snapshot, or a 1 ms network poll.
        let elapsed_ms = start.elapsed().as_micros() as u64 / 1000;
        let deadline = zone.next_deadline().unwrap_or(u64::MAX).min(next_snapshot);
        let wait_ms = deadline.saturating_sub(elapsed_ms).min(1);
        if wait_ms > 0 {
            std::thread::sleep(Duration::from_micros(wait_ms * 1000 - 100));
        }
    }
}

fn find_content() -> String {
    for p in [
        "godot/content/abilities.yaml",
        "../godot/content/abilities.yaml",
        "../../godot/content/abilities.yaml",
    ] {
        if std::path::Path::new(p).exists() {
            return p.to_string();
        }
    }
    panic!("could not find godot/content/abilities.yaml; pass --content <path>");
}
