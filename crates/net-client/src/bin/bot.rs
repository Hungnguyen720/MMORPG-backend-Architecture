//! Headless client that plays a queued rotation against a training dummy
//! through the fake-lag transport and reports the server's spell-queue gaps.
//! The Phase 0 pass/fail check, kept as a regression test.
//!
//!   cargo run --release -p net-client --bin bot -- --latency 150 --loss 2 --seconds 30

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use combat_core::codec::flags;
use combat_core::movement::dist_xz;
use combat_core::{ContentPack, DEFAULT_PORT};
use net_client::{Auth, ClientEvent, ClientSession};

fn main() {
    let mut server: SocketAddr = format!("127.0.0.1:{DEFAULT_PORT}").parse().unwrap();
    let (mut latency, mut loss_pct, mut seconds) = (150u32, 2.0f32, 30u64);
    let mut content = "tenants/default".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().expect("missing value");
        match a.as_str() {
            "--server" => server = v().parse().expect("--server host:port"),
            "--latency" => latency = v().parse().unwrap(),
            "--loss" => loss_pct = v().parse().unwrap(),
            "--seconds" => seconds = v().parse().unwrap(),
            "--content" => content = v(),
            other => panic!("unknown argument {other}"),
        }
    }
    let pack = ContentPack::load(&content).expect("content pack");
    let slot = |s: &ClientSession, name: &str| {
        let id = s.pack.ability_index(name).unwrap();
        s.bar.iter().position(|a| *a == id).unwrap()
    };

    let mut s = ClientSession::connect(pack, server, Auth::Dev { name: "RotationBot".into() })
        .expect("connect");
    s.set_fake_lag(latency, loss_pct / 100.0);
    println!("bot: {server}, +{latency} ms RTT, {loss_pct}% loss each way, {seconds} s");

    let start = Instant::now();
    while !s.is_connected() {
        s.update();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "could not connect: {:?}",
            s.disconnect_reason()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // Wait for the spawn position, then target the nearest training dummy.
    let dummy = loop {
        s.update();
        if let Some(p) = s.take_correction() {
            s.pos = p;
        }
        let pos = s.pos;
        let best = s
            .entities
            .iter()
            .filter(|(_, e)| e.has_flag(flags::PASSIVE))
            .filter_map(|(id, _)| s.entity_pos(*id).map(|p| (*id, dist_xz(pos, p))))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((id, _)) = best {
            break id;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no training dummy in view");
        std::thread::sleep(Duration::from_millis(2));
    };
    s.target = Some(dummy);
    let rotation = [
        slot(&s, "searing_brand"),
        slot(&s, "frost_lance"),
        slot(&s, "ice_shard"),
        slot(&s, "arcane_torrent"),
        slot(&s, "frost_lance"),
        slot(&s, "ice_shard"),
    ];
    let surge = slot(&s, "arcane_surge");

    let (mut gaps, mut errors, mut damage, mut presses) =
        (Vec::<u32>::new(), Vec::<String>::new(), 0.0f32, 0u32);
    let mut i = 0;
    let mut pressed_at = 0u64;
    let run = Instant::now();
    while run.elapsed() < Duration::from_secs(seconds) {
        s.update();
        let now = s.now();
        let next = rotation[i % rotation.len()];
        let ab = s.bar[next];
        let def = &s.pack.abilities[ab as usize];
        let ready = s.caster.ready_at(ab, def);
        // Press once ~250 ms before ready, like a player leaning on the queue.
        if s.queued_slot().is_none() && now + 250 >= ready && now >= pressed_at + 100 {
            s.press(next);
            presses += 1;
            pressed_at = now;
            i += 1;
        }
        if s.swipe(surge) == 0.0 && s.caster.cast.is_none() && s.queued_slot().is_none() {
            s.press(surge);
        }
        for e in s.take_events() {
            match e {
                ClientEvent::Confirmed {
                    queue_gap_us: Some(g),
                    ..
                } => gaps.push(g),
                ClientEvent::Hit {
                    amount, mine: true, heal: false, ..
                } => damage += amount,
                ClientEvent::Error(m) => errors.push(m),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    gaps.sort();
    let ms = |us: u32| us as f64 / 1000.0;
    println!(
        "presses: {presses}, queued casts confirmed: {}, damage: {damage:.0} ({:.0} dps)",
        gaps.len(),
        damage / seconds as f32
    );
    if !gaps.is_empty() {
        let p = |q: f64| gaps[((gaps.len() - 1) as f64 * q) as usize];
        println!(
            "queue gap ms: median {:.2}, p99 {:.2}, max {:.2}",
            ms(p(0.5)),
            ms(p(0.99)),
            ms(*gaps.last().unwrap())
        );
    }
    println!(
        "measured rtt {:.0} ms, renet loss estimate {:.1}%",
        s.rtt_ms(),
        s.packet_loss() * 100.0
    );
    println!(
        "errors: {}",
        if errors.is_empty() {
            "none".into()
        } else {
            errors.join(", ")
        }
    );
    let pass = !gaps.is_empty() && *gaps.last().unwrap() < 5000;
    println!(
        "{}",
        if pass {
            "PASS: every queued cast started < 5 ms after the caster was ready"
        } else {
            "FAIL"
        }
    );
    s.disconnect();
    for _ in 0..20 {
        s.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    std::process::exit(if pass { 0 } else { 1 });
}
