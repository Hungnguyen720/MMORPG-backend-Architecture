//! End-to-end check of the core game loop through the real services:
//! log in, create a character, level it in two zones (crossing a portal),
//! log out, log back in and compare; then loot an item and hold the
//! connection while a script kills the zone process with SIGKILL, and
//! verify the item survived. `scripts/e2e.sh` drives the whole sequence.
//!
//!   e2e level  --login URL --email E --name N --state out.json
//!   e2e verify --login URL --email E --name N --state out.json
//!   e2e loot-hold --login URL --email E --name N --state out.json

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use combat_core::codec::flags;
use combat_core::movement::dist_xz;
use combat_core::ContentPack;
use net_client::brain::Brain;
use net_client::login::LoginClient;
use net_client::{Auth, ClientEvent, ClientSession};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Saved {
    zone: String,
    level: u32,
    xp: u32,
    /// item id -> total count (bag + equipped)
    items: BTreeMap<String, u32>,
}

struct Opts {
    cmd: String,
    login: String,
    email: String,
    password: String,
    name: String,
    state: String,
    level_a: u32,
    level_b: u32,
    timeout_s: u64,
}

fn parse() -> Opts {
    let mut a = std::env::args().skip(1);
    let mut o = Opts {
        cmd: a.next().unwrap_or_else(|| {
            eprintln!("usage: e2e level|verify|loot-hold [--login URL] [--email E] [--name N] [--state F]");
            std::process::exit(2)
        }),
        login: "http://127.0.0.1:7000".into(),
        email: "e2e@example.com".into(),
        password: "correct horse battery".into(),
        name: "Testrunner".into(),
        state: "e2e-state.json".into(),
        level_a: 3,
        level_b: 4,
        timeout_s: 900,
    };
    while let Some(k) = a.next() {
        let mut v = || a.next().unwrap_or_else(|| panic!("{k} needs a value"));
        match k.as_str() {
            "--login" => o.login = v(),
            "--email" => o.email = v(),
            "--password" => o.password = v(),
            "--name" => o.name = v(),
            "--state" => o.state = v(),
            "--level-a" => o.level_a = v().parse().unwrap(),
            "--level-b" => o.level_b = v().parse().unwrap(),
            "--timeout" => o.timeout_s = v().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    o
}

fn log(msg: impl AsRef<str>) {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    let t = T0.get_or_init(Instant::now).elapsed().as_secs_f32();
    println!("[{t:7.1}s] {}", msg.as_ref());
}

/// Log in (registering on first use), find or create the character, and
/// connect to the zone the world service picks.
fn enter(o: &Opts) -> ClientSession {
    let mut lc = LoginClient::new(&o.login);
    if lc.login(&o.email, &o.password).is_err() {
        lc.register(&o.email, &o.password).expect("register");
        lc.login(&o.email, &o.password).expect("login");
        log(format!("registered {}", o.email));
    }
    let (pack, downloaded) = lc.sync_pack(None).expect("content pack");
    log(format!(
        "content pack \"{}\" {:016x}{}",
        pack.tenant.name,
        pack.hash,
        if downloaded { " (downloaded)" } else { "" }
    ));
    let list = lc.characters().expect("characters");
    let ch = match list.characters.iter().find(|c| c.name == o.name) {
        Some(c) => c.clone(),
        None => {
            let c = lc
                .create_character(&o.name, &list.classes[0].id)
                .expect("create character");
            log(format!("created {} the {}", c.name, c.class));
            c
        }
    };
    // A character that just logged out may still show as online for a
    // moment; retry for up to 40 s.
    let start = Instant::now();
    let ticket = loop {
        match lc.play(ch.id) {
            Ok(t) => break t,
            Err(e) if start.elapsed() < Duration::from_secs(40) => {
                log(format!("play: {e}; retrying"));
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => panic!("play: {e}"),
        }
    };
    log(format!(
        "{} (level {}) enters {} layer {} at {}",
        ch.name, ch.level, ticket.zone, ticket.layer, ticket.addr
    ));
    let mut s = ClientSession::connect(pack, ticket.addr, Auth::Token(ticket.token)).expect("connect");
    let t0 = Instant::now();
    while !(s.is_connected() && !s.bag.is_empty()) {
        s.update();
        if let Some(r) = s.disconnect_reason() {
            panic!("disconnected while joining: {r}");
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out joining");
        std::thread::sleep(Duration::from_millis(5));
    }
    s
}

fn snapshot(s: &ClientSession) -> Saved {
    let mut items = BTreeMap::new();
    for (item, n) in s.bag.iter().chain(s.equip.iter()).flatten() {
        *items.entry(s.pack.items[*item as usize].id.clone()).or_insert(0) += n;
    }
    Saved {
        zone: s.pack.zones[s.zone as usize].id.clone(),
        level: s.level,
        xp: s.xp,
        items,
    }
}

fn frame(s: &mut ClientSession, b: &mut Brain) {
    s.update();
    b.tick(s);
    for e in &b.seen {
        match e {
            ClientEvent::Xp { gained, leveled } => {
                if *leveled {
                    log(format!("LEVEL UP: level {} ({} xp from that kill)", s.level, gained));
                }
            }
            ClientEvent::Looted { items } => {
                let names: Vec<String> = items
                    .iter()
                    .map(|(i, n)| format!("{n}x {}", s.item_name(*i)))
                    .collect();
                log(format!("looted {}", names.join(", ")));
            }
            ClientEvent::ZoneChange { zone, layer } => log(format!(
                "handoff to {} layer {layer}",
                s.pack.zones[*zone as usize].name
            )),
            ClientEvent::Chat { from, text, .. } if from.is_empty() => log(format!("system: {text}")),
            _ => {}
        }
    }
    if let Some(r) = s.disconnect_reason()
        && !s.changing_zone()
    {
        panic!("disconnected: {r}");
    }
    std::thread::sleep(Duration::from_millis(10));
}

/// Fight around `camp` until the character reaches `level`.
fn grind(s: &mut ClientSession, camp: [f32; 3], level: u32, deadline: Instant) -> Brain {
    let mut b = Brain::new(camp, 25.0, 0x5eed ^ level as u64);
    log(format!(
        "grinding in {} near ({:.0}, {:.0}) to level {level}",
        s.pack.zones[s.zone as usize].name, camp[0], camp[2]
    ));
    while s.level < level {
        assert!(Instant::now() < deadline, "timed out grinding");
        frame(s, &mut b);
    }
    log(format!(
        "level {level} reached: {} kills, {} deaths, {} potions, {} items looted",
        b.stats.kills, b.stats.deaths, b.stats.potions, b.stats.items_looted
    ));
    b
}

/// Walk into the portal to `to_zone`, fighting off anything that attacks.
fn travel(s: &mut ClientSession, to_zone: &str, deadline: Instant) {
    let zd = &s.pack.zones[s.zone as usize];
    let portal = zd
        .portals
        .iter()
        .find(|p| p.to_zone == to_zone)
        .unwrap_or_else(|| panic!("no portal to {to_zone}"))
        .clone();
    log(format!("walking to the {} portal", portal.name));
    let target_zone = s.pack.zone_index(to_zone).unwrap();
    let mut b = Brain::new(s.pos, 0.0, 77);
    let mut stuck_since: Option<Instant> = None;
    let mut detour: Option<([f32; 3], Instant)> = None;
    let mut last_pos = s.pos;
    loop {
        assert!(Instant::now() < deadline, "timed out travelling");
        if s.is_connected() && s.zone == target_zone {
            log(format!("arrived in {}", s.pack.zones[s.zone as usize].name));
            return;
        }
        let in_combat = s.my_state().is_some_and(|st| st.flags & flags::IN_COMBAT != 0);
        if !s.is_connected() || s.dead() {
            frame(s, &mut b);
            continue;
        }
        // Fight back only against mobs that are fighting us.
        if in_combat {
            let attacker = s
                .entities
                .iter()
                .filter(|(_, e)| {
                    e.kind() == combat_core::EntityKind::Mob
                        && !e.dead()
                        && e.has_flag(flags::IN_COMBAT)
                        && !e.has_flag(flags::PASSIVE)
                })
                .filter_map(|(id, _)| Some((*id, dist_xz(s.pos, s.entity_pos(*id)?))))
                .filter(|(_, d)| *d < 30.0)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((id, _)) = attacker {
                s.target = Some(id);
                b.fight = true;
                b.home = s.pos;
                frame(s, &mut b);
                continue;
            }
            // Nobody left: wait outside the portal until combat ends.
            if dist_xz(s.pos, portal.pos) < portal.radius + 3.0 {
                s.set_local_move(s.pos, s.facing, false);
                frame(s, &mut b);
                continue;
            }
        }
        s.target = None;
        s.update();
        let dt = 0.01f32;
        let goal = match detour {
            Some((g, until)) if Instant::now() < until => g,
            _ => portal.pos,
        };
        let (dx, dz) = (goal[0] - s.pos[0], goal[2] - s.pos[2]);
        let d = (dx * dx + dz * dz).sqrt().max(0.001);
        let step = (s.move_speed() * dt).min(d);
        s.walk([dx / d * step, dz / d * step]);
        if dist_xz(s.pos, last_pos) < 0.001 {
            let since = *stuck_since.get_or_insert(Instant::now());
            if since.elapsed() > Duration::from_millis(300) {
                // Sidestep around whatever is in the way.
                let side = [s.pos[0] - dz / d * 12.0, 0.0, s.pos[2] + dx / d * 12.0];
                detour = Some((side, Instant::now() + Duration::from_secs(2)));
                stuck_since = None;
            }
        } else {
            stuck_since = None;
        }
        last_pos = s.pos;
        for e in s.take_events() {
            if let ClientEvent::Chat { from, text, .. } = e
                && from.is_empty()
            {
                log(format!("system: {text}"));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn logout(mut s: ClientSession) {
    s.disconnect();
    for _ in 0..20 {
        s.update();
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(s);
    log("logged out");
}

fn camp_of(pack: &ContentPack, zone: &str, mob: &str) -> [f32; 3] {
    pack.zone(zone)
        .and_then(|z| z.spawns.iter().find(|sp| sp.mob == mob))
        .map(|sp| sp.pos)
        .unwrap_or_else(|| panic!("no {mob} spawn in {zone}"))
}

fn main() {
    let o = parse();
    let deadline = Instant::now() + Duration::from_secs(o.timeout_s);
    match o.cmd.as_str() {
        "level" => {
            let mut s = enter(&o);
            let start = snapshot(&s);
            log(format!("start: {start:?}"));
            let home = s.pack.tenant.start_zone.clone();
            let second = s.pack.zone(&home).unwrap().portals[0].to_zone.clone();
            if start.zone == home {
                let camp = camp_of(&s.pack, &home, "forest_wolf");
                grind(&mut s, camp, o.level_a, deadline);
                travel(&mut s, &second, deadline);
            }
            let camp = camp_of(&s.pack, &second, "skeleton_warrior");
            grind(&mut s, camp, o.level_b, deadline);
            // Let the last kill's loot and any level save arrive.
            let mut b = Brain::new(s.pos, 0.0, 1);
            b.fight = false;
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(500) {
                frame(&mut s, &mut b);
            }
            let end = snapshot(&s);
            log(format!("end: {end:?}"));
            assert_eq!(end.zone, second, "should finish in the second zone");
            std::fs::write(&o.state, serde_json::to_string_pretty(&end).unwrap()).unwrap();
            logout(s);
        }
        "verify" => {
            let want: Saved = serde_json::from_str(&std::fs::read_to_string(&o.state).unwrap())
                .expect("state file");
            let s = enter(&o);
            let got = snapshot(&s);
            log(format!("expected: {want:?}"));
            log(format!("got:      {got:?}"));
            let mut ok = true;
            if got.zone != want.zone {
                log(format!("FAIL zone {} != {}", got.zone, want.zone));
                ok = false;
            }
            if got.level < want.level {
                log(format!("FAIL level {} < {}", got.level, want.level));
                ok = false;
            }
            for (item, n) in &want.items {
                let have = got.items.get(item).copied().unwrap_or(0);
                if have < *n {
                    log(format!("FAIL item {item}: have {have}, expected {n}"));
                    ok = false;
                }
            }
            logout(s);
            if ok {
                log("PASS: character, level and every item survived");
            } else {
                std::process::exit(1);
            }
        }
        "loot-hold" => {
            let mut s = enter(&o);
            let before = snapshot(&s);
            let zone = before.zone.clone();
            let mob = if zone == s.pack.tenant.start_zone {
                "forest_wolf"
            } else {
                "skeleton_warrior"
            };
            let camp = camp_of(&s.pack, &zone, mob);
            let mut b = Brain::new(camp, 25.0, 99);
            log("fighting until something drops");
            loop {
                assert!(Instant::now() < deadline, "timed out waiting for loot");
                frame(&mut s, &mut b);
                if b.seen.iter().any(|e| matches!(e, ClientEvent::Looted { .. })) {
                    break;
                }
            }
            // Wait for the inventory message that follows the loot.
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(300) {
                frame(&mut s, &mut b);
            }
            let after = snapshot(&s);
            std::fs::write(&o.state, serde_json::to_string_pretty(&after).unwrap()).unwrap();
            log(format!("inventory now: {:?}", after.items));
            // Stand still and keep the connection open until the server dies.
            log("HOLDING: kill the zone process now");
            b.fight = false;
            loop {
                s.update();
                if s.disconnect_reason().is_some() {
                    log("zone server went away (as planned)");
                    return;
                }
                if Instant::now() > deadline {
                    panic!("nobody killed the zone server");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        other => panic!("unknown command {other}"),
    }
}
