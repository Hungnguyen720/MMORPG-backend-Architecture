//! What a character keeps between sessions: experience, levels, loot and
//! inventory, plus chat and party plumbing. Inventory changes are saved the
//! moment they happen; progress and position are written behind.

use combat_core::content::{EquipSlot, ItemStats};
use combat_core::protocol::{ChatChannel, ItemStack, PartyMember, ServerMsg};
use combat_core::{EntityId, EntityKind, ITEM_ABILITY, Ms, channel};
use persistence::{CharRecord, EQUIP_SLOT_BASE, ItemRow};
use renet::ClientId;
use services::Party;

use crate::svc::{SocialOp, SvcReq};
use crate::zone::{Handoff, Zone};

pub const N_EQUIP: usize = EquipSlot::ALL.len();
/// Party members this close to a kill share its experience.
pub const PARTY_SHARE_RANGE: f32 = 80.0;
pub const SAY_RANGE: f32 = 40.0;
const CHAT_MAX_LEN: usize = 200;
const CHAT_MIN_INTERVAL_MS: Ms = 400;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inventory {
    pub bag: Vec<ItemStack>,
    pub equip: [ItemStack; N_EQUIP],
}

impl Inventory {
    pub fn new(slots: usize) -> Self {
        Self {
            bag: vec![None; slots],
            equip: Default::default(),
        }
    }

    /// Rows from the database. Items the pack no longer has are dropped
    /// (with a log line) rather than failing the login.
    pub fn from_rows(pack: &combat_core::ContentPack, rows: &[ItemRow], slots: usize) -> Self {
        let mut inv = Self::new(slots);
        for r in rows {
            let Some(item) = pack.item_index(&r.item) else {
                eprintln!("dropping unknown item '{}' from a saved inventory", r.item);
                continue;
            };
            let stack = Some((item, r.count.max(1) as u32));
            if r.slot >= EQUIP_SLOT_BASE {
                if let Some(s) = inv.equip.get_mut((r.slot - EQUIP_SLOT_BASE) as usize) {
                    *s = stack;
                }
            } else if let Some(s) = inv.bag.get_mut(r.slot as usize) {
                *s = stack;
            }
        }
        inv
    }

    pub fn to_rows(&self, pack: &combat_core::ContentPack) -> Vec<ItemRow> {
        let row = |slot: i16, s: &ItemStack| {
            s.map(|(item, count)| ItemRow {
                slot,
                item: pack.items[item as usize].id.clone(),
                count: count as i32,
            })
        };
        let bag = self.bag.iter().enumerate().filter_map(|(i, s)| row(i as i16, s));
        let eq = self
            .equip
            .iter()
            .enumerate()
            .filter_map(|(i, s)| row(EQUIP_SLOT_BASE + i as i16, s));
        bag.chain(eq).collect()
    }

    /// Add up to `count`, topping up stacks first. Returns what did not fit.
    pub fn add(&mut self, pack: &combat_core::ContentPack, item: u16, mut count: u32) -> u32 {
        let max = pack.items[item as usize].stack.max(1);
        for (it, n) in self.bag.iter_mut().flatten() {
            if *it == item && *n < max {
                let k = (max - *n).min(count);
                *n += k;
                count -= k;
            }
        }
        for s in self.bag.iter_mut() {
            if count == 0 {
                break;
            }
            if s.is_none() {
                let k = max.min(count);
                *s = Some((item, k));
                count -= k;
            }
        }
        count
    }

    #[cfg(test)]
    pub fn count(&self, item: u16) -> u32 {
        self.bag
            .iter()
            .chain(self.equip.iter())
            .flatten()
            .filter(|(i, _)| *i == item)
            .map(|(_, n)| n)
            .sum()
    }

    pub fn gear_stats(&self, pack: &combat_core::ContentPack) -> ItemStats {
        let mut s = ItemStats::default();
        for (item, _) in self.equip.iter().flatten() {
            let st = &pack.items[*item as usize].stats;
            s.max_hp += st.max_hp;
            s.max_mana += st.max_mana;
            s.spell_power += st.spell_power;
        }
        s
    }
}

