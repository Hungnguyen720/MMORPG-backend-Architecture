//! GDExtension: exposes `CombatClient` to GDScript. All networking,
//! prediction and interpolation live in `net-client`; this file only
//! converts between Rust and Godot types.

use std::net::ToSocketAddrs;

use combat_core::ContentPack;
use combat_core::Delivery;
use combat_core::movement::ARENA_HALF_EXTENT;
use combat_core::protocol::{EntityKind, PawnCmd};
use godot::prelude::*;
use net_client::{ClientEvent, ClientSession, MarkerKind};

struct Phase0Extension;

#[gdextension]
unsafe impl ExtensionLibrary for Phase0Extension {}

#[derive(GodotClass)]
#[class(base = Node, init)]
pub struct CombatClient {
    session: Option<ClientSession>,
    base: Base<Node>,
}

fn dict(pairs: &[(&str, Variant)]) -> VarDictionary {
    let mut d = VarDictionary::new();
    for (k, v) in pairs {
        d.set(&k.to_variant(), v);
    }
    d
}

fn v3(p: [f32; 3]) -> Vector3 {
    Vector3::new(p[0], p[1], p[2])
}

#[godot_api]
impl CombatClient {
    /// Loads the content pack text and starts connecting. Returns "" on
    /// success, otherwise an error message.
    #[func]
    fn connect_to_server(&mut self, host: GString, port: i32, content_yaml: GString) -> GString {
        let pack = match ContentPack::from_yaml(&content_yaml.to_string()) {
            Ok(p) => p,
            Err(e) => return GString::from(&format!("content pack: {e}")),
        };
        let addr = match (host.to_string().as_str(), port as u16)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
        {
            Some(a) => a,
            None => return GString::from(&format!("cannot resolve {host}")),
        };
        match ClientSession::connect(pack, addr) {
            Ok(s) => {
                self.session = Some(s);
                GString::new()
            }
            Err(e) => GString::from(&e),
        }
    }

    /// Call every frame.
    #[func]
    fn poll(&mut self) {
        if let Some(s) = &mut self.session {
            s.update();
        }
    }

