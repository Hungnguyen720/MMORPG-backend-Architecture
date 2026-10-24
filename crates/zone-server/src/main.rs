//! Zone server. One process hosts several zones (or layers of one zone),
//! each on its own thread and UDP port. Network I/O runs every millisecond,
//! the world (AI, snapshots) ticks at 20 Hz, and combat timers fire the
//! moment they fall due, so queued spells never wait for a tick.

mod ai;
mod interest;
mod metrics;
mod net;
mod transport;
mod zone;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use combat_core::{ContentPack, DEFAULT_PORT};
use renet_netcode::ServerAuthentication;

use net::{NetConfig, NetIn, NetOut};

use metrics::{Shared, TickWindow, ZoneMetrics};
use zone::Zone;

const SNAPSHOT_MS: u64 = 50;

struct ZoneSpec {
    zone: String,
    layer: u16,
    port: u16,
}

struct Options {
    content: String,
    zones: Vec<ZoneSpec>,
    metrics_port: u16,
    max_clients: usize,
    public_ip: String,
}

fn usage() -> ! {
    println!(
        "zone-server [--content tenants/default] [--zone elderwood:5000[:layer]]... \\
             [--metrics 9100] [--max-clients 1024] [--public-ip 127.0.0.1]"
    );
    std::process::exit(0)
}

fn parse_args() -> Options {
    let mut o = Options {
        content: String::new(),
        zones: Vec::new(),
        metrics_port: 9100,
        max_clients: 1024,
        public_ip: "127.0.0.1".into(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--content" => o.content = v(),
            "--zone" => {
                let s = v();
                let parts: Vec<&str> = s.split(':').collect();
                o.zones.push(ZoneSpec {
                    zone: parts[0].to_string(),
                    port: parts
                        .get(1)
                        .map(|p| p.parse().expect("--zone id:port"))
                        .unwrap_or(DEFAULT_PORT),
                    layer: parts.get(2).map(|l| l.parse().expect("layer")).unwrap_or(1),
                });
            }
            "--metrics" => o.metrics_port = v().parse().expect("--metrics port"),
            "--max-clients" => o.max_clients = v().parse().expect("--max-clients n"),
            "--public-ip" => o.public_ip = v(),
            "-h" | "--help" => usage(),
            other => panic!("unknown argument {other}"),
        }
    }
    if o.content.is_empty() {
        o.content = find_content();
    }
    o
}

fn main() {
    let opts = parse_args();
    let pack = ContentPack::load(&opts.content)
        .unwrap_or_else(|e| panic!("invalid content pack {}: {e}", opts.content));
    println!(
        "content: {} \"{}\" ({} zones, {} mobs, {} abilities, hash {:016x})",
        opts.content,
        pack.tenant.name,
        pack.zones.len(),
        pack.mobs.len(),
        pack.abilities.len(),
        pack.hash
    );
    let pack = Arc::new(pack);
    let mut zones = opts.zones;
    if zones.is_empty() {
        zones.push(ZoneSpec {
            zone: pack.tenant.start_zone.clone(),
            layer: 1,
            port: DEFAULT_PORT,
        });
    }
    let shared: Shared = Arc::new(Mutex::new(vec![ZoneMetrics::default(); zones.len()]));
    metrics::serve(opts.metrics_port, shared.clone());

    let mut handles = Vec::new();
    for (slot, spec) in zones.into_iter().enumerate() {
        let zone_idx = pack
            .zone_index(&spec.zone)
            .unwrap_or_else(|| panic!("unknown zone '{}'", spec.zone));
        let pack = pack.clone();
        let shared = shared.clone();
        let max_clients = opts.max_clients;
        let public_ip = opts.public_ip.clone();
        handles.push(
            std::thread::Builder::new()
                .name(format!("zone-{}-{}", spec.zone, spec.layer))
                .spawn(move || {
                    run_zone(pack, zone_idx, spec, slot, shared, max_clients, &public_ip)
                })
                .unwrap(),
        );
    }
    for h in handles {
        let _ = h.join();
    }
}

