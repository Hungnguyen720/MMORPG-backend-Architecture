//! GDExtension: exposes `CombatClient` to GDScript. All networking,
//! prediction and interpolation live in `net-client`; this file only
//! converts between Rust and Godot types.

use std::net::ToSocketAddrs;

use combat_core::ContentPack;
use combat_core::protocol::EntityKind;
use godot::prelude::*;
use net_client::{ClientEvent, ClientSession};

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
            };
            let name = match e.kind {
                EntityKind::Player => format!("Player {id}"),
                EntityKind::Dummy => s.pack.dummy.name.clone(),
            };
            out.push(
                &dict(&[
                    ("id", (*id as i64).to_variant()),
                    ("kind", kind.to_variant()),
                    ("name", name.to_variant()),
                    ("is_me", (Some(*id) == s.me).to_variant()),
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

    /// Drains events since the last call: {type: "hit"|"error"|"log"|"confirmed", ...}.
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
                    mine,
                } => {
                    let name = match aura {
                        Some(a) => s.pack.aura(a).map(|a| a.name.clone()),
                        None => s.pack.ability(ability).map(|a| a.name.clone()),
                    }
                    .unwrap_or_default();
                    dict(&[
                        ("type", "hit".to_variant()),
                        ("target", (target as i64).to_variant()),
                        ("name", name.to_variant()),
                        ("amount", amount.to_variant()),
                        ("crit", crit.to_variant()),
                        ("periodic", aura.is_some().to_variant()),
                        ("mine", mine.to_variant()),
                    ])
                }
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

    /// {rtt_ms, loss, last_queue_gap_ms (-1 if none)}
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
