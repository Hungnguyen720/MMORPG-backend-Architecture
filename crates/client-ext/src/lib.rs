//! GDExtension: exposes `CombatClient` to GDScript. All networking,
//! prediction and interpolation live in `net-client`; this file only
//! converts between Rust and Godot types.

use std::net::ToSocketAddrs;

use combat_core::codec::flags;
use combat_core::{ContentPack, EntityKind};
use godot::prelude::*;
use net_client::{Auth, ClientEvent, ClientSession};

struct EngineExtension;

#[gdextension]
unsafe impl ExtensionLibrary for EngineExtension {}

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

fn resolve(host: &str, port: u16) -> Option<std::net::SocketAddr> {
    (host, port).to_socket_addrs().ok().and_then(|mut a| a.next())
}

#[godot_api]
impl CombatClient {
    /// Development connection: no login service. `content` is a tenant
    /// directory or a compiled .pack file. Returns "" on success, otherwise
    /// an error message.
    #[func]
    fn connect_dev(&mut self, host: GString, port: i32, content: GString, name: GString) -> GString {
        let pack = match ContentPack::load(content.to_string()) {
            Ok(p) => p,
            Err(e) => return GString::from(&format!("content pack: {e}")),
        };
        let Some(addr) = resolve(&host.to_string(), port as u16) else {
            return GString::from(&format!("cannot resolve {host}"));
        };
        match ClientSession::connect(pack, addr, Auth::Dev { name: name.to_string() }) {
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

    /// Press action bar slot `slot` (0-based).
    #[func]
    fn press(&mut self, slot: i32) {
        if let Some(s) = &mut self.session
            && slot >= 0
        {
            s.press(slot as usize);
        }
    }

    /// Move by (dx, dz) metres this frame, sliding along walls. Returns the
    /// new position.
    #[func]
    fn walk(&mut self, dx: f32, dz: f32) -> Vector3 {
        match &mut self.session {
            Some(s) => v3(s.walk([dx, dz])),
            None => Vector3::ZERO,
        }
    }

    /// Standing still: report position and facing.
    #[func]
    fn set_local_move(&mut self, pos: Vector3, facing: f32, moving: bool) {
        if let Some(s) = &mut self.session {
            s.set_local_move([pos.x, pos.y, pos.z], facing, moving);
        }
    }

    #[func]
    fn my_pos(&self) -> Vector3 {
        self.session.as_ref().map_or(Vector3::ZERO, |s| v3(s.pos))
    }

    #[func]
    fn move_speed(&self) -> f32 {
        self.session.as_ref().map_or(7.0, |s| s.move_speed())
    }

    #[func]
    fn release(&mut self) {
        if let Some(s) = &mut self.session {
            s.release();
        }
    }

    /// -1 clears the target.
    #[func]
    fn set_target(&mut self, id: i64) {
        if let Some(s) = &mut self.session {
            s.target = (id >= 0).then_some(id as u32);
        }
    }

    #[func]
    fn get_target(&self) -> i64 {
        self.session
            .as_ref()
            .and_then(|s| s.target)
            .map_or(-1, |t| t as i64)
    }

    /// Tab targeting: nearest hostile in range, cycling. -1 if none.
    #[func]
    fn next_target(&self, max_range: f32) -> i64 {
        self.session
            .as_ref()
            .and_then(|s| s.next_target(max_range, true))
            .map_or(-1, |t| t as i64)
    }

    #[func]
    fn my_id(&self) -> i64 {
        self.session
            .as_ref()
            .and_then(|s| s.me)
            .map_or(-1, |t| t as i64)
    }

    /// {hp, max_hp, mana, max_mana, level, dead, in_combat, name}
    #[func]
    fn my_state(&self) -> VarDictionary {
        let Some(s) = &self.session else {
            return VarDictionary::new();
        };
        let Some(st) = s.my_state() else {
            return VarDictionary::new();
        };
        dict(&[
            ("hp", (st.hp as f32).to_variant()),
            ("max_hp", (st.max_hp as f32).to_variant()),
            ("mana", s.caster.mana.to_variant()),
            ("max_mana", s.caster.max_mana.to_variant()),
            ("level", (st.level as i64).to_variant()),
            ("dead", (st.flags & flags::DEAD != 0).to_variant()),
            ("in_combat", (st.flags & flags::IN_COMBAT != 0).to_variant()),
            ("name", s.my_name.to_variant()),
        ])
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
    fn bar_count(&self) -> i32 {
        self.session.as_ref().map_or(0, |s| s.bar.len() as i32)
    }

    #[func]
    fn ability_info(&self, slot: i32) -> VarDictionary {
        let Some(a) = self.session.as_ref().and_then(|s| {
            s.bar
                .get(slot as usize)
                .and_then(|id| s.pack.ability(*id))
        }) else {
            return VarDictionary::new();
        };
        dict(&[
            ("id", a.id.to_variant()),
            ("name", a.name.to_variant()),
            ("color", a.color.to_variant()),
            ("icon", a.icon.to_variant()),
            ("vfx", a.vfx.to_variant()),
            ("cast_ms", a.cast_ms.to_variant()),
            ("channel_ms", a.channel_ms.to_variant()),
            ("gcd_ms", a.gcd_ms.to_variant()),
            ("cooldown_ms", a.cooldown_ms.to_variant()),
            ("mana_cost", a.mana_cost.to_variant()),
            ("range", a.range.to_variant()),
        ])
    }

    /// 1 → 0 remaining fraction of whatever gates this button.
    #[func]
    fn swipe(&self, slot: i32) -> f32 {
        self.session
            .as_ref()
            .map_or(0.0, |s| s.swipe(slot.max(0) as usize))
    }

    #[func]
    fn queued_slot(&self) -> i32 {
        self.session
            .as_ref()
            .and_then(|s| s.queued_slot())
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

    /// Tenant name and UI theme colours from the content pack.
    #[func]
    fn tenant_info(&self) -> VarDictionary {
        let Some(s) = &self.session else {
            return VarDictionary::new();
        };
        let mut theme = VarDictionary::new();
        for (k, v) in &s.pack.tenant.theme {
            theme.set(&k.to_variant(), &v.to_variant());
        }
        dict(&[
            ("id", s.pack.tenant.id.to_variant()),
            ("name", s.pack.tenant.name.to_variant()),
            ("theme", theme.to_variant()),
        ])
    }

    /// The current zone: name, size, look and walls.
    #[func]
    fn zone_info(&self) -> VarDictionary {
        let Some(s) = &self.session else {
            return VarDictionary::new();
        };
        let z = &s.pack.zones[s.zone as usize];
        let mut blockers = VarArray::new();
        for b in &z.blockers {
            blockers.push(
                &dict(&[
                    ("center", Vector2::new(b.center[0], b.center[1]).to_variant()),
                    ("size", Vector2::new(b.size[0], b.size[1]).to_variant()),
                    ("height", b.height.to_variant()),
                ])
                .to_variant(),
            );
        }
        let mut portals = VarArray::new();
        for p in &z.portals {
            let to = s.pack.zone(&p.to_zone).map_or(p.to_zone.clone(), |z| z.name.clone());
            portals.push(
                &dict(&[
                    ("pos", v3(p.pos).to_variant()),
                    ("radius", p.radius.to_variant()),
                    ("name", if p.name.is_empty() { to } else { p.name.clone() }.to_variant()),
                ])
                .to_variant(),
            );
        }
        dict(&[
            ("id", z.id.to_variant()),
            ("name", z.name.to_variant()),
            ("layer", (s.layer as i64).to_variant()),
            ("half_extent", Vector2::new(z.half_extent[0], z.half_extent[1]).to_variant()),
            ("ground_color", z.ground_color.to_variant()),
            ("scene", z.scene.to_variant()),
            ("blockers", blockers.to_variant()),
            ("portals", portals.to_variant()),
        ])
    }

    /// Every entity in view, remote ones interpolated 100 ms back.
    #[func]
    fn entities(&self) -> VarArray {
        let mut out = VarArray::new();
        let Some(s) = &self.session else { return out };
        let server_now = s.server_now();
        for (id, e) in &s.entities {
            let (pos, facing) = if Some(*id) == s.me {
                (s.pos, s.facing)
            } else {
                s.interpolated(*id).unwrap_or(([0.0; 3], 0.0))
            };
            let mut auras = VarArray::new();
            for (aura, expires) in e.state.auras() {
                let name = s
                    .pack
                    .aura(*aura)
                    .map_or("?".to_string(), |a| a.name.clone());
                let remaining = (*expires as f64 - server_now).max(0.0);
                auras.push(
                    &dict(&[
                        ("name", name.to_variant()),
                        ("remaining_ms", remaining.to_variant()),
                    ])
                    .to_variant(),
                );
            }
            let (cast_name, cast_progress) = match s.remote_cast(*id) {
                Some((ab, p)) => (s.pack.ability(ab).map_or(String::new(), |a| a.name.clone()), p),
                None => (String::new(), 0.0),
            };
            let (kind, model, color, scale) = match e.kind() {
                EntityKind::Player => {
                    let c = &s.pack.classes[e.state.def as usize % s.pack.classes.len()];
                    ("player", c.model.clone(), "#e6c04d".to_string(), 1.0f32)
                }
                EntityKind::Mob => {
                    let m = &s.pack.mobs[e.state.def as usize % s.pack.mobs.len()];
                    ("mob", m.model.clone(), m.color.clone(), m.scale)
                }
            };
            out.push(
                &dict(&[
                    ("id", (*id as i64).to_variant()),
                    ("kind", kind.to_variant()),
                    ("name", s.entity_name(*id).to_variant()),
                    ("level", (e.state.level as i64).to_variant()),
                    ("is_me", (Some(*id) == s.me).to_variant()),
                    ("pos", v3(pos).to_variant()),
                    ("facing", facing.to_variant()),
                    ("hp", (e.state.hp as f32).to_variant()),
                    ("max_hp", (e.state.max_hp as f32).to_variant()),
                    ("dead", e.dead().to_variant()),
                    ("in_combat", e.has_flag(flags::IN_COMBAT).to_variant()),
                    ("evading", e.has_flag(flags::EVADING).to_variant()),
                    ("passive", e.has_flag(flags::PASSIVE).to_variant()),
                    ("moving", e.has_flag(flags::MOVING).to_variant()),
                    ("model", model.to_variant()),
                    ("color", color.to_variant()),
                    ("scale", scale.to_variant()),
                    ("cast_name", cast_name.to_variant()),
                    ("cast_progress", cast_progress.to_variant()),
                    ("auras", auras.to_variant()),
                ])
                .to_variant(),
            );
        }
        out
    }

    /// Drains events since the last call: {type: "hit"|"died"|"error"|"log"|"confirmed", ...}.
    #[func]
    fn take_events(&mut self) -> VarArray {
        let mut out = VarArray::new();
        let Some(s) = &mut self.session else {
            return out;
        };
        for e in s.take_events() {
            let d = match e {
                ClientEvent::Hit {
                    target,
                    ability,
                    aura,
                    amount,
                    crit,
                    heal,
                    mine,
                    on_me,
                    ..
                } => dict(&[
                    ("type", "hit".to_variant()),
                    ("target", (target as i64).to_variant()),
                    ("name", s.hit_name(ability, aura).to_variant()),
                    ("amount", amount.to_variant()),
                    ("crit", crit.to_variant()),
                    ("heal", heal.to_variant()),
                    ("periodic", aura.is_some().to_variant()),
                    ("mine", mine.to_variant()),
                    ("on_me", on_me.to_variant()),
                ]),
                ClientEvent::Died { entity, killer } => dict(&[
                    ("type", "died".to_variant()),
                    ("entity", (entity as i64).to_variant()),
                    ("killer", (killer as i64).to_variant()),
                    ("name", s.entity_name(entity).to_variant()),
                    ("me", (Some(entity) == s.me).to_variant()),
                ]),
                ClientEvent::Error(m) => {
                    dict(&[("type", "error".to_variant()), ("text", m.to_variant())])
                }
                ClientEvent::Log(m) => {
                    dict(&[("type", "log".to_variant()), ("text", m.to_variant())])
                }
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

    /// {rtt_ms, loss, last_queue_gap_ms (-1 if none), down_kbps}
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
            ("entities", (s.entities.len() as i64).to_variant()),
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