fn run_zone(
    pack: Arc<ContentPack>,
    zone_idx: u16,
    spec: ZoneSpec,
    slot: usize,
    shared: Shared,
    max_clients: usize,
    public_ip: &str,
) {
    let bind: SocketAddr = format!("0.0.0.0:{}", spec.port).parse().unwrap();
    let public: SocketAddr = format!("{public_ip}:{}", spec.port).parse().unwrap();
    let (to_sim, from_net) = crossbeam_channel::unbounded::<NetIn>();
    let (to_net, from_sim) = crossbeam_channel::unbounded::<NetOut>();
    let _alive = net::spawn(
        NetConfig {
            bind,
            public,
            max_clients,
            // Phase 2 switches to signed connect tokens from the login service.
            auth: ServerAuthentication::Unsecure,
            slot,
        },
        to_sim,
        from_sim,
        shared.clone(),
        format!("{}-{}", spec.zone, spec.layer),
    );

    let start = Instant::now();
    let mut zone = Zone::new(pack.clone(), zone_idx, spec.layer, 0);
    println!(
        "zone {} layer {} listening on UDP {bind} ({} mobs)",
        spec.zone,
        spec.layer,
        zone.entities.len()
    );
    let mut next_snapshot = 0u64;
    let mut ticks = TickWindow::default();
    // Inputs, AI, snapshot build: where the tick time goes.
    let mut phases: [TickWindow; 3] = Default::default();
    let mut next_report = 10_000u64;

    loop {
        // Sleep until the next combat deadline or world tick, waking at once
        // when the network thread hands us input.
        let now_ms = start.elapsed().as_micros() as u64 / 1000;
        let deadline = zone.next_deadline().unwrap_or(u64::MAX).min(next_snapshot);
        let wait = Duration::from_millis(deadline.saturating_sub(now_ms));
        let first = if wait.is_zero() {
            from_net.try_recv().ok()
        } else {
            match from_net.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        };
        let iter_start = Instant::now();
        let now_us = (iter_start - start).as_micros() as u64;
        let now = now_us / 1000;
        for ev in first.into_iter().chain(from_net.try_iter()) {
            match ev {
                NetIn::Connected { client, user_data } => {
                    let name = user_data.map(|d| name_from_user_data(&d)).unwrap_or_default();
                    zone.add_player(client, name, 0, 1, None, now);
                }
                NetIn::Disconnected { client } => {
                    zone.remove_player(client);
                }
                NetIn::Move { client, input } => zone.handle_move(client, input, now),
                NetIn::Msg { client, msg, rtt_ms } => zone.handle(client, msg, now, now_us, rtt_ms),
            }
        }
        let t_in = iter_start.elapsed().as_micros() as u32;

        zone.advance(now, now_us);
        let world_tick = now >= next_snapshot;
        let (mut t_ai, mut t_snap) = (0u32, 0u32);
        if world_tick {
            let a = Instant::now();
            zone.tick_ai(now, now_us);
            t_ai = a.elapsed().as_micros() as u32;
            let a = Instant::now();
            zone.snapshot(now);
            t_snap = a.elapsed().as_micros() as u32;
            next_snapshot = now - now % SNAPSHOT_MS + SNAPSHOT_MS;
        }
        if world_tick || !zone.outbox.is_empty() {
            let msgs = std::mem::take(&mut zone.outbox);
            let _ = to_net.send(NetOut::Batch { msgs, flush: true });
        }
        for c in zone.kicks.drain(..) {
            let _ = to_net.send(NetOut::Kick(c));
        }
        if world_tick {
            ticks.push(iter_start.elapsed().as_micros() as u32);
            phases[0].push(t_in);
            phases[1].push(t_ai);
            phases[2].push(t_snap);
        }

        if now >= next_report {
            next_report = now + 10_000;
            let (p50, p99, max, n) = ticks.take();
            let nc = zone.player_count().max(1) as f64;
            let view_avg = zone.nets.values().map(|n| n.last_view).sum::<usize>() as f64 / nc;
            let mut all = shared.lock().unwrap();
            let m = &mut all[slot];
            m.zone = spec.zone.clone();
            m.layer = spec.layer;
            m.port = spec.port;
            m.players = zone.player_count();
            m.entities = zone.entities.len();
            m.tick_p50 = p50;
            m.tick_p99 = p99;
            m.tick_max = max;
            m.ticks = n;
            m.view_avg = view_avg;
            m.queue_casts = zone.stats.count;
            m.queue_gap_max_ms = zone.stats.max_us as f64 / 1000.0;
            m.movement_corrections = zone.corrections;
            println!(
                "[{} L{}] players {:>4}  entities {:>4}  tick p50 {:.2} ms  p99 {:.2} ms  max {:.2} ms  net loop p99 {:.2} ms  out/client {:.0} B/s (max {:.0})  in {:.0} B/s  view {:.0}  queue gap max {:.2} ms",
                m.zone,
                m.layer,
                m.players,
                m.entities,
                m.tick_p50,
                m.tick_p99,
                m.tick_max,
                m.net_loop_p99,
                m.bytes_out_avg,
                m.bytes_out_max,
                m.bytes_in_avg,
                m.view_avg,
                m.queue_gap_max_ms
            );
            drop(all);
            if std::env::var_os("ZONE_PHASES").is_some() {
                let [a, b, c] = &mut phases;
                let (a, b, c) = (a.take(), b.take(), c.take());
                println!(
                    "    phases p50/p99 ms: input {:.2}/{:.2}  ai {:.2}/{:.2}  snapshot {:.2}/{:.2}",
                    a.0, a.1, b.0, b.1, c.0, c.1
                );
            } else {
                for p in phases.iter_mut() {
                    p.take();
                }
            }
            zone.stats = Default::default();
        }
    }
}

/// Unsecure (dev) connections carry the player name in the user data.
fn name_from_user_data(d: &[u8; 256]) -> String {
    let end = d.iter().position(|b| *b == 0).unwrap_or(32).min(32);
    String::from_utf8_lossy(&d[..end]).to_string()
}

fn find_content() -> String {
    for p in ["tenants/default", "../tenants/default", "../../tenants/default"] {
        if std::path::Path::new(p).is_dir() {
            return p.to_string();
        }
    }
    panic!("could not find tenants/default; pass --content <dir or .pack>");
}
