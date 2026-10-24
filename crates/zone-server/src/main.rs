//! Zone server. One process hosts several zones (or layers of one zone),
//! each on its own thread and UDP port. Network I/O runs every millisecond,
//! the world (AI, snapshots) ticks at 20 Hz, and combat timers fire the
//! moment they fall due, so queued spells never wait for a tick.

mod ai;
mod game;
mod interest;
mod metrics;
mod net;
mod svc;
mod transport;
mod zone;

// The tick, its rayon workers and the net thread all allocate per client
// per tick; glibc malloc contends badly under that.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use combat_core::{ContentPack, DEFAULT_PORT};
use renet_netcode::ServerAuthentication;
use services::{Env, TokenUser};

use net::{NetConfig, NetIn, NetOut};

use metrics::{Shared, TickWindow, ZoneMetrics};
use svc::{Envelope, Registry, SpawnCmd, SvcEvent, SvcReq, ZoneHandle};
use zone::{Handoff, PlayerInit, Zone};

const SNAPSHOT_MS: u64 = 50;
const HEARTBEAT_MS: u64 = 5_000;
const WRITE_BEHIND_MS: u64 = 60_000;
/// Players handed off per world tick while a layer drains.
const DRAIN_PER_TICK: usize = 10;

struct ZoneSpec {
    zone: String,
    layer: u16,
    port: u16,
}

struct Options {
    env: Env,
    secure: bool,
    zones: Vec<ZoneSpec>,
    metrics_port: u16,
    control_port: u16,
    ports: (u16, u16),
    max_clients: usize,
    public_ip: String,
    proc_name: String,
}

fn usage() -> ! {
    println!(
        "zone-server [--content tenants/default | game.pack] [--zone elderwood:5000[:layer]]...
             [--metrics 9100] [--max-clients 1024] [--public-ip 127.0.0.1]
  Production (signed tokens, persistence, layers):
             --db postgres://... [--redis redis://...] --netcode-key <64 hex> --internal-key <secret>
             [--world http://127.0.0.1:7001] [--control 7100] [--ports 5100-5199] [--name proc-1]
  Without --db the server runs in development mode: unsigned connects, nothing saved."
    );
    std::process::exit(0)
}

fn parse_args() -> Options {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
    }
    let mut env = Env::from_args(&mut args);
    let mut o = Options {
        secure: !env.db.is_empty(),
        env: env.clone(),
        zones: Vec::new(),
        metrics_port: 9100,
        control_port: 7100,
        ports: (5100, 5199),
        max_clients: 1024,
        public_ip: "127.0.0.1".into(),
        proc_name: String::new(),
    };
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        let mut v = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
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
            "--control" => o.control_port = v().parse().expect("--control port"),
            "--ports" => {
                let s = v();
                let (a, b) = s.split_once('-').expect("--ports first-last");
                o.ports = (a.parse().expect("port"), b.parse().expect("port"));
            }
            "--max-clients" => o.max_clients = v().parse().expect("--max-clients n"),
            "--public-ip" => o.public_ip = v(),
            "--name" => o.proc_name = v(),
            other => panic!("unknown argument {other}"),
        }
    }
    if std::env::var_os("MMO_CONTENT").is_none() && !std::path::Path::new(&env.content).exists() {
        env.content = find_content();
    }
    o.env.content = env.content;
    if o.proc_name.is_empty() {
        o.proc_name = format!("zone-{}-{}", o.public_ip, o.control_port);
    }
    o
}

/// Shared by the main thread (which starts layers) and every zone thread.
#[derive(Clone)]
struct Process {
    pack: Arc<ContentPack>,
    shared: Shared,
    registry: Registry,
    svc: Option<tokio::sync::mpsc::UnboundedSender<Envelope>>,
    netcode_key: Option<[u8; 32]>,
    max_clients: usize,
    public_ip: String,
}

