//! Data-driven content. A tenant reskins the game by shipping a different
//! YAML pack; the engine only knows effect types, never specific abilities.

use serde::{Deserialize, Serialize};

use crate::{AbilityId, AuraId, Ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPack {
    pub class: ClassDef,
    pub abilities: Vec<AbilityDef>,
    #[serde(default)]
    pub auras: Vec<AuraDef>,
    pub dummy: DummyDef,
    #[serde(default)]
    pub world: WorldDef,
    /// Hostile NPC types. Placed in the world by `camps`.
    #[serde(default)]
    pub mobs: Vec<MobDef>,
    #[serde(default)]
    pub camps: Vec<CampDef>,
    /// AI companion types a player can bring.
    #[serde(default)]
    pub pawns: Vec<PawnDef>,
    /// Pawn ids every player gets on joining, until hiring exists.
    #[serde(default)]
    pub pawn_party: Vec<String>,
    /// FNV-1a of the source text, so client and server can detect a mismatch.
    #[serde(skip)]
    pub hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassDef {
    pub name: String,
    pub max_hp: f32,
    pub max_mana: f32,
    pub mana_regen_per_sec: f32,
    pub spell_power: f32,
    pub crit_chance: f32,
    pub move_speed: f32,
    /// Health per second, once out of combat for `OUT_OF_COMBAT_MS`.
    #[serde(default = "default_hp_regen")]
    pub hp_regen_per_sec: f32,
    #[serde(default = "default_respawn")]
    pub respawn_ms: u32,
}

fn default_hp_regen() -> f32 {
    250.0
}
fn default_respawn() -> u32 {
    5000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbilityDef {
    pub id: String,
    pub name: String,
    /// Hard cast time. 0 = instant.
    #[serde(default)]
    pub cast_ms: u32,
    /// Channel duration. Effects apply once per tick instead of on completion.
    #[serde(default)]
    pub channel_ms: u32,
    #[serde(default)]
    pub channel_ticks: u32,
    /// 0 = off the global cooldown.
    #[serde(default = "default_gcd")]
    pub gcd_ms: u32,
    #[serde(default)]
    pub cooldown_ms: u32,
    #[serde(default)]
    pub mana_cost: f32,
    pub range: f32,
    #[serde(default)]
    pub cast_while_moving: bool,
    #[serde(with = "serde_yaml::with::singleton_map_recursive")]
    pub effects: Vec<Effect>,
    /// Hex colour for the action button and cast bar. Purely cosmetic.
    #[serde(default = "default_color")]
    pub color: String,
    /// Animation tag on the shared skeleton and VFX id from the asset pack.
    #[serde(default)]
    pub anim: String,
    #[serde(default)]
    pub vfx: String,
    /// How the ability reaches its victims. Everything except `target`
    /// is aimed at a ground point instead of needing a target.
    #[serde(default)]
    pub delivery: Delivery,
    /// Skillshot: flight speed (m/s) and collision width (m).
    #[serde(default)]
    pub speed: f32,
    #[serde(default)]
    pub width: f32,
    /// Ground: blast radius (m) and the delay before it lands.
    #[serde(default)]
    pub radius: f32,
    #[serde(default)]
    pub delay_ms: u32,
    /// Dash: how far it carries the caster (m).
    #[serde(default)]
    pub dash_m: f32,
    /// Most enemies one ground blast can hit.
    #[serde(default = "default_max_targets")]
    pub max_targets: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Tab-target: resolves on the selected enemy.
    #[default]
    Target,
    /// A projectile along the ground that hits the first enemy it touches.
    Skillshot,
    /// A circle at a point that lands after `delay_ms`.
    Ground,
    /// Moves the caster up to `dash_m` toward the aim point.
    Dash,
}

impl Delivery {
    pub fn aimed(self) -> bool {
        self != Delivery::Target
    }
}

fn default_max_targets() -> u32 {
    8
}

fn default_gcd() -> u32 {
    1500
}
fn default_color() -> String {
    "#8899aa".into()
}

impl AbilityDef {
    pub fn is_channel(&self) -> bool {
        self.channel_ms > 0
    }
    pub fn is_instant(&self) -> bool {
        self.cast_ms == 0 && self.channel_ms == 0
    }
    pub fn on_gcd(&self) -> bool {
        self.gcd_ms > 0
    }
    /// Length of the cast or channel bar.
    pub fn bar_ms(&self) -> Ms {
        if self.is_channel() {
            self.channel_ms as Ms
        } else {
            self.cast_ms as Ms
        }
    }
    pub fn channel_tick_interval(&self) -> Ms {
        (self.channel_ms / self.channel_ticks.max(1)) as Ms
    }
    /// Seconds a skillshot flies before it gives up.
    pub fn flight_ms(&self) -> Ms {
        if self.speed > 0.0 {
            (self.range / self.speed * 1000.0) as Ms
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Damage {
        #[serde(default)]
        school: String,
        base: f32,
        #[serde(default)]
        coeff_spellpower: f32,
    },
    ApplyAura {
        aura: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuraDef {
    pub id: String,
    pub name: String,
    pub duration_ms: u32,
    /// Periodic damage. 0 = no ticks.
    #[serde(default)]
    pub tick_ms: u32,
    #[serde(default)]
    pub tick_base: f32,
    #[serde(default)]
    pub tick_coeff_spellpower: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DummyDef {
    pub name: String,
    pub max_hp: f32,
    /// Heals to full after this long without taking damage.
    pub reset_after_ms: u32,
    pub position: [f32; 3],
    /// Walks a circle of this radius around `position`, so aimed abilities
    /// have something moving to hit. 0 = stands still.
    #[serde(default)]
    pub patrol_radius: f32,
    #[serde(default)]
    pub patrol_speed: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldDef {
    /// Where players appear on joining and after death.
    pub spawn: [f32; 3],
}

impl Default for WorldDef {
    fn default() -> Self {
        Self {
            spawn: [0.0, 0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobDef {
    pub id: String,
    pub name: String,
    pub max_hp: f32,
    pub move_speed: f32,
    pub attack_range: f32,
    pub attack_ms: u32,
    pub attack_damage: f32,
    /// Notices players and pawns this close.
    pub aggro_radius: f32,
    /// Gives up and walks home past this distance from its spawn point.
    pub leash_radius: f32,
    pub respawn_ms: u32,
    /// Visual size relative to a 1.8 m human. Purely cosmetic.
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default = "default_mob_color")]
    pub color: String,
    /// A telegraphed area attack players can step out of.
    #[serde(default)]
    pub slam: Option<SlamDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlamDef {
    pub radius: f32,
    /// How long the warning shows before it lands.
    pub windup_ms: u32,
    pub cooldown_ms: u32,
    pub damage: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampDef {
    pub mob: String,
    pub position: [f32; 3],
    #[serde(default = "one_u32")]
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PawnRole {
    /// Closes to melee and holds the enemy's attention.
    Melee,
    /// Keeps its distance.
    Ranged,
    /// Heals the party, attacks when nobody needs it.
    Healer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PawnDef {
    pub id: String,
    pub name: String,
    pub role: PawnRole,
    pub max_hp: f32,
    pub move_speed: f32,
    pub attack_range: f32,
    pub attack_ms: u32,
    pub attack_damage: f32,
    #[serde(default)]
    pub heal_amount: f32,
    /// Threat per point of damage. Above 1 pulls enemies off the player.
    #[serde(default = "one")]
    pub threat_mult: f32,
    pub respawn_ms: u32,
    #[serde(default = "default_pawn_color")]
    pub color: String,
}

fn one() -> f32 {
    1.0
}
fn one_u32() -> u32 {
    1
}
fn default_mob_color() -> String {
    "#b04a3a".into()
}
fn default_pawn_color() -> String {
    "#4ab06a".into()
}

impl ContentPack {
    pub fn from_yaml(text: &str) -> Result<Self, String> {
        let mut pack: ContentPack = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        pack.hash = fnv1a(text.as_bytes());
        pack.validate()?;
        Ok(pack)
    }

    fn validate(&self) -> Result<(), String> {
        if self.abilities.is_empty() {
            return Err("pack has no abilities".into());
        }
        if self.abilities.len() > u16::MAX as usize || self.auras.len() > u16::MAX as usize {
            return Err("too many abilities or auras".into());
        }
        for a in &self.abilities {
            if a.cast_ms > 0 && a.channel_ms > 0 {
                return Err(format!("{}: cannot be both a cast and a channel", a.id));
            }
            match a.delivery {
                Delivery::Skillshot if a.speed <= 0.0 || a.width <= 0.0 => {
                    return Err(format!("{}: skillshot needs speed and width", a.id));
                }
                Delivery::Ground if a.radius <= 0.0 => {
                    return Err(format!("{}: ground ability needs a radius", a.id));
                }
                Delivery::Dash if a.dash_m <= 0.0 => {
                    return Err(format!("{}: dash needs dash_m", a.id));
                }
                _ => {}
            }
            if a.delivery.aimed() && a.channel_ms > 0 {
                return Err(format!("{}: aimed abilities cannot channel", a.id));
            }
            if a.channel_ms > 0 && a.channel_ticks == 0 {
                return Err(format!("{}: channel needs channel_ticks > 0", a.id));
            }
            for e in &a.effects {
                if let Effect::ApplyAura { aura } = e
                    && self.aura_index(aura).is_none()
                {
                    return Err(format!("{}: unknown aura '{}'", a.id, aura));
                }
            }
        }
        for c in &self.camps {
            if self.mob_index(&c.mob).is_none() {
                return Err(format!("camp at {:?}: unknown mob '{}'", c.position, c.mob));
            }
        }
        for p in &self.pawn_party {
            if self.pawn_index(p).is_none() {
                return Err(format!("pawn_party: unknown pawn '{p}'"));
            }
        }
        for au in &self.auras {
            if au.tick_ms > 0 && au.duration_ms < au.tick_ms {
                return Err(format!("{}: duration shorter than one tick", au.id));
            }
        }
        Ok(())
    }

    pub fn ability(&self, id: AbilityId) -> Option<&AbilityDef> {
        self.abilities.get(id as usize)
    }
    pub fn aura(&self, id: AuraId) -> Option<&AuraDef> {
        self.auras.get(id as usize)
    }
    pub fn mob_index(&self, id: &str) -> Option<u16> {
        self.mobs.iter().position(|m| m.id == id).map(|i| i as u16)
    }
    pub fn pawn_index(&self, id: &str) -> Option<u16> {
        self.pawns.iter().position(|p| p.id == id).map(|i| i as u16)
    }
    pub fn aura_index(&self, id: &str) -> Option<AuraId> {
        self.auras
            .iter()
            .position(|a| a.id == id)
            .map(|i| i as AuraId)
    }
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}
