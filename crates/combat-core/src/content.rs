//! Data-driven content. A tenant reskins the game by shipping a different
//! pack; the engine only knows effect types, never specific abilities, mobs
//! or zones.
//!
//! Source form is a directory of YAML files (`tenants/<id>/*.yaml`). Every
//! file is a partial pack: its top-level keys are merged, so a tenant can
//! split content however it likes. `ContentPack::load_dir` reads that form
//! directly; the pack compiler turns it into the binary `.pack` that servers
//! and clients load in production.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{AbilityId, AuraId, Ms};

/// Bumped whenever the compiled layout changes; old packs are rejected.
pub const PACK_SCHEMA: u32 = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackSource {
    #[serde(default)]
    pub tenant: Option<TenantConfig>,
    #[serde(default)]
    pub classes: Vec<ClassDef>,
    #[serde(default)]
    pub abilities: Vec<AbilityDef>,
    #[serde(default)]
    pub auras: Vec<AuraDef>,
    #[serde(default)]
    pub mobs: Vec<MobDef>,
    #[serde(default)]
    pub zones: Vec<ZoneDef>,
    #[serde(default)]
    pub items: Vec<ItemDef>,
    #[serde(default)]
    pub loot_tables: Vec<LootTableDef>,
}

impl PackSource {
    fn merge(&mut self, other: PackSource) -> Result<(), String> {
        if let Some(t) = other.tenant {
            if self.tenant.is_some() {
                return Err("`tenant` is defined in more than one file".into());
            }
            self.tenant = Some(t);
        }
        self.classes.extend(other.classes);
        self.abilities.extend(other.abilities);
        self.auras.extend(other.auras);
        self.mobs.extend(other.mobs);
        self.zones.extend(other.zones);
        self.items.extend(other.items);
        self.loot_tables.extend(other.loot_tables);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPack {
    pub tenant: TenantConfig,
    pub classes: Vec<ClassDef>,
    pub abilities: Vec<AbilityDef>,
    pub auras: Vec<AuraDef>,
    pub mobs: Vec<MobDef>,
    pub zones: Vec<ZoneDef>,
    pub items: Vec<ItemDef>,
    pub loot_tables: Vec<LootTableDef>,
    /// FNV-1a of the canonical compiled bytes, so client and server can
    /// detect a mismatch. Filled in by `finish`.
    #[serde(skip)]
    pub hash: u64,
    #[serde(skip)]
    index: PackIndex,
}

#[derive(Debug, Clone, Default)]
struct PackIndex {
    abilities: HashMap<String, AbilityId>,
    auras: HashMap<String, AuraId>,
    mobs: HashMap<String, u16>,
    zones: HashMap<String, u16>,
    items: HashMap<String, u16>,
    loot: HashMap<String, u16>,
    classes: HashMap<String, u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// Short machine id, e.g. "default" or "ashfall". Names the database,
    /// Redis key prefix and asset pack.
    pub id: String,
    /// Display name of the game.
    pub name: String,
    #[serde(default = "one")]
    pub xp_rate: f32,
    /// XP needed to go from level N to N+1, index 0 = level 1. Its length
    /// sets the level cap (length + 1).
    pub xp_per_level: Vec<u32>,
    /// Zone new characters start in.
    pub start_zone: String,
    /// Players per layer before the world service opens another copy.
    #[serde(default = "default_layer_cap")]
    pub layer_soft_cap: u32,
    /// Bag slots for a new character.
    #[serde(default = "default_bag")]
    pub bag_slots: u16,
    /// What a new character finds in its bags.
    #[serde(default)]
    pub starting_items: Vec<StartingItem>,
    /// UI theme, read by the client: colours by role. The asset pack can
    /// override any of these with its own theme file.
    #[serde(default)]
    pub theme: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartingItem {
    pub item: String,
    #[serde(default = "one_u32")]
    pub count: u32,
}

fn one() -> f32 {
    1.0
}
fn default_layer_cap() -> u32 {
    150
}
fn default_bag() -> u16 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassDef {
    pub id: String,
    pub name: String,
    pub max_hp: f32,
    pub max_mana: f32,
    pub mana_regen_per_sec: f32,
    /// Health regained per second while out of combat.
    #[serde(default)]
    pub hp_regen_per_sec_ooc: f32,
    pub spell_power: f32,
    pub crit_chance: f32,
    pub move_speed: f32,
    /// Growth per level above 1.
    #[serde(default)]
    pub hp_per_level: f32,
    #[serde(default)]
    pub mana_per_level: f32,
    #[serde(default)]
    pub spell_power_per_level: f32,
    /// Action bar, in key order.
    pub abilities: Vec<String>,
    /// Model tag resolved by the tenant asset pack.
    #[serde(default)]
    pub model: String,
}

impl ClassDef {
    pub fn stats_at(&self, level: u32) -> Stats {
        let l = level.saturating_sub(1) as f32;
        Stats {
            max_hp: self.max_hp + self.hp_per_level * l,
            max_mana: self.max_mana + self.mana_per_level * l,
            spell_power: self.spell_power + self.spell_power_per_level * l,
            crit_chance: self.crit_chance,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub max_hp: f32,
    pub max_mana: f32,
    pub spell_power: f32,
    pub crit_chance: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    #[default]
    Enemy,
    /// Always targets the caster; no target needed.
    Caster,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub target: TargetKind,
    #[serde(default = "yes")]
    pub requires_los: bool,
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
    #[serde(default)]
    pub icon: String,
}

fn default_gcd() -> u32 {
    1500
}
fn default_color() -> String {
    "#8899aa".into()
}
fn yes() -> bool {
    true
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
    Heal {
        base: f32,
        #[serde(default)]
        coeff_spellpower: f32,
    },
    ApplyAura {
        aura: String,
    },
    /// Mana for the caster (potions).
    RestoreMana {
        amount: f32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    pub vfx: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MobDef {
    pub id: String,
    pub name: String,
    #[serde(default = "one_u32")]
    pub level: u32,
    pub max_hp: f32,
    /// Melee hit, before ±5% variance.
    #[serde(default)]
    pub melee_damage: f32,
    #[serde(default = "default_attack_ms")]
    pub attack_ms: u32,
    #[serde(default = "default_melee_range")]
    pub melee_range: f32,
    /// Abilities the mob casts, by priority, using the same rules as players.
    #[serde(default)]
    pub abilities: Vec<String>,
    #[serde(default)]
    pub spell_power: f32,
    #[serde(default)]
    pub mana: f32,
    /// Players closer than this pull the mob. 0 = never aggressive.
    #[serde(default)]
    pub aggro_radius: f32,
    /// Distance from its spawn point at which the mob gives up and resets.
    #[serde(default = "default_leash")]
    pub leash_radius: f32,
    #[serde(default = "default_mob_speed")]
    pub move_speed: f32,
    /// How far it strolls from its spawn point when idle. 0 = stands still.
    #[serde(default)]
    pub wander_radius: f32,
    #[serde(default)]
    pub xp: u32,
    #[serde(default)]
    pub loot: Option<String>,
    /// Training dummies never die, never fight back, and heal when left alone.
    #[serde(default)]
    pub dummy: bool,
    #[serde(default = "default_reset_ms")]
    pub reset_after_ms: u32,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_mob_color")]
    pub color: String,
    /// Model scale, cosmetic.
    #[serde(default = "one")]
    pub scale: f32,
}

fn one_u32() -> u32 {
    1
}
fn default_attack_ms() -> u32 {
    2000
}
fn default_melee_range() -> f32 {
    2.5
}
fn default_leash() -> f32 {
    40.0
}
fn default_mob_speed() -> f32 {
    6.0
}
fn default_reset_ms() -> u32 {
    10_000
}
fn default_mob_color() -> String {
    "#cc5544".into()
}

/// Axis-aligned wall or obstacle on the ground plane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocker {
    /// Centre on the ground plane: [x, z].
    pub center: [f32; 2],
    /// Width along x and depth along z.
    pub size: [f32; 2],
    #[serde(default = "default_blocker_height")]
    pub height: f32,
}

fn default_blocker_height() -> f32 {
    3.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnDef {
    pub mob: String,
    pub pos: [f32; 3],
    #[serde(default = "one_u32")]
    pub count: u32,
    /// Spread `count` mobs over a circle of this radius.
    #[serde(default)]
    pub radius: f32,
    #[serde(default = "default_respawn")]
    pub respawn_ms: u32,
}

fn default_respawn() -> u32 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortalDef {
    pub pos: [f32; 3],
    #[serde(default = "default_portal_radius")]
    pub radius: f32,
    pub to_zone: String,
    pub to_pos: [f32; 3],
    #[serde(default)]
    pub name: String,
}

fn default_portal_radius() -> f32 {
    3.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneDef {
    pub id: String,
    pub name: String,
    /// The zone spans [-x, x] by [-z, z] metres around the origin.
    pub half_extent: [f32; 2],
    pub player_spawn: [f32; 3],
    /// Where dead players come back.
    pub graveyard: [f32; 3],
    #[serde(default)]
    pub blockers: Vec<Blocker>,
    #[serde(default)]
    pub spawns: Vec<SpawnDef>,
    #[serde(default)]
    pub portals: Vec<PortalDef>,
    /// Look of the zone, resolved by the asset pack (ground, sky, props).
    #[serde(default)]
    pub scene: String,
    #[serde(default = "default_ground")]
    pub ground_color: String,
}

fn default_ground() -> String {
    "#2a312c".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rarity {
    Poor,
    #[default]
    Common,
    Uncommon,
    Rare,
    Epic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquipSlot {
    Head,
    Chest,
    Legs,
    Weapon,
    Trinket,
}

impl EquipSlot {
    pub const ALL: [EquipSlot; 5] = [
        EquipSlot::Head,
        EquipSlot::Chest,
        EquipSlot::Legs,
        EquipSlot::Weapon,
        EquipSlot::Trinket,
    ];
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap()
    }
    pub fn name(self) -> &'static str {
        match self {
            EquipSlot::Head => "head",
            EquipSlot::Chest => "chest",
            EquipSlot::Legs => "legs",
            EquipSlot::Weapon => "weapon",
            EquipSlot::Trinket => "trinket",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemStats {
    #[serde(default)]
    pub max_hp: f32,
    #[serde(default)]
    pub max_mana: f32,
    #[serde(default)]
    pub spell_power: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub rarity: Rarity,
    #[serde(default = "one_u32")]
    pub stack: u32,
    #[serde(default)]
    pub equip: Option<EquipSlot>,
    #[serde(default)]
    pub stats: ItemStats,
    /// Using the item applies these effects to yourself (potions, food).
    #[serde(default, with = "serde_yaml::with::singleton_map_recursive")]
    pub on_use: Vec<Effect>,
    #[serde(default)]
    pub use_cooldown_ms: u32,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
}

impl ItemDef {
    /// Rare and better loot is written to the database the moment it drops.
    pub fn is_economy_critical(&self) -> bool {
        matches!(self.rarity, Rarity::Rare | Rarity::Epic)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LootTableDef {
    pub id: String,
    pub entries: Vec<LootEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LootEntry {
    pub item: String,
    /// 0..1 per kill, rolled independently per entry.
    pub chance: f32,
    #[serde(default = "one_u32")]
    pub min: u32,
    #[serde(default = "one_u32")]
    pub max: u32,
}

impl ContentPack {
    /// Read every `*.yaml` in `dir`, merged in file-name order.
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = dir.as_ref();
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(format!("{}: no .yaml files", dir.display()));
        }
        let mut texts = Vec::new();
        for f in &files {
            let text = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
            texts.push((f.file_name().unwrap().to_string_lossy().to_string(), text));
        }
        Self::from_yaml_files(&texts)
    }

    /// Merge partial packs given as (file name, YAML text).
    pub fn from_yaml_files(files: &[(String, String)]) -> Result<Self, String> {
        let mut src = PackSource::default();
        for (name, text) in files {
            let part: PackSource =
                serde_yaml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
            src.merge(part).map_err(|e| format!("{name}: {e}"))?;
        }
        Self::from_source(src)
    }

    pub fn from_source(src: PackSource) -> Result<Self, String> {
        let tenant = src
            .tenant
            .ok_or("no `tenant:` section (tenant id, name, xp table)")?;
        let mut pack = ContentPack {
            tenant,
            classes: src.classes,
            abilities: src.abilities,
            auras: src.auras,
            mobs: src.mobs,
            zones: src.zones,
            items: src.items,
            loot_tables: src.loot_tables,
            hash: 0,
            index: PackIndex::default(),
        };
        pack.finish()?;
        Ok(pack)
    }

    /// The compiled form: what the pack compiler writes and clients load.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MMOPACK\0");
        out.extend_from_slice(&PACK_SCHEMA.to_le_bytes());
        // JSON body: the content types use YAML-friendly serde attributes
        // that a schema-less binary format cannot read back. Packs are a
        // few tens of KB, downloaded once per content update.
        out.extend(serde_json::to_vec(self).expect("pack serializes"));
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 12 || &bytes[..8] != b"MMOPACK\0" {
            return Err("not a compiled content pack".into());
        }
        let schema = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if schema != PACK_SCHEMA {
            return Err(format!(
                "pack schema {schema}, engine expects {PACK_SCHEMA}: recompile the pack"
            ));
        }
        let mut pack: ContentPack =
            serde_json::from_slice(&bytes[12..]).map_err(|e| format!("corrupt pack: {e}"))?;
        pack.finish()?;
        Ok(pack)
    }

    /// Load a compiled `.pack` file or a YAML source directory.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if path.is_dir() {
            Self::load_dir(path)
        } else {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Self::from_bytes(&bytes)
        }
    }

    fn finish(&mut self) -> Result<(), String> {
        fn index<T>(
            kind: &str,
            items: &[T],
            id: impl Fn(&T) -> &str,
        ) -> Result<HashMap<String, u16>, String> {
            if items.len() > u16::MAX as usize {
                return Err(format!("too many {kind}"));
            }
            let mut m = HashMap::new();
            for (i, it) in items.iter().enumerate() {
                if m.insert(id(it).to_string(), i as u16).is_some() {
                    return Err(format!("duplicate {kind} id '{}'", id(it)));
                }
            }
            Ok(m)
        }
        self.index = PackIndex {
            abilities: index("abilities", &self.abilities, |a| &a.id)?,
            auras: index("auras", &self.auras, |a| &a.id)?,
            mobs: index("mobs", &self.mobs, |a| &a.id)?,
            zones: index("zones", &self.zones, |a| &a.id)?,
            items: index("items", &self.items, |a| &a.id)?,
            loot: index("loot tables", &self.loot_tables, |a| &a.id)?,
            classes: index("classes", &self.classes, |a| &a.id)?,
        };
        self.validate()?;
        self.hash = 0;
        self.hash = crate::content::fnv1a(&self.to_bytes());
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        let t = &self.tenant;
        if t.xp_per_level.is_empty() {
            return Err("tenant.xp_per_level is empty".into());
        }
        if self.zone_index(&t.start_zone).is_none() {
            return Err(format!("tenant.start_zone: unknown zone '{}'", t.start_zone));
        }
        if self.classes.is_empty() {
            return Err("pack has no classes".into());
        }
        for si in &t.starting_items {
            if self.item_index(&si.item).is_none() {
                return Err(format!("tenant.starting_items: unknown item '{}'", si.item));
            }
        }
        for c in &self.classes {
            if c.abilities.is_empty() || c.abilities.len() > 9 {
                return Err(format!("class {}: needs 1 to 9 abilities", c.id));
            }
            for a in &c.abilities {
                if self.ability_index(a).is_none() {
                    return Err(format!("class {}: unknown ability '{a}'", c.id));
                }
            }
        }
        for a in &self.abilities {
            if a.cast_ms > 0 && a.channel_ms > 0 {
                return Err(format!("{}: cannot be both a cast and a channel", a.id));
            }
            if a.channel_ms > 0 && a.channel_ticks == 0 {
                return Err(format!("{}: channel needs channel_ticks > 0", a.id));
            }
            self.check_effects(&a.id, &a.effects)?;
        }
        for au in &self.auras {
            if au.tick_ms > 0 && au.duration_ms < au.tick_ms {
                return Err(format!("{}: duration shorter than one tick", au.id));
            }
        }
        for m in &self.mobs {
            for a in &m.abilities {
                if self.ability_index(a).is_none() {
                    return Err(format!("mob {}: unknown ability '{a}'", m.id));
                }
            }
            if let Some(l) = &m.loot
                && self.loot_index(l).is_none()
            {
                return Err(format!("mob {}: unknown loot table '{l}'", m.id));
            }
        }
        for z in &self.zones {
            let inside = |p: [f32; 3]| {
                p[0].abs() <= z.half_extent[0] && p[2].abs() <= z.half_extent[1]
            };
            if z.half_extent[0] * 2.0 > crate::codec::MAX_ZONE_SIZE
                || z.half_extent[1] * 2.0 > crate::codec::MAX_ZONE_SIZE
            {
                return Err(format!(
                    "zone {}: larger than {} m",
                    z.id,
                    crate::codec::MAX_ZONE_SIZE
                ));
            }
            if !inside(z.player_spawn) || !inside(z.graveyard) {
                return Err(format!("zone {}: spawn or graveyard outside the zone", z.id));
            }
            for s in &z.spawns {
                if self.mob_index(&s.mob).is_none() {
                    return Err(format!("zone {}: unknown mob '{}'", z.id, s.mob));
                }
                if !inside(s.pos) {
                    return Err(format!("zone {}: spawn of {} outside the zone", z.id, s.mob));
                }
            }
            for p in &z.portals {
                let Some(to) = self.zone_index(&p.to_zone) else {
                    return Err(format!("zone {}: portal to unknown zone '{}'", z.id, p.to_zone));
                };
                let tz = &self.zones[to as usize];
                if p.to_pos[0].abs() > tz.half_extent[0] || p.to_pos[2].abs() > tz.half_extent[1] {
                    return Err(format!(
                        "zone {}: portal lands outside zone {}",
                        z.id, p.to_zone
                    ));
                }
            }
        }
        for it in &self.items {
            if it.stack == 0 {
                return Err(format!("item {}: stack must be at least 1", it.id));
            }
            if it.equip.is_some() && it.stack > 1 {
                return Err(format!("item {}: equipment cannot stack", it.id));
            }
            self.check_effects(&it.id, &it.on_use)?;
        }
        for lt in &self.loot_tables {
            for e in &lt.entries {
                if self.item_index(&e.item).is_none() {
                    return Err(format!("loot table {}: unknown item '{}'", lt.id, e.item));
                }
                if e.min > e.max || e.max == 0 {
                    return Err(format!("loot table {}: bad min/max for {}", lt.id, e.item));
                }
            }
        }
        Ok(())
    }

    fn check_effects(&self, owner: &str, effects: &[Effect]) -> Result<(), String> {
        for e in effects {
            if let Effect::ApplyAura { aura } = e
                && self.aura_index(aura).is_none()
            {
                return Err(format!("{owner}: unknown aura '{aura}'"));
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
    pub fn ability_index(&self, id: &str) -> Option<AbilityId> {
        self.index.abilities.get(id).copied()
    }
    pub fn aura_index(&self, id: &str) -> Option<AuraId> {
        self.index.auras.get(id).copied()
    }
    pub fn mob_index(&self, id: &str) -> Option<u16> {
        self.index.mobs.get(id).copied()
    }
    pub fn zone_index(&self, id: &str) -> Option<u16> {
        self.index.zones.get(id).copied()
    }
    pub fn item_index(&self, id: &str) -> Option<u16> {
        self.index.items.get(id).copied()
    }
    pub fn loot_index(&self, id: &str) -> Option<u16> {
        self.index.loot.get(id).copied()
    }
    pub fn class_index(&self, id: &str) -> Option<u16> {
        self.index.classes.get(id).copied()
    }
    pub fn zone(&self, id: &str) -> Option<&ZoneDef> {
        self.zone_index(id).map(|i| &self.zones[i as usize])
    }

    /// Ability ids on a class's action bar, in key order.
    pub fn class_bar(&self, class: u16) -> Vec<AbilityId> {
        self.classes[class as usize]
            .abilities
            .iter()
            .map(|a| self.ability_index(a).expect("validated"))
            .collect()
    }

    pub fn level_cap(&self) -> u32 {
        self.tenant.xp_per_level.len() as u32 + 1
    }

    /// XP to reach the next level from `level`, or None at the cap.
    pub fn xp_to_next(&self, level: u32) -> Option<u32> {
        self.tenant
            .xp_per_level
            .get(level.saturating_sub(1) as usize)
            .copied()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_pack_round_trips_with_the_same_hash() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tenants/default");
        let pack = ContentPack::load_dir(dir).unwrap();
        let bytes = pack.to_bytes();
        let back = ContentPack::from_bytes(&bytes).unwrap();
        assert_eq!(back.hash, pack.hash);
        assert_eq!(back.to_bytes(), bytes);
        assert_eq!(back.items.len(), pack.items.len());
        assert!(ContentPack::from_bytes(b"MMOPACK\0\x02\0\0\0{}").is_err());
    }
}