    #[func]
    fn is_online(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_connected())
    }

    #[func]
    fn content_mismatch(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.content_mismatch)
    }

    #[func]
    fn set_fake_lag(&mut self, latency_ms: i32, loss_percent: f32) {
        if let Some(s) = &mut self.session {
            s.set_fake_lag(latency_ms.max(0) as u32, loss_percent / 100.0);
        }
    }

    #[func]
    fn press(&mut self, slot: i32) {
        if let Some(s) = &mut self.session
            && slot >= 0
            && (slot as usize) < s.pack.abilities.len()
        {
            s.press(slot as u16);
        }
    }

    /// Press an aimed ability at a ground point (the cursor, or a drag on
    /// mobile). Targeted abilities ignore the point.
    #[func]
    fn press_at(&mut self, slot: i32, aim: Vector3) {
        if let Some(s) = &mut self.session
            && slot >= 0
            && (slot as usize) < s.pack.abilities.len()
        {
            s.press_at(slot as u16, Some([aim.x, aim.z]));
        }
    }

    /// Skillshots in flight on our screen: [{pos, dir, color, mine}].
    #[func]
    fn shots(&self) -> VarArray {
        let mut out = VarArray::new();
        let Some(s) = &self.session else { return out };
        for v in s.shots() {
            let color = s
                .pack
                .ability(v.ability)
                .map_or("#ffffff".to_string(), |a| a.color.clone());
            out.push(
                &dict(&[
                    ("pos", Vector3::new(v.pos[0], 0.0, v.pos[1]).to_variant()),
                    ("dir", Vector3::new(v.dir[0], 0.0, v.dir[1]).to_variant()),
                    ("color", color.to_variant()),
                    ("mine", v.mine.to_variant()),
                ])
                .to_variant(),
            );
        }
        out
    }

    /// Circles on the ground: [{pos, radius, kind ("mine"|"ally"|"danger"),
    /// progress 0..1 until it lands, landed}].
    #[func]
    fn markers(&self) -> VarArray {
        let mut out = VarArray::new();
        let Some(s) = &self.session else { return out };
        let now = s.now();
        for m in s.markers() {
            let span = m.lands_at.saturating_sub(m.placed_at).max(1) as f32;
            let progress = (now.saturating_sub(m.placed_at) as f32 / span).min(1.0);
            let kind = match m.kind {
                MarkerKind::Mine => "mine",
                MarkerKind::Ally => "ally",
                MarkerKind::Danger => "danger",
            };
            out.push(
                &dict(&[
                    ("pos", Vector3::new(m.pos[0], 0.0, m.pos[1]).to_variant()),
                    ("radius", m.radius.to_variant()),
                    ("kind", kind.to_variant()),
                    ("progress", progress.to_variant()),
                    ("landed", (now >= m.lands_at).to_variant()),
                ])
                .to_variant(),
            );
        }
        out
    }

    #[func]
    fn set_local_move(&mut self, pos: Vector3, facing: f32, moving: bool) {
        if let Some(s) = &mut self.session {
            s.set_local_move([pos.x, pos.y, pos.z], facing, moving);
        }
    }

    /// -1 clears the target.
    #[func]
    fn set_target(&mut self, id: i64) {
        if let Some(s) = &mut self.session {
            s.set_target((id >= 0).then_some(id as u32));
        }
    }

    #[func]
    fn get_target(&self) -> i64 {
        self.session
            .as_ref()
            .and_then(|s| s.target)
            .map_or(-1, |t| t as i64)
    }

    /// "attack": pawns attack our target now. "regroup": pawns come back.
    #[func]
    fn pawn_command(&mut self, cmd: GString) {
        let cmd = match cmd.to_string().as_str() {
            "attack" => PawnCmd::Attack,
            "regroup" => PawnCmd::Regroup,
            _ => return,
        };
        if let Some(s) = &mut self.session {
            s.pawn_command(cmd);
        }
    }

    /// Our health as (current, max), from the latest snapshot.
    #[func]
    fn hp(&self) -> Vector2 {
        self.session
            .as_ref()
            .map_or(Vector2::ZERO, |s| Vector2::new(s.hp, s.max_hp))
    }

    #[func]
    fn is_dead(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_dead())
    }

    /// Half the side length of the square map, in metres.
    #[func]
    fn world_half_extent(&self) -> f32 {
        ARENA_HALF_EXTENT
    }

    /// Static layout for the greybox level: {spawn, dummy, camps: [{pos, mob, count}]}.
    #[func]
    fn world_layout(&self) -> VarDictionary {
        let Some(s) = &self.session else {
            return VarDictionary::new();
        };
        let mut camps = VarArray::new();
        for c in &s.pack.camps {
            camps.push(
                &dict(&[
                    ("pos", v3(c.position).to_variant()),
                    ("mob", c.mob.to_variant()),
                    ("count", c.count.to_variant()),
                ])
                .to_variant(),
            );
        }
        dict(&[
            ("spawn", v3(s.pack.world.spawn).to_variant()),
            ("dummy", v3(s.pack.dummy.position).to_variant()),
            ("camps", camps.to_variant()),
        ])
    }

    #[func]
    fn my_id(&self) -> i64 {
        self.session
            .as_ref()
            .and_then(|s| s.me)
            .map_or(-1, |t| t as i64)
    }

    #[func]
    fn dummy_id(&self) -> i64 {
        self.session
            .as_ref()
            .and_then(|s| s.dummy())
            .map_or(-1, |t| t as i64)
    }

    /// Vector3 if the server moved us, otherwise null.
    #[func]
    fn take_correction(&mut self) -> Variant {
        match self.session.as_mut().and_then(|s| s.take_correction()) {
            Some(p) => v3(p).to_variant(),
            None => Variant::nil(),
        }
    }

    #[func]
    fn ability_count(&self) -> i32 {
        self.session
            .as_ref()
            .map_or(0, |s| s.pack.abilities.len() as i32)
    }

    #[func]
    fn ability_info(&self, slot: i32) -> VarDictionary {
        let Some(a) = self
            .session
            .as_ref()
            .and_then(|s| s.pack.abilities.get(slot as usize))
        else {
            return VarDictionary::new();
        };
        dict(&[
            ("id", a.id.to_variant()),
            ("name", a.name.to_variant()),
            ("color", a.color.to_variant()),
            ("cast_ms", a.cast_ms.to_variant()),
            ("channel_ms", a.channel_ms.to_variant()),
            ("gcd_ms", a.gcd_ms.to_variant()),
            ("cooldown_ms", a.cooldown_ms.to_variant()),
            ("mana_cost", a.mana_cost.to_variant()),
            ("range", a.range.to_variant()),
            ("anim", a.anim.to_variant()),
            ("vfx", a.vfx.to_variant()),
            (
                "delivery",
                match a.delivery {
                    Delivery::Target => "target",
                    Delivery::Skillshot => "skillshot",
                    Delivery::Ground => "ground",
                    Delivery::Dash => "dash",
                }
                .to_variant(),
            ),
            ("radius", a.radius.to_variant()),
            ("width", a.width.to_variant()),
            ("dash_m", a.dash_m.to_variant()),
        ])
    }

    /// 1 → 0 remaining fraction of whatever gates this button.
    #[func]
    fn swipe(&self, slot: i32) -> f32 {
        self.session.as_ref().map_or(0.0, |s| s.swipe(slot as u16))
    }

    #[func]
    fn queued_slot(&self) -> i32 {
        self.session
            .as_ref()
            .and_then(|s| s.queued_ability())
            .map_or(-1, |a| a as i32)
    }

    /// Our own predicted cast bar: {active, name, color, progress, remaining_ms, channel}.
    #[func]
    fn cast_bar(&self) -> VarDictionary {
        let Some(s) = &self.session else {
            return dict(&[("active", false.to_variant())]);
        };
        match s.my_cast_bar() {
            Some((ab, progress, remaining, channel)) => {
                let a = &s.pack.abilities[ab as usize];
                dict(&[
                    ("active", true.to_variant()),
                    ("name", a.name.to_variant()),
                    ("color", a.color.to_variant()),
                    ("progress", progress.to_variant()),
                    ("remaining_ms", (remaining as i64).to_variant()),
                    ("channel", channel.to_variant()),
                ])
            }
            None => dict(&[("active", false.to_variant())]),
        }
    }

    #[func]
    fn mana(&self) -> Vector2 {
        self.session.as_ref().map_or(Vector2::ZERO, |s| {
            Vector2::new(s.caster.mana, s.caster.max_mana)
        })
    }

    /// Every entity the server knows about, remote ones interpolated 100 ms back.
    #[func]
    fn entities(&self) -> VarArray {
        let mut out = VarArray::new();
        let Some(s) = &self.session else { return out };
        for (id, e) in &s.entities {
            let (pos, facing) = s.interpolated(*id).unwrap_or(([0.0; 3], 0.0));
            let mut auras = VarArray::new();
            for (aura, remaining) in &e.auras {
                let name = s
                    .pack
                    .aura(*aura)
                    .map_or("?".to_string(), |a| a.name.clone());
                auras.push(
                    &dict(&[
                        ("name", name.to_variant()),
                        ("remaining_ms", remaining.to_variant()),
                    ])
                    .to_variant(),
                );
            }
            let (cast_name, cast_progress) = match e.casting {
                Some((ab, elapsed, bar)) => (
                    s.pack.ability(ab).map_or(String::new(), |a| a.name.clone()),
                    if bar > 0 {
                        elapsed as f32 / bar as f32
                    } else {
                        1.0
                    },
                ),
                None => (String::new(), 0.0),
            };
            let kind = match e.kind {
                EntityKind::Player => "player",
                EntityKind::Dummy => "dummy",
                EntityKind::Mob => "mob",
                EntityKind::Pawn => "pawn",
            };
            // (name, def id, colour, scale, attack range) from the content pack.
            let (name, def_id, color, scale, reach) = match e.kind {
                EntityKind::Player => (
                    format!("Player {id}"),
                    String::new(),
                    "#3a8fff".to_string(),
                    1.0,
                    0.0,
                ),
                EntityKind::Dummy => (
                    s.pack.dummy.name.clone(),
                    "dummy".into(),
                    "#8c6640".into(),
                    1.0,
                    0.0,
                ),
                EntityKind::Mob => s.pack.mobs.get(e.def as usize).map_or_else(
                    || ("?".into(), String::new(), "#ff00ff".into(), 1.0, 0.0),
                    |m| {
                        (
                            m.name.clone(),
                            m.id.clone(),
                            m.color.clone(),
                            m.scale,
                            m.attack_range,
                        )
                    },
                ),
                EntityKind::Pawn => s.pack.pawns.get(e.def as usize).map_or_else(
                    || ("?".into(), String::new(), "#ff00ff".into(), 1.0, 0.0),
                    |p| {
                        (
                            p.name.clone(),
                            p.id.clone(),
                            p.color.clone(),
                            1.0,
                            p.attack_range,
                        )
                    },
                ),
            };
            let mine = e.owner.is_some() && e.owner == s.me;
            out.push(
                &dict(&[
                    ("id", (*id as i64).to_variant()),
                    ("kind", kind.to_variant()),
                    ("name", name.to_variant()),
                    ("is_me", (Some(*id) == s.me).to_variant()),
                    ("def", def_id.to_variant()),
                    ("color", color.to_variant()),
                    ("scale", scale.to_variant()),
                    ("reach", reach.to_variant()),
                    ("owner", e.owner.map_or(-1, |o| o as i64).to_variant()),
                    ("mine", mine.to_variant()),
                    ("hostile", e.kind.hostile().to_variant()),
                    ("dead", (e.hp <= 0.0).to_variant()),
                    ("target", e.target.map_or(-1, |t| t as i64).to_variant()),
                    ("pos", v3(pos).to_variant()),
                    ("facing", facing.to_variant()),
                    ("hp", e.hp.to_variant()),
                    ("max_hp", e.max_hp.to_variant()),
                    ("cast_name", cast_name.to_variant()),
                    ("cast_progress", cast_progress.to_variant()),
                    ("auras", auras.to_variant()),
                ])
                .to_variant(),
            );
        }
        out
    }

    /// Drains events since the last call:
    /// {type: "hit"|"swing"|"died"|"error"|"log"|"confirmed", ...}.
    #[func]
    fn take_events(&mut self) -> VarArray {
        let mut out = VarArray::new();
        let Some(s) = &mut self.session else {
            return out;
        };
        for e in s.take_events() {
            let d = match e {
                ClientEvent::Hit {
                    source,
                    target,
                    ability,
                    aura,
                    amount,
                    crit,
                    mine,
                } => {
                    let name = match aura {
                        Some(a) => s.pack.aura(a).map(|a| a.name.clone()),
                        None => s.pack.ability(ability).map(|a| a.name.clone()),
                    }
                    .unwrap_or_default();
                    let aimed = aura.is_none()
                        && s.pack.ability(ability).is_some_and(|a| a.delivery.aimed());
                    dict(&[
                        ("type", "hit".to_variant()),
                        ("aimed", aimed.to_variant()),
                        ("source", (source as i64).to_variant()),
                        ("target", (target as i64).to_variant()),
                        ("name", name.to_variant()),
                        ("amount", amount.to_variant()),
                        ("crit", crit.to_variant()),
                        ("periodic", aura.is_some().to_variant()),
                        ("mine", mine.to_variant()),
                    ])
                }
                ClientEvent::Swing {
                    source,
                    target,
                    amount,
                    crit,
                    heal,
                    mine,
                } => dict(&[
                    ("type", "swing".to_variant()),
                    ("source", (source as i64).to_variant()),
                    ("target", (target as i64).to_variant()),
                    ("amount", amount.to_variant()),
                    ("crit", crit.to_variant()),
                    ("heal", heal.to_variant()),
                    ("mine", mine.to_variant()),
                ]),
                ClientEvent::Died { entity } => dict(&[
                    ("type", "died".to_variant()),
                    ("entity", (entity as i64).to_variant()),
                ]),
                ClientEvent::Error(m) => {
                    dict(&[("type", "error".to_variant()), ("text", m.to_variant())])
                }
                ClientEvent::Log(m) => {
                    dict(&[("type", "log".to_variant()), ("text", m.to_variant())])
                }
                ClientEvent::Dash { dir, distance } => dict(&[
                    ("type", "dash".to_variant()),
                    ("dir", Vector3::new(dir[0], 0.0, dir[1]).to_variant()),
                    ("distance", distance.to_variant()),
                ]),
                ClientEvent::ShotResolved { predicted, server } => dict(&[
                    ("type", "shot_resolved".to_variant()),
                    ("predicted", predicted.map_or(-1, |e| e as i64).to_variant()),
                    ("server", server.map_or(-1, |e| e as i64).to_variant()),
                    ("agree", (predicted == server).to_variant()),
                ]),
                ClientEvent::Confirmed {
                    seq,
                    ability,
                    queue_gap_us,
                } => dict(&[
                    ("type", "confirmed".to_variant()),
                    ("seq", seq.to_variant()),
                    (
                        "name",
                        s.pack
                            .ability(ability)
                            .map_or(String::new(), |a| a.name.clone())
                            .to_variant(),
                    ),
                    (
                        "queue_gap_ms",
                        queue_gap_us
                            .map_or(-1.0, |g| g as f64 / 1000.0)
                            .to_variant(),
                    ),
                ]),
            };
            out.push(&d.to_variant());
        }
        out
    }

    /// {rtt_ms, loss, last_queue_gap_ms (-1 if none), shots_agreed, shots_judged}
    #[func]
    fn net_stats(&self) -> VarDictionary {
        let Some(s) = &self.session else {
            return VarDictionary::new();
        };
        dict(&[
            ("rtt_ms", s.rtt_ms().to_variant()),
            ("loss", s.packet_loss().to_variant()),
            (
                "last_queue_gap_ms",
                s.last_queue_gap_us
                    .map_or(-1.0, |g| g as f64 / 1000.0)
                    .to_variant(),
            ),
            ("shots_agreed", s.shot_agreement.0.to_variant()),
            ("shots_judged", s.shot_agreement.1.to_variant()),
        ])
    }

    #[func]
    fn disconnect_from_server(&mut self) {
        if let Some(s) = &mut self.session {
            s.disconnect();
        }
        self.session = None;
    }
}
