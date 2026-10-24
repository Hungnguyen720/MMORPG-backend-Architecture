//! Load test: many headless bots in one process, each a full client with
//! its own UDP socket, wandering and fighting through the real protocol.
//! At the end it reads the zone server's metrics and checks the targets.
//!
//!   cargo run --release -p net-client --bin swarm -- --bots 200 --seconds 60
//!
//! Options:
//!   --server HOST:PORT      zone server (dev auth), default 127.0.0.1:5000
//!   --bots N                default 200
//!   --seconds S             measured run length after ramp-up, default 60
//!   --ramp-per-sec N        connections per second, default 100
//!   --threads T             bot threads, default 3
//!   --center X,Z            where bots roam, default the zone spawn
//!   --roam R                roam radius in metres, default 120
//!   --no-fight              bots only walk (movement-only load)
//!   --latency MS --loss PCT fake lag on every bot
//!   --metrics HOST:PORT     zone metrics endpoint, default 127.0.0.1:9100
//!   --max-tick-p99 MS       pass threshold, default 10
//!   --max-down-kbps KB      pass threshold per client, default 10
//!   --content DIR|PACK      default tenants/default
//!   --json PATH             also write the results as JSON

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use combat_core::ContentPack;
use net_client::brain::{BotStats, Brain};
use net_client::{Auth, ClientSession};

struct Opts {
    server: SocketAddr,
    bots: usize,
    seconds: u64,
    ramp: usize,
    threads: usize,
    center: Option<[f32; 2]>,
    roam: f32,
    fight: bool,
    latency: u32,
    loss: f32,
    metrics: String,
    max_tick_p99: f64,
    max_down_kbps: f64,
    content: String,
    json: Option<String>,
}

#[derive(Default)]
struct BotReport {
    stats: BotStats,
    connected: bool,
    bytes_in: u64,
    bytes_out: u64,
    packets_in: u64,
    rtt_ms: f64,
    snapshot_misses: u32,
    entities_seen: usize,
}

fn parse() -> Opts {
    let mut o = Opts {
        server: "127.0.0.1:5000".parse().unwrap(),
        bots: 200,
        seconds: 60,
        ramp: 100,
        threads: 3,
        center: None,
        roam: 120.0,
        fight: true,
        latency: 0,
        loss: 0.0,
        metrics: "127.0.0.1:9100".into(),
        max_tick_p99: 10.0,
        max_down_kbps: 10.0,
        content: "tenants/default".into(),
        json: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--server" => o.server = v().parse().expect("host:port"),
            "--bots" => o.bots = v().parse().unwrap(),
            "--seconds" => o.seconds = v().parse().unwrap(),
            "--ramp-per-sec" => o.ramp = v().parse().unwrap(),
            "--threads" => o.threads = v().parse().unwrap(),
            "--center" => {
                let s = v();
                let p: Vec<f32> = s.split(',').map(|x| x.parse().unwrap()).collect();
                o.center = Some([p[0], p[1]]);
            }
            "--roam" => o.roam = v().parse().unwrap(),
            "--no-fight" => o.fight = false,
            "--latency" => o.latency = v().parse().unwrap(),
            "--loss" => o.loss = v().parse::<f32>().unwrap() / 100.0,
            "--metrics" => o.metrics = v(),
            "--max-tick-p99" => o.max_tick_p99 = v().parse().unwrap(),
            "--max-down-kbps" => o.max_down_kbps = v().parse().unwrap(),
            "--content" => o.content = v(),
            "--json" => o.json = Some(v()),
            other => panic!("unknown argument {other}"),
        }
    }
    o
}

