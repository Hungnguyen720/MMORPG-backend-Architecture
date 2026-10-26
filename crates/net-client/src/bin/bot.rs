//! Headless client that plays against the dummy through the fake-lag
//! transport.
//!
//! Default mode plays a queued rotation and reports the server's spell-queue
//! gaps:
//!
//!   cargo run --release -p net-client --bin bot -- --latency 150 --loss 2 --seconds 30
//!
//! `--mode aimed` leads skillshots and ground runes at the walking dummy and
//! reports how often what hit on screen also hit on the server:
//!
//!   cargo run --release -p net-client --bin bot -- --mode aimed --latency 150 --loss 2
//!
//! Add `--jitter 1.5` to throw aims up to 1.5 m off, so near misses get tested.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use combat_core::movement::dist_xz;
use combat_core::{ContentPack, DEFAULT_PORT, EntityId};
use net_client::{ClientEvent, ClientSession};

fn main() {
    let mut server: SocketAddr = format!("127.0.0.1:{DEFAULT_PORT}").parse().unwrap();
    let (mut latency, mut loss_pct, mut seconds) = (150u32, 2.0f32, 30u64);
    let mut content = "godot/content/abilities.yaml".to_string();
    let mut mode = "queue".to_string();
    let mut jitter = 0.0f32;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().expect("missing value");
        match a.as_str() {
            "--server" => server = v().parse().expect("--server host:port"),
            "--latency" => latency = v().parse().unwrap(),
            "--loss" => loss_pct = v().parse().unwrap(),
            "--seconds" => seconds = v().parse().unwrap(),
            "--content" => content = v(),
            "--mode" => mode = v(),
            "--jitter" => jitter = v().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    let pack =
        ContentPack::from_yaml(&std::fs::read_to_string(&content).expect("content file")).unwrap();
    let id = |name: &str| pack.abilities.iter().position(|a| a.id == name).unwrap() as u16;
    let rotation = [
        id("searing_brand"),
        id("frost_lance"),
        id("ice_shard"),
        id("arcane_torrent"),
        id("frost_lance"),
        id("ice_shard"),
    ];
    let surge = id("arcane_surge");

    let mut s = ClientSession::connect(pack, server).expect("connect");
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
    // Stand still at the spawn point, in range of the dummy.
    while s.dummy().is_none() {
        s.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    let dummy = s.dummy();
    s.set_target(dummy);
    if mode == "aimed" {
        let pass = aimed(&mut s, dummy.unwrap(), seconds, jitter);
        finish(s, pass);
    }

    let (mut gaps, mut errors, mut damage, mut presses) =
        (Vec::<u32>::new(), Vec::<String>::new(), 0.0f32, 0u32);
    let mut i = 0;
    let mut pressed_at = 0u64;
    let run = Instant::now();
    while run.elapsed() < Duration::from_secs(seconds) {
        s.update();
        let now = s.now();
        let next = rotation[i % rotation.len()];
        let def = &s.pack.abilities[next as usize];
        let ready = s.caster.ready_at(next, def);
        // Press once ~250 ms before ready, like a player leaning on the queue.
        if s.queued_ability().is_none() && now + 250 >= ready && now >= pressed_at + 100 {
            s.press(next);
            presses += 1;
            pressed_at = now;
            i += 1;
        }
        if s.swipe(surge) == 0.0 && s.caster.cast.is_none() && s.queued_ability().is_none() {
            s.press(surge);
        }
        for e in s.take_events() {
            match e {
                ClientEvent::Confirmed {
                    queue_gap_us: Some(g),
                    ..
                } => gaps.push(g),
                ClientEvent::Hit {
                    amount, mine: true, ..
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
    finish(s, pass);
}

fn finish(mut s: ClientSession, pass: bool) -> ! {
    s.disconnect();
    for _ in 0..20 {
        s.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    std::process::exit(if pass { 0 } else { 1 });
}

/// Lead skillshots and runes at the walking dummy, like a player would, and
/// compare each shot's on-screen verdict with the server's.
/// `jitter` throws each aim off by up to that many metres sideways, so some
/// shots graze or miss and the edge cases get tested too.
fn aimed(s: &mut ClientSession, dummy: EntityId, seconds: u64, jitter: f32) -> bool {
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut off = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        ((rng >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * jitter
    };
    let id = |name: &str| s.pack.abilities.iter().position(|a| a.id == name).unwrap() as u16;
    let (spike, rune) = (id("glacial_spike"), id("flame_rune"));
    let spike_def = s.pack.abilities[spike as usize].clone();
    let rune_def = s.pack.abilities[rune as usize].clone();
    let (mut screen_hits, mut server_hits, mut confirmed, mut rune_hits) = (0u32, 0u32, 0u32, 0u32);
    let mut disagreements = Vec::new();
    let mut errors = Vec::<String>::new();
    // Last two sightings of the dummy, for its velocity.
    let mut seen: Option<(u64, [f32; 3])> = None;
    let mut vel = [0.0f32; 2];
    let run = Instant::now();
    while run.elapsed() < Duration::from_secs(seconds) {
        s.update();
        let now = s.now();
        let Some(dp) = s.entity_pos(dummy) else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        match seen {
            Some((t, p)) if now >= t + 100 => {
                let dt = (now - t) as f32 / 1000.0;
                vel = [(dp[0] - p[0]) / dt, (dp[2] - p[2]) / dt];
                seen = Some((now, dp));
            }
            None => seen = Some((now, dp)),
            _ => {}
        }
        // Where the dummy will be after `lead_s` seconds.
        let ahead = |lead_s: f32| [dp[0] + vel[0] * lead_s, dp[2] + vel[1] * lead_s];
        let busy = s.caster.cast.is_some() || s.queued_ability().is_some();
        if !busy && s.swipe(rune) == 0.0 && s.caster.mana >= rune_def.mana_cost {
            s.press_at(rune, Some(ahead(rune_def.delay_ms as f32 / 1000.0)));
        } else if !busy && s.swipe(spike) == 0.0 {
            // Flight time to the dummy plus the wind-up, refined once.
            let mut lead = spike_def.cast_ms as f32 / 1000.0;
            for _ in 0..2 {
                let a = ahead(lead);
                let d = dist_xz(s.pos, [a[0], 0.0, a[1]]);
                lead = spike_def.cast_ms as f32 / 1000.0 + d / spike_def.speed;
            }
            let mut a = ahead(lead);
            let (dx, dz) = (a[0] - s.pos[0], a[1] - s.pos[2]);
            let len = (dx * dx + dz * dz).sqrt().max(1e-3);
            let k = off();
            a = [a[0] - dz / len * k, a[1] + dx / len * k];
            s.press_at(spike, Some(a));
        }
        for e in s.take_events() {
            match e {
                ClientEvent::ShotResolved { predicted, server } => {
                    screen_hits += predicted.is_some() as u32;
                    server_hits += server.is_some() as u32;
                    confirmed += (predicted.is_some() && predicted == server) as u32;
                    if predicted != server {
                        disagreements.push(format!("screen {predicted:?} vs server {server:?}"));
                    }
                }
                ClientEvent::Hit {
                    ability,
                    mine: true,
                    ..
                } if ability == rune => rune_hits += 1,
                ClientEvent::Error(m) => errors.push(m),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let (agreed, judged) = s.shot_agreement;
    let pct = |a: u32, b: u32| {
        if b == 0 {
            0.0
        } else {
            a as f64 * 100.0 / b as f64
        }
    };
    println!(
        "skillshots judged: {judged}, screen and server agreed: {agreed} ({:.1}%)",
        pct(agreed, judged)
    );
    println!(
        "hits on screen: {screen_hits}, confirmed by the server: {confirmed} ({:.1}%); \
         server hits: {server_hits}; rune hits: {rune_hits}",
        pct(confirmed, screen_hits)
    );
    if !disagreements.is_empty() {
        println!("disagreements: {}", disagreements.join("; "));
    }
    println!(
        "measured rtt {:.0} ms; errors: {}",
        s.rtt_ms(),
        if errors.is_empty() {
            "none".into()
        } else {
            errors.join(", ")
        }
    );
    // The Phase 0.5 bar: a shot that visibly connects is confirmed.
    let pass = judged >= 10 && screen_hits > 0 && pct(confirmed, screen_hits) >= 95.0;
    println!(
        "{}",
        if pass {
            "PASS: at least 95% of skillshots that hit on screen also hit on the server"
        } else {
            "FAIL"
        }
    );
    pass
}