fn main() {
    let opts = parse_args();
    let pack = ContentPack::load(&opts.env.content)
        .unwrap_or_else(|e| panic!("invalid content pack {}: {e}", opts.env.content));
    println!(
        "content: {} \"{}\" ({} zones, {} mobs, {} abilities, {} items, hash {:016x})",
        opts.env.content,
        pack.tenant.name,
        pack.zones.len(),
        pack.mobs.len(),
        pack.abilities.len(),
        pack.items.len(),
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
    let shared: Shared = Arc::new(Mutex::new(Vec::new()));
    metrics::serve(opts.metrics_port, shared.clone());
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let (spawn_tx, spawn_rx) = std::sync::mpsc::channel::<SpawnCmd>();
    let svc = if opts.secure {
        println!(
            "production mode: signed connect tokens, Postgres persistence, world at {}",
            opts.env.world_url
        );
        Some(svc::start(
            opts.env.clone(),
            pack.clone(),
            registry.clone(),
            opts.proc_name.clone(),
            opts.control_port,
            opts.public_ip.clone(),
            spawn_tx,
        ))
    } else {
        println!("development mode: unsigned connects, nothing is saved");
        drop(spawn_tx);
        None
    };
    let proc = Process {
        pack: pack.clone(),
        shared,
        registry,
        svc,
        netcode_key: opts.secure.then_some(opts.env.netcode_key),
        max_clients: opts.max_clients,
        public_ip: opts.public_ip.clone(),
    };
    let mut used_ports: Vec<u16> = Vec::new();
    for spec in zones {
        used_ports.push(spec.port);
        start_zone(&proc, spec, true).expect("starting zone");
    }
    // Serve layer requests from the world service until the process ends.
    while let Ok(cmd) = spawn_rx.recv() {
        let in_use: Vec<u16> = {
            let reg = proc.registry.lock().unwrap();
            reg.values()
                .filter_map(|z| z.addr.rsplit(':').next()?.parse().ok())
                .collect()
        };
        let port =
            (opts.ports.0..=opts.ports.1).find(|p| !in_use.contains(p) && !used_ports.contains(p));
        let result = match (port, proc.pack.zone_index(&cmd.zone)) {
            (_, None) => Err(format!("unknown zone {}", cmd.zone)),
            (None, _) => Err("no free ports".to_string()),
            (Some(port), Some(_)) => start_zone(
                &proc,
                ZoneSpec {
                    zone: cmd.zone.clone(),
                    layer: cmd.layer,
                    port,
                },
                false,
            ),
        };
        let _ = cmd.reply.send(result);
    }
    // Development mode has no control endpoint: just keep running.
    loop {
        std::thread::park();
    }
}

/// Start one zone layer on its own threads. Returns its public address.
fn start_zone(proc: &Process, spec: ZoneSpec, permanent: bool) -> Result<String, String> {
    let zone_idx = proc
        .pack
        .zone_index(&spec.zone)
        .ok_or_else(|| format!("unknown zone '{}'", spec.zone))?;
    let bind: SocketAddr = format!("0.0.0.0:{}", spec.port).parse().unwrap();
    let public: SocketAddr = format!("{}:{}", proc.public_ip, spec.port)
        .parse()
        .map_err(|e| format!("public address: {e}"))?;
    let socket = std::net::UdpSocket::bind(bind).map_err(|e| format!("binding UDP {bind}: {e}"))?;
    let (to_sim, from_net) = crossbeam_channel::unbounded::<NetIn>();
    let (to_net, from_sim) = crossbeam_channel::unbounded::<NetOut>();
    let slot = {
        let mut m = proc.shared.lock().unwrap();
        m.push(ZoneMetrics {
            zone: spec.zone.clone(),
            layer: spec.layer,
            port: spec.port,
            ..Default::default()
        });
        m.len() - 1
    };
    let handle = Arc::new(ZoneHandle {
        zone: spec.zone.clone(),
        layer: spec.layer,
        addr: public.to_string(),
        to_sim: to_sim.clone(),
        permanent,
    });
    proc.registry
        .lock()
        .unwrap()
        .insert((spec.zone.clone(), spec.layer), handle.clone());
    let auth = match proc.netcode_key {
        Some(private_key) => ServerAuthentication::Secure { private_key },
        None => ServerAuthentication::Unsecure,
    };
    let alive = net::spawn(
        NetConfig {
            public,
            max_clients: proc.max_clients,
            auth,
            slot,
        },
        socket,
        to_sim,
        from_sim,
        proc.shared.clone(),
        format!("{}-{}", spec.zone, spec.layer),
    );
    let proc2 = proc.clone();
    std::thread::Builder::new()
        .name(format!("zone-{}-{}", spec.zone, spec.layer))
        .spawn(move || {
            run_zone(
                proc2.clone(),
                zone_idx,
                spec,
                slot,
                handle.clone(),
                from_net,
                to_net,
            );
            alive.store(false, std::sync::atomic::Ordering::Relaxed);
            proc2
                .registry
                .lock()
                .unwrap()
                .remove(&(handle.zone.clone(), handle.layer));
            proc2.shared.lock().unwrap()[slot].closed = true;
        })
        .unwrap();
    Ok(public.to_string())
}

fn run_zone(
    proc: Process,
    zone_idx: u16,
    spec: ZoneSpec,
    slot: usize,
    handle: Arc<ZoneHandle>,
    from_net: crossbeam_channel::Receiver<NetIn>,
    to_net: crossbeam_channel::Sender<NetOut>,
) {
    let pack = proc.pack.clone();
    let shared = proc.shared.clone();
    let send_svc = |req: SvcReq| {
        if let Some(s) = &proc.svc {
            let _ = s.send(Envelope {
                zone: handle.clone(),
                req,
            });
        }
    };

    let start = Instant::now();
    let mut zone = Zone::new(pack.clone(), zone_idx, spec.layer, 0);
    println!(
        "zone {} layer {} listening on UDP {} ({} mobs)",
        spec.zone,
        spec.layer,
        handle.addr,
        zone.entities.len()
    );
    // Connected with a valid token, character still loading.
    let mut pending: HashMap<renet::ClientId, TokenUser> = HashMap::new();
    let mut next_snapshot = 0u64;
    let mut ticks = TickWindow::default();
    // Inputs, AI, snapshot build: where the tick time goes.
    let mut phases: [TickWindow; 3] = Default::default();
    let mut next_report = 10_000u64;
    let mut next_heartbeat = 0u64;
    let mut next_save = WRITE_BEHIND_MS;
    let mut empty_since: Option<u64> = None;

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
                    if proc.svc.is_some() {
                        match user_data.as_ref().and_then(TokenUser::from_bytes) {
                            Some(user) => {
                                pending.insert(client, user);
                                send_svc(SvcReq::Load { client, user });
                            }
                            None => zone.kicks.push(client),
                        }
                    } else {
                        let name = user_data
                            .map(|d| name_from_user_data(&d))
                            .unwrap_or_default();
                        zone.add_player(client, name, 0, 1, None, now);
                    }
                }
                NetIn::Disconnected { client } => {
                    if pending.remove(&client).is_some() {
                        continue;
                    }
                    if let Some(&id) = zone.clients.get(&client) {
                        let handed_off = zone.entities[&id]
                            .player
                            .as_ref()
                            .is_some_and(|p| matches!(p.handoff, Handoff::Sent { .. }));
                        // After a handoff everything was saved already, and the
                        // next zone owns the character now.
                        if !handed_off && let Some(record) = zone.char_record(id) {
                            send_svc(SvcReq::Logout { record });
                        }
                    }
                    zone.remove_player(client);
                }
                NetIn::Move { client, input } => zone.handle_move(client, input, now),
                NetIn::Msg {
                    client,
                    msg,
                    rtt_ms,
                } => zone.handle(client, msg, now, now_us, rtt_ms),
                NetIn::Svc(ev) => match ev {
                    SvcEvent::Loaded { client, result } => {
                        let Some(user) = pending.remove(&client) else {
                            continue; // left while loading
                        };
                        match result {
                            Ok((rec, party)) => {
                                // A second login of the same character replaces the first.
                                if let Some(&old) = zone.by_character.get(&user.character)
                                    && let Some(c) = zone.entities[&old].client()
                                {
                                    zone.remove_player(c);
                                    zone.kicks.push(c);
                                }
                                let class = pack.class_index(&rec.class).unwrap_or(0);
                                let bag = pack.tenant.bag_slots as usize;
                                let in_zone = rec.zone == spec.zone;
                                let init = PlayerInit {
                                    name: rec.name.clone(),
                                    class,
                                    level: rec.level.max(1) as u32,
                                    pos: in_zone.then_some(rec.pos),
                                    character: rec.id,
                                    account: rec.account_id,
                                    xp: rec.xp.max(0) as u32,
                                    hp: Some(rec.hp),
                                    mana: Some(rec.mana),
                                    inv: Some(game::Inventory::from_rows(&pack, &rec.items, bag)),
                                    party,
                                };
                                let id = zone.add_player_with(client, init, now);
                                if !in_zone {
                                    // Saved somewhere else (stale token): it lives here now.
                                    zone.save_progress(id);
                                }
                            }
                            Err(e) => {
                                eprintln!("client {client}: {e}");
                                zone.kicks.push(client);
                            }
                        }
                    }
                    SvcEvent::Placed { client, result } => zone.on_placed(client, result, now),
                    SvcEvent::Bus(msg) => zone.on_bus(msg),
                    SvcEvent::Drain => {
                        if !zone.draining {
                            println!(
                                "[{} L{}] draining: merging players into other layers",
                                spec.zone, spec.layer
                            );
                        }
                        zone.draining = true;
                    }
                },
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
            housekeeping(&mut zone, now, spec.layer);
        }
        if world_tick || !zone.outbox.is_empty() {
            let msgs = std::mem::take(&mut zone.outbox);
            let _ = to_net.send(NetOut::Batch { msgs, flush: true });
        }
        for c in zone.kicks.drain(..) {
            let _ = to_net.send(NetOut::Kick(c));
        }
        for req in zone.svc.drain(..) {
            send_svc(req);
        }
        if world_tick {
            ticks.push(iter_start.elapsed().as_micros() as u32);
            phases[0].push(t_in);
            phases[1].push(t_ai);
            phases[2].push(t_snap);
        }

        if proc.svc.is_some() {
            if now >= next_save {
                next_save = now + WRITE_BEHIND_MS;
                zone.save_dirty();
            }
            if now >= next_heartbeat {
                next_heartbeat = now + HEARTBEAT_MS;
                let players: Vec<(i64, String)> = zone
                    .entities
                    .values()
                    .filter_map(|e| {
                        let p = e.player.as_ref()?;
                        (p.character != 0 && p.handoff == Handoff::None)
                            .then(|| (p.character, e.name.clone()))
                    })
                    .collect();
                send_svc(SvcReq::Heartbeat {
                    count: zone.player_count() + pending.len(),
                    players,
                    draining: zone.draining,
                });
            }
            for req in zone.svc.drain(..) {
                send_svc(req);
            }
            // A drained layer closes once it has been empty for a moment.
            if zone.draining && zone.player_count() == 0 && pending.is_empty() {
                let since = *empty_since.get_or_insert(now);
                if now - since > 3000 && !handle.permanent {
                    println!("[{} L{}] merged away, closing", spec.zone, spec.layer);
                    send_svc(SvcReq::Closed);
                    return;
                }
            } else {
                empty_since = None;
            }
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

/// Once per world tick: kick clients that ignored a handoff, and move
/// players off a draining layer a few at a time.
fn housekeeping(zone: &mut Zone, now: u64, layer: u16) {
    let mut late = Vec::new();
    let mut to_move = Vec::new();
    for e in zone.entities.values() {
        let Some(p) = e.player.as_ref() else {
            continue;
        };
        match &p.handoff {
            Handoff::Sent { kick_at, .. } if *kick_at <= now => late.push(p.client_id),
            Handoff::None
                if zone.draining && p.character != 0 && to_move.len() < DRAIN_PER_TICK =>
            {
                to_move.push((e.id, e.pos))
            }
            _ => {}
        }
    }
    zone.kicks.extend(late);
    let zone_id = zone.zone_def().id.clone();
    for (id, pos) in to_move {
        zone.begin_handoff(id, zone_id.clone(), pos, Some(layer));
    }
}

/// Unsecure (dev) connections carry the player name in the user data.
fn name_from_user_data(d: &[u8; 256]) -> String {
    let end = d.iter().position(|b| *b == 0).unwrap_or(32).min(32);
    String::from_utf8_lossy(&d[..end]).to_string()
}

fn find_content() -> String {
    for p in [
        "tenants/default",
        "../tenants/default",
        "../../tenants/default",
    ] {
        if std::path::Path::new(p).is_dir() {
            return p.to_string();
        }
    }
    panic!("could not find tenants/default; pass --content <dir or .pack>");
}
