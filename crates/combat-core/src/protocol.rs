//! Reliable wire messages (channels COMBAT and BULK), bincode encoded. The
//! two 20 Hz messages, movement and snapshots, use the hand-packed codec in
//! `codec` instead.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{AbilityId, AuraId, EntityId, FailReason, Ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Channel COMBAT. `queued` means the client pressed inside the spell
    /// queue window and expects the server to hold it.
    /// `move_seq` is the last Move sent before the press: only newer moves
    /// can interrupt this cast on the server.
    Cast {
        seq: u32,
        ability: AbilityId,
        target: EntityId,
        queued: bool,
        move_seq: u32,
    },
    CancelCast,
    /// Dead players come back at the zone's graveyard.
    Release,
    // --- Channel BULK: inventory, chat, party ---
    /// Use (consumable) or equip (gear) the item in a bag slot.
    UseItem {
        slot: u16,
    },
    Unequip {
        equip_slot: u8,
    },
    Destroy {
        slot: u16,
    },
    Chat {
        channel: ChatChannel,
        text: String,
    },
    PartyInvite {
        name: String,
    },
    PartyAccept,
    PartyLeave,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatChannel {
    /// Players within 40 m on the same layer.
    Say,
    /// Everyone online in this game.
    World,
    Party,
    Whisper(String),
    /// Server notices (never sent by clients).
    System,
}

/// One bag or equipment slot: item index in the pack and stack count.
pub type ItemStack = Option<(u16, u32)>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PartyMember {
    pub name: String,
    pub leader: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        you: EntityId,
        content_hash: u64,
        server_time: Ms,
        /// Zone index in the content pack.
        zone: u16,
        layer: u16,
        class: u16,
        name: String,
    },
    /// `queue_gap_us` is set when the cast came from the spell queue: wall
    /// time between the caster becoming ready and the cast actually starting.
    CastStart {
        caster: EntityId,
        seq: u32,
        ability: AbilityId,
        target: EntityId,
        bar_ms: u32,
        queue_gap_us: Option<u32>,
    },
    CastFailed {
        seq: u32,
        reason: FailReason,
    },
    CastStop {
        caster: EntityId,
        seq: u32,
        reason: StopReason,
    },
    Hit {
        source: EntityId,
        target: EntityId,
        ability: AbilityId,
        /// Set for aura ticks.
        aura: Option<AuraId>,
        amount: f32,
        crit: bool,
        heal: bool,
    },
    Died {
        entity: EntityId,
        killer: EntityId,
    },
    /// The server moved us: respawn, failed movement check, zone entry.
    Correction {
        pos: [f32; 3],
    },
    // --- Channel BULK ---
    /// Experience after a change. `gained` is 0 on login.
    Xp {
        level: u32,
        xp: u32,
        /// XP needed for the next level, 0 at the cap.
        next: u32,
        gained: u32,
    },
    /// Full inventory after any change: bag slots then equipment slots.
    Inventory {
        bag: Vec<ItemStack>,
        equip: Vec<ItemStack>,
    },
    /// Items just added to the bag (personal auto-loot).
    Looted {
        items: Vec<(u16, u32)>,
    },
    Chat {
        channel: ChatChannel,
        from: String,
        text: String,
    },
    PartyInvite {
        from: String,
    },
    Party {
        members: Vec<PartyMember>,
    },
    /// Leave this zone server and connect to another with this token.
    /// Sent on portals and when a layer merges away.
    Handoff {
        addr: String,
        token: Vec<u8>,
        zone: u16,
        layer: u16,
    },
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bincode::serialize(msg).expect("protocol messages always serialize")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    bincode::deserialize(bytes).ok()
}
