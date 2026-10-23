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