impl Zone {
    /// Base class stats at the current level plus equipped gear.
    pub fn recalc_stats(&mut self, id: EntityId) {
        let pack = self.pack.clone();
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let Some(p) = e.player.as_ref() else {
            return;
        };
        let mut stats = pack.classes[p.class as usize].stats_at(e.level);
        let g = p.inv.gear_stats(&pack);
        stats.max_hp += g.max_hp;
        stats.max_mana += g.max_mana;
        stats.spell_power += g.spell_power;
        e.stats = stats;
        e.hp = e.hp.min(stats.max_hp);
        e.caster.max_mana = stats.max_mana;
        e.caster.mana = e.caster.mana.min(stats.max_mana);
    }

    pub fn send_bulk(&mut self, client: ClientId, msg: ServerMsg) {
        self.outbox.push(crate::zone::Outgoing {
            client,
            channel: channel::BULK,
            bytes: bytes::Bytes::from(combat_core::protocol::encode(&msg)),
        });
    }

    pub fn system(&mut self, client: ClientId, text: impl Into<String>) {
        self.send_bulk(
            client,
            ServerMsg::Chat {
                channel: ChatChannel::System,
                from: String::new(),
                text: text.into(),
            },
        );
    }

    pub fn send_xp(&mut self, id: EntityId, gained: u32) {
        let Some(e) = self.entities.get(&id) else {
            return;
        };
        let (Some(c), Some(p)) = (e.client(), e.player.as_ref()) else {
            return;
        };
        let msg = ServerMsg::Xp {
            level: e.level,
            xp: p.xp,
            next: self.pack.xp_to_next(e.level).unwrap_or(0),
            gained,
        };
        self.send(c, msg);
    }

    pub fn send_inventory(&mut self, id: EntityId) {
        let Some(e) = self.entities.get(&id) else {
            return;
        };
        let (Some(c), Some(p)) = (e.client(), e.player.as_ref()) else {
            return;
        };
        let msg = ServerMsg::Inventory {
            bag: p.inv.bag.clone(),
            equip: p.inv.equip.to_vec(),
        };
        self.send(c, msg);
    }