fn main() {
    let o = parse();
    let pack = ContentPack::load(&o.content).expect("content pack");
    let zone = pack.zone(&pack.tenant.start_zone).unwrap();
    let center = o
        .center
        .unwrap_or([zone.player_spawn[0], zone.player_spawn[2]]);
    println!(
        "swarm: {} bots -> {}, {} threads, roam {:.0} m around ({:.0}, {:.0}), fight {}, +{} ms / {:.1}% loss",
        o.bots, o.server, o.threads, o.roam, center[0], center[1], o.fight, o.latency, o.loss * 100.0
    );

    let threads = o.threads.max(1).min(o.bots.max(1));
    let total_ramp = Duration::from_secs_f64(o.bots as f64 / o.ramp.max(1) as f64);
    let measure = Duration::from_secs(o.seconds);
    let barrier = Arc::new(Barrier::new(threads + 1));
    let (tx, rx) = mpsc::channel::<Vec<BotReport>>();
    let started = Instant::now();
    for t in 0..threads {
        let tx = tx.clone();
        let barrier = barrier.clone();
        let pack = pack.clone();
        let mine: Vec<usize> = (0..o.bots).filter(|i| i % threads == t).collect();
        let (server, latency, loss, fight, roam, ramp) =
            (o.server, o.latency, o.loss, o.fight, o.roam, o.ramp.max(1));
        std::thread::spawn(move || {
            let mut bots: Vec<(ClientSession, Brain, Option<(u64, u64, u64)>)> = Vec::new();
            let mut pending = mine.into_iter().peekable();
            let begin = Instant::now();
            let mut measuring_from: Option<Instant> = None;
            loop {
                // Connect on schedule: bot i starts at i / ramp seconds.
                while let Some(&i) = pending.peek() {
                    if begin.elapsed().as_secs_f64() < i as f64 / ramp as f64 {
                        break;
                    }
                    pending.next();
                    let mut s = ClientSession::connect(
                        pack.clone(),
                        server,
                        Auth::Dev { name: format!("Bot{i}") },
                    )
                    .expect("socket");
                    s.set_fake_lag(latency, loss);
                    let mut b = Brain::new(
                        [center[0], 0.0, center[1]],
                        roam,
                        0x9E37_79B9 ^ (i as u64 * 7919),
                    );
                    b.fight = fight;
                    bots.push((s, b, None));
                }
                for (s, b, _) in bots.iter_mut() {
                    s.update();
                    b.tick(s);
                }
                if pending.peek().is_none() && measuring_from.is_none() {
                    // Everyone connected: wait for the other threads, then
                    // give connections 3 s to settle and start measuring.
                    barrier.wait();
                    let settle = Instant::now();
                    while settle.elapsed() < Duration::from_secs(3) {
                        for (s, b, _) in bots.iter_mut() {
                            s.update();
                            b.tick(s);
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    for (s, b, base) in bots.iter_mut() {
                        let (bi, bo, pi, _) = s.wire_counters();
                        *base = Some((bi, bo, pi));
                        b.stats = BotStats::default();
                    }
                    measuring_from = Some(Instant::now());
                    barrier.wait();
                }
                if let Some(m) = measuring_from
                    && m.elapsed() >= measure
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let reports = bots
                .iter_mut()
                .map(|(s, b, base)| {
                    let (bi, bo, pi, _) = s.wire_counters();
                    let (b0, o0, p0) = base.unwrap_or((0, 0, 0));
                    let r = BotReport {
                        stats: std::mem::take(&mut b.stats),
                        connected: s.is_connected(),
                        bytes_in: bi - b0,
                        bytes_out: bo - o0,
                        packets_in: pi - p0,
                        rtt_ms: s.rtt_ms(),
                        snapshot_misses: s.snapshot_misses,
                        entities_seen: s.entities.len(),
                    };
                    s.disconnect();
                    r
                })
                .collect();
            for (s, _, _) in bots.iter_mut() {
                s.update();
            }
            tx.send(reports).unwrap();
        });
    }
    drop(tx);
    barrier.wait();
    println!(
        "all {} bots connecting took {:.1} s (planned {:.1} s); settling 3 s",
        o.bots,
        started.elapsed().as_secs_f64(),
        total_ramp.as_secs_f64()
    );
    barrier.wait();
    println!("measuring for {} s ...", o.seconds);
    // Read the server's 10 s windows while the bots run.
    let mut server_samples = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < measure + Duration::from_millis(500) {
        std::thread::sleep(Duration::from_secs(10).min(measure.saturating_sub(t0.elapsed()) + Duration::from_millis(400)));
        if t0.elapsed() >= Duration::from_secs(9)
            && let Some(m) = fetch_metrics(&o.metrics)
        {
            server_samples.push(m);
        }
    }
    let mut reports: Vec<BotReport> = rx.iter().flatten().collect();
    report(&o, &mut reports, &server_samples);
}

fn fetch_metrics(addr: &str) -> Option<String> {
    let mut s = TcpStream::connect_timeout(&addr.parse().ok()?, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET /metrics.json HTTP/1.0\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.split("\r\n\r\n").nth(1).map(|b| b.to_string())
}

/// Pull a numeric field out of the first JSON object with players > 0.
fn field(json: &str, key: &str) -> Option<f64> {
    let obj = json
        .split('{')
        .find(|o| o.contains("\"players\":") && !o.contains("\"players\":0,"))?;
    let i = obj.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &obj[i..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

fn report(o: &Opts, r: &mut [BotReport], server: &[String]) {
    let secs = o.seconds as f64;
    let connected = r.iter().filter(|b| b.connected).count();
    let down: Vec<f64> = r.iter().map(|b| b.bytes_in as f64 / secs).collect();
    let up: Vec<f64> = r.iter().map(|b| b.bytes_out as f64 / secs).collect();
    let pk: f64 = r.iter().map(|b| b.packets_in as f64 / secs).sum::<f64>() / r.len().max(1) as f64;
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let max = |v: &[f64]| v.iter().cloned().fold(0.0, f64::max);
    let mut gaps: Vec<u32> = r
        .iter()
        .flat_map(|b| b.stats.queue_gaps_us.iter().copied())
        .collect();
    gaps.sort_unstable();
    let casts: u32 = r.iter().map(|b| b.stats.casts_confirmed).sum();
    let kills: u32 = r.iter().map(|b| b.stats.kills).sum();
    let deaths: u32 = r.iter().map(|b| b.stats.deaths).sum();
    let corrections: u32 = r.iter().map(|b| b.stats.corrections).sum();
    let misses: u32 = r.iter().map(|b| b.snapshot_misses).sum();
    let damage: f64 = r.iter().map(|b| b.stats.damage).sum();
    let rtt = mean(&r.iter().map(|b| b.rtt_ms).collect::<Vec<_>>());
    let seen = mean(&r.iter().map(|b| b.entities_seen as f64).collect::<Vec<_>>());

    // Overhead the bots can't see: IP + UDP headers, 28 bytes per packet.
    let wire_down = mean(&down) + pk * 28.0;
    let tick_p99s: Vec<f64> = server.iter().filter_map(|s| field(s, "tick_p99_ms")).collect();
    let tick_maxs: Vec<f64> = server.iter().filter_map(|s| field(s, "tick_max_ms")).collect();
    let tick_p50s: Vec<f64> = server.iter().filter_map(|s| field(s, "tick_p50_ms")).collect();
    let players: Vec<f64> = server.iter().filter_map(|s| field(s, "players")).collect();
    let view: Vec<f64> = server.iter().filter_map(|s| field(s, "view_avg")).collect();
    let worst_p99 = max(&tick_p99s);

    println!();
    println!("=== results: {} bots, {} s measured ===", r.len(), o.seconds);
    println!("connected at end        {connected}/{}", r.len());
    println!(
        "server players          {}",
        players.iter().map(|p| format!("{p:.0}")).collect::<Vec<_>>().join(", ")
    );
    println!(
        "server tick p50 / p99 / max (per 10 s window, ms)   {} / {} / {}",
        fmt_list(&tick_p50s),
        fmt_list(&tick_p99s),
        fmt_list(&tick_maxs)
    );
    println!("server view entities    {}", fmt_list(&view));
    println!(
        "down per client         mean {:.2} KB/s, max {:.2} KB/s (UDP payload); {:.2} KB/s incl. IP/UDP headers",
        mean(&down) / 1000.0,
        max(&down) / 1000.0,
        wire_down / 1000.0
    );
    println!("up per client           mean {:.2} KB/s", mean(&up) / 1000.0);
    println!("packets down per client {pk:.1}/s");
    println!("entities in view        {seen:.1} (client side, mean)");
    println!("mean rtt                {rtt:.1} ms");
    println!(
        "combat                  {casts} casts confirmed, {kills} kills, {deaths} deaths, {:.0} damage",
        damage
    );
    if !gaps.is_empty() {
        let p = |q: f64| gaps[((gaps.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
        println!(
            "spell-queue gap ms      median {:.2}, p99 {:.2}, max {:.2} ({} queued casts)",
            p(0.5),
            p(0.99),
            p(1.0),
            gaps.len()
        );
    }
    println!("movement corrections    {corrections}");
    println!("snapshot baseline misses {misses}");

    let tick_ok = !tick_p99s.is_empty() && worst_p99 < o.max_tick_p99;
    let bw_ok = mean(&down) / 1000.0 < o.max_down_kbps;
    let conn_ok = connected == r.len();
    println!();
    println!(
        "{} tick p99 < {} ms (worst window {:.2} ms)",
        if tick_ok { "PASS" } else { "FAIL" },
        o.max_tick_p99,
        worst_p99
    );
    println!(
        "{} down < {} KB/s per client (mean {:.2} KB/s)",
        if bw_ok { "PASS" } else { "FAIL" },
        o.max_down_kbps,
        mean(&down) / 1000.0
    );
    println!(
        "{} all bots stayed connected",
        if conn_ok { "PASS" } else { "FAIL" }
    );
    if let Some(path) = &o.json {
        let json = format!(
            "{{\"bots\":{},\"seconds\":{},\"connected\":{},\"tick_p50_ms\":[{}],\"tick_p99_ms\":[{}],\"tick_max_ms\":[{}],\"down_kbps_mean\":{:.3},\"down_kbps_max\":{:.3},\"down_kbps_wire\":{:.3},\"up_kbps_mean\":{:.3},\"view_avg\":[{}],\"rtt_ms\":{:.1},\"casts\":{},\"kills\":{},\"deaths\":{},\"corrections\":{},\"snapshot_misses\":{},\"fight\":{},\"roam\":{}}}",
            r.len(),
            o.seconds,
            connected,
            join(&tick_p50s),
            join(&tick_p99s),
            join(&tick_maxs),
            mean(&down) / 1000.0,
            max(&down) / 1000.0,
            wire_down / 1000.0,
            mean(&up) / 1000.0,
            join(&view),
            rtt,
            casts,
            kills,
            deaths,
            corrections,
            misses,
            o.fight,
            o.roam
        );
        std::fs::write(path, json).expect("writing --json");
    }
    std::process::exit(if tick_ok && bw_ok && conn_ok { 0 } else { 1 });
}

fn fmt_list(v: &[f64]) -> String {
    if v.is_empty() {
        return "n/a".into();
    }
    v.iter().map(|x| format!("{x:.2}")).collect::<Vec<_>>().join(", ")
}

fn join(v: &[f64]) -> String {
    v.iter().map(|x| format!("{x:.3}")).collect::<Vec<_>>().join(",")
}