    pub fn send_party(&mut self, id: EntityId) {
        let Some(e) = self.entities.get(&id) else {
            return;
        };
        let (Some(c), Some(p)) = (e.client(), e.player.as_ref()) else {
            return;
        };
        let members = p
            .party
            .as_ref()
            .map(|party| {
                party
                    .members
                    .iter()
                    .map(|(cid, name)| PartyMember {
                        name: name.clone(),
                        leader: *cid == party.leader,
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.send_bulk(c, ServerMsg::Party { members });
    }

    /// Queue an immediate inventory save (no-op for dev-mode players).
    fn save_inventory(&mut self, id: EntityId, economy: Vec<(String, String, String, i32)>) {
        let Some(p) = self.entities.get(&id).and_then(|e| e.player.as_ref()) else {
            return;
        };
        if p.character == 0 {
            return;
        }
        let req = SvcReq::SaveInv {
            character: p.character,
            items: p.inv.to_rows(&self.pack),
            economy,
        };
        self.svc.push(req);
    }

    /// Progress and position as they should be saved now. A player mid
    /// handoff is saved at its destination.
    pub fn char_record(&self, id: EntityId) -> Option<CharRecord> {
        let e = self.entities.get(&id)?;
        let p = e.player.as_ref()?;
        if p.character == 0 {
            return None;
        }
        let (zone, pos) = match &p.handoff {
            Handoff::Pending { zone, pos } | Handoff::Sent { zone, pos, .. } => {
                (zone.clone(), *pos)
            }
            Handoff::None => (self.zone_def().id.clone(), e.pos),
        };
        Some(CharRecord {
            id: p.character,
            account_id: p.account,
            name: e.name.clone(),
            class: self.pack.classes[p.class as usize].id.clone(),
            level: e.level as i32,
            xp: p.xp as i64,
            zone,
            pos,
            hp: if e.dead { 0.0 } else { e.hp },
            mana: e.caster.mana,
            items: Vec::new(),
        })
    }

    pub fn save_progress(&mut self, id: EntityId) {
        if let Some(record) = self.char_record(id) {
            self.svc.push(SvcReq::SaveChar { record });
            if let Some(p) = self.entities.get_mut(&id).and_then(|e| e.player.as_mut()) {
                p.dirty = false;
            }
        }
    }

    /// Write-behind: every player whose progress changed since the last save.
    pub fn save_dirty(&mut self) {
        let ids: Vec<EntityId> = self
            .entities
            .values()
            .filter(|e| e.player.as_ref().is_some_and(|p| p.dirty && p.character != 0))
            .map(|e| e.id)
            .collect();
        for id in ids {
            self.save_progress(id);
        }
    }

    pub fn grant_xp(&mut self, id: EntityId, amount: u32) {
        let pack = self.pack.clone();
        let amount = (amount as f32 * pack.tenant.xp_rate).round() as u32;
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let Some(p) = e.player.as_mut() else {
            return;
        };
        if amount == 0 || e.level >= pack.level_cap() {
            return;
        }
        p.xp += amount;
        p.dirty = true;
        let mut leveled = false;
        while let Some(need) = pack.xp_to_next(e.level) {
            if p.xp < need {
                break;
            }
            p.xp -= need;
            e.level += 1;
            leveled = true;
        }
        if e.level >= pack.level_cap() {
            p.xp = 0;
        }
        let (level, name) = (e.level, e.name.clone());
        if leveled {
            self.recalc_stats(id);
            let e = self.entities.get_mut(&id).unwrap();
            e.hp = e.stats.max_hp;
            e.caster.mana = e.caster.max_mana;
            let c = e.client().unwrap();
            self.system(c, format!("You reached level {level}!"));
            if self.verbose {
                println!("{name} reached level {level}");
            }
            // Levels are worth saving at once, not on the next write-behind.
            self.save_progress(id);
        }
        self.send_xp(id, amount);
    }

    /// Experience and personal loot for everyone who fought the mob, shared
    /// with their party members nearby.
    pub fn on_mob_killed(&mut self, mob: EntityId, tapped_by: &[EntityId], t: Ms) {
        let pack = self.pack.clone();
        let Some(m) = self.entities.get(&mob) else {
            return;
        };
        let def = &pack.mobs[m.def as usize];
        if def.dummy {
            return;
        }
        let (mob_pos, mob_level) = (m.pos, def.level);
        // Who gets a share: tappers plus their party members in range,
        // grouped by party.
        let mut groups: Vec<(i64, Vec<EntityId>)> = Vec::new();
        for &tid in tapped_by {
            let Some(te) = self.entities.get(&tid) else {
                continue;
            };
            let Some(tp) = te.player.as_ref() else {
                continue;
            };
            let key = tp.party.as_ref().map_or(-(tid as i64), |p| p.leader);
            if groups.iter().any(|g| g.0 == key) {
                continue;
            }
            let mut members = vec![tid];
            if let Some(party) = &tp.party {
                for (cid, _) in &party.members {
                    if let Some(&eid) = self.by_character.get(cid)
                        && eid != tid
                        && self.entities.get(&eid).is_some_and(|e| {
                            combat_core::movement::dist_xz(e.pos, mob_pos) <= PARTY_SHARE_RANGE
                        })
                    {
                        members.push(eid);
                    }
                }
            }
            groups.push((key, members));
        }
        for (_, members) in groups {
            let n = members.len() as f32;
            let bonus = 1.0 + 0.1 * (n - 1.0);
            for id in members {
                let level = self.entities[&id].level;
                // Grey mobs give nothing; higher ones a little more.
                let diff = mob_level as f32 - level as f32;
                let scale = if diff <= -5.0 {
                    0.0
                } else {
                    (1.0 + 0.1 * diff).clamp(0.2, 1.5)
                };
                let xp = (def.xp as f32 * scale * bonus / n).round() as u32;
                self.grant_xp(id, xp);
                if let Some(table) = def.loot.as_ref().and_then(|l| pack.loot_index(l)) {
                    let mut drops = Vec::new();
                    for entry in &pack.loot_tables[table as usize].entries {
                        if self.rand01() < entry.chance {
                            let span = entry.max.saturating_sub(entry.min) + 1;
                            let count = entry.min + (self.rand01() * span as f32) as u32;
                            let item = pack.item_index(&entry.item).expect("validated");
                            drops.push((item, count.min(entry.max).max(1)));
                        }
                    }
                    if !drops.is_empty() {
                        self.give_items(id, drops, &format!("loot:{mob}:{t}"));
                    }
                }
            }
        }
    }

    /// Put items in a player's bag and save at once. `source` makes the
    /// economy log key unique and retry-safe.
    pub fn give_items(&mut self, id: EntityId, items: Vec<(u16, u32)>, source: &str) {
        let pack = self.pack.clone();
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let Some(p) = e.player.as_mut() else {
            return;
        };
        let character = p.character;
        let mut got = Vec::new();
        let mut lost = false;
        let mut economy = Vec::new();
        for (item, count) in items {
            let left = p.inv.add(&pack, item, count);
            lost |= left > 0;
            let added = count - left;
            if added == 0 {
                continue;
            }
            got.push((item, added));
            let def = &pack.items[item as usize];
            if def.is_economy_critical() {
                economy.push((
                    format!("{source}:{character}:{}", def.id),
                    "loot".to_string(),
                    def.id.clone(),
                    added as i32,
                ));
            }
        }
        let c = e.client().unwrap();
        if lost {
            self.system(c, "Your bags are full.");
        }
        if got.is_empty() {
            return;
        }
        self.send(c, ServerMsg::Looted { items: got });
        self.send_inventory(id);
        self.save_inventory(id, economy);
    }

    /// Inventory requests from the client.
    pub fn handle_item(&mut self, id: EntityId, msg: &combat_core::protocol::ClientMsg, now: Ms) {
        use combat_core::protocol::ClientMsg;
        let pack = self.pack.clone();
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let dead = e.dead;
        let c = e.client().unwrap();
        let p = e.player.as_mut().unwrap();
        match *msg {
            ClientMsg::UseItem { slot } => {
                let Some(Some((item, _))) = p.inv.bag.get(slot as usize).copied() else {
                    return;
                };
                let def = &pack.items[item as usize];
                if let Some(eslot) = def.equip {
                    // Equip, swapping with whatever was in that slot.
                    let i = eslot.index();
                    let old = p.inv.equip[i].take();
                    p.inv.equip[i] = p.inv.bag[slot as usize].take();
                    p.inv.bag[slot as usize] = old;
                    self.recalc_stats(id);
                    self.send_inventory(id);
                    self.save_inventory(id, Vec::new());
                    return;
                }
                if def.on_use.is_empty() {
                    return;
                }
                if dead {
                    self.system(c, "You can't do that while dead.");
                    return;
                }
                if let Some(cd) = p.item_cd.iter().find(|(i, _)| *i == item)
                    && cd.1 > now
                {
                    self.system(c, "That item isn't ready yet.");
                    return;
                }
                p.item_cd.retain(|(i, _)| *i != item);
                p.item_cd.push((item, now + def.use_cooldown_ms as Ms));
                if let Some((_, n)) = p.inv.bag[slot as usize].as_mut() {
                    *n -= 1;
                    if *n == 0 {
                        p.inv.bag[slot as usize] = None;
                    }
                }
                let effects = def.on_use.clone();
                let (sp, crit) = (e.stats.spell_power, 0.0);
                self.apply_effects(id, &effects, ITEM_ABILITY, id, sp, crit, now);
                self.send_inventory(id);
                self.save_inventory(id, Vec::new());
            }
            ClientMsg::Unequip { equip_slot } => {
                let i = equip_slot as usize;
                if p.inv.equip.get(i).copied().flatten().is_none() {
                    return;
                }
                let Some(free) = p.inv.bag.iter().position(|s| s.is_none()) else {
                    self.system(c, "Your bags are full.");
                    return;
                };
                p.inv.bag[free] = p.inv.equip[i].take();
                self.recalc_stats(id);
                self.send_inventory(id);
                self.save_inventory(id, Vec::new());
            }
            ClientMsg::Destroy { slot } => {
                let Some(s) = p.inv.bag.get_mut(slot as usize) else {
                    return;
                };
                let Some((item, count)) = s.take() else {
                    return;
                };
                let def = &pack.items[item as usize];
                let economy = if def.is_economy_critical() {
                    vec![(
                        format!("destroy:{}:{now}:{}", p.character, def.id),
                        "destroy".to_string(),
                        def.id.clone(),
                        count as i32,
                    )]
                } else {
                    Vec::new()
                };
                self.send_inventory(id);
                self.save_inventory(id, economy);
            }
            _ => {}
        }
    }

    /// Chat and party requests from the client.
    pub fn handle_social(&mut self, id: EntityId, msg: combat_core::protocol::ClientMsg, now: Ms) {
        use combat_core::protocol::ClientMsg;
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let name = e.name.clone();
        let pos = e.pos;
        let c = e.client().unwrap();
        let p = e.player.as_mut().unwrap();
        let character = p.character;
        if let ClientMsg::Chat { .. } = msg {
            if now < p.last_chat + CHAT_MIN_INTERVAL_MS {
                self.system(c, "You are chatting too fast.");
                return;
            }
            p.last_chat = now;
        }
        let online = character != 0;
        match msg {
            ClientMsg::Chat { channel, text } => {
                let text: String = text.chars().take(CHAT_MAX_LEN).collect();
                let text = text.trim().to_string();
                if text.is_empty() {
                    return;
                }
                match channel {
                    ChatChannel::Say => {
                        let hearers: Vec<ClientId> = self
                            .entities
                            .values()
                            .filter(|o| {
                                o.kind == EntityKind::Player
                                    && combat_core::movement::dist_xz(o.pos, pos) <= SAY_RANGE
                            })
                            .filter_map(|o| o.client())
                            .collect();
                        for h in hearers {
                            self.send_bulk(
                                h,
                                ServerMsg::Chat {
                                    channel: ChatChannel::Say,
                                    from: name.clone(),
                                    text: text.clone(),
                                },
                            );
                        }
                    }
                    ChatChannel::World if online => {
                        self.svc.push(SvcReq::WorldChat { from: name, text });
                    }
                    ChatChannel::Party if online => {
                        self.svc.push(SvcReq::Social {
                            character,
                            name,
                            op: SocialOp::PartyChat(text),
                        });
                    }
                    ChatChannel::Whisper(to) if online => {
                        self.svc.push(SvcReq::Social {
                            character,
                            name,
                            op: SocialOp::Whisper(to, text),
                        });
                    }
                    ChatChannel::System => {}
                    _ => self.system(c, "That channel needs the login service."),
                }
            }
            ClientMsg::PartyInvite { name: to } if online => self.svc.push(SvcReq::Social {
                character,
                name,
                op: SocialOp::Invite(to),
            }),
            ClientMsg::PartyAccept if online => self.svc.push(SvcReq::Social {
                character,
                name,
                op: SocialOp::Accept,
            }),
            ClientMsg::PartyLeave if online => self.svc.push(SvcReq::Social {
                character,
                name,
                op: SocialOp::Leave,
            }),
            _ => self.system(c, "Parties need the login service."),
        }
    }

    /// An event from the tenant's message bus, for every player here or one.
    pub fn on_bus(&mut self, msg: services::BusMsg) {
        use services::{BusMsg, CharEvent};
        match msg {
            BusMsg::WorldChat { from, text } => {
                let all: Vec<ClientId> = self.clients.keys().copied().collect();
                for c in all {
                    self.send_bulk(
                        c,
                        ServerMsg::Chat {
                            channel: ChatChannel::World,
                            from: from.clone(),
                            text: text.clone(),
                        },
                    );
                }
            }
            BusMsg::ToChar { character, payload } => {
                let Some(&id) = self.by_character.get(&character) else {
                    return;
                };
                let Some(c) = self.entities.get(&id).and_then(|e| e.client()) else {
                    return;
                };
                match payload {
                    CharEvent::PartyInvite { from_name, .. } => {
                        self.send_bulk(c, ServerMsg::PartyInvite { from: from_name })
                    }
                    CharEvent::PartyChanged { party } => {
                        self.set_party(id, party);
                    }
                    CharEvent::PartyChat { from, text } => self.send_bulk(
                        c,
                        ServerMsg::Chat {
                            channel: ChatChannel::Party,
                            from,
                            text,
                        },
                    ),
                    CharEvent::Whisper { from, text } => self.send_bulk(
                        c,
                        ServerMsg::Chat {
                            channel: ChatChannel::Whisper(from.clone()),
                            from,
                            text,
                        },
                    ),
                    CharEvent::System { text } => self.system(c, text),
                }
            }
        }
    }

    pub fn set_party(&mut self, id: EntityId, party: Option<Party>) {
        if let Some(p) = self.entities.get_mut(&id).and_then(|e| e.player.as_mut()) {
            p.party = party;
        }
        self.send_party(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn pack() -> combat_core::ContentPack {
        combat_core::ContentPack::load_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tenants/default"
        ))
        .unwrap()
    }

    #[test]
    fn inventory_stacks_and_round_trips() {
        let pack = pack();
        let pelt = pack.item_index("wolf_pelt").unwrap();
        let staff = pack.item_index("elderwood_staff").unwrap();
        let mut inv = Inventory::new(3);
        assert_eq!(inv.add(&pack, pelt, 25), 0); // 20 + 5
        assert_eq!(inv.add(&pack, staff, 1), 0);
        assert_eq!(inv.add(&pack, staff, 1), 1); // full
        assert_eq!(inv.count(pelt), 25);
        inv.equip[EquipSlot::Weapon.index()] = inv.bag[2].take();
        let rows = inv.to_rows(&pack);
        assert_eq!(Inventory::from_rows(&pack, &rows, 3), inv);
        assert_eq!(inv.gear_stats(&pack).spell_power, 45.0);
    }

    #[test]
    fn kills_grant_xp_levels_and_loot() {
        let pack = Arc::new(pack());
        let idx = pack.zone_index("elderwood").unwrap();
        let mut z = Zone::new(pack.clone(), idx, 1, 0);
        let p = z.add_player(1, "A".into(), 0, 1, None, 0);
        let wolf_def = pack.mob_index("forest_wolf").unwrap();
        let wolves: Vec<EntityId> = z
            .entities
            .values()
            .filter(|e| e.def == wolf_def && e.kind == EntityKind::Mob)
            .map(|e| e.id)
            .collect();
        let hp1 = z.entities[&p].stats.max_hp;
        for (i, w) in wolves.iter().enumerate() {
            z.damage(p, *w, 1e6, 0, None, false, 100 + i as Ms);
        }
        let e = &z.entities[&p];
        assert!(e.level >= 2, "level {}", e.level);
        assert!(e.stats.max_hp > hp1);
        let pelt = pack.item_index("wolf_pelt").unwrap();
        assert!(e.player.as_ref().unwrap().inv.count(pelt) > 0);
    }
}
