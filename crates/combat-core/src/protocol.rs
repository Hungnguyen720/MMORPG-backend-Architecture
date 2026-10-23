//! Wire messages. bincode for Phase 0; the hand-packed codec from the
//! architecture doc replaces this once bandwidth matters (Phase 1).

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{AbilityId, AuraId, EntityId, FailReason, Ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Channel STATE, 20 Hz.
    Move {
        seq: u32,
        pos: [f32; 3],
        facing: f32,
        moving: bool,
    },
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
    },
    AuraApplied {
        target: EntityId,
        aura: AuraId,
        duration_ms: u32,
    },
    AuraRemoved {
        target: EntityId,
        aura: AuraId,
    },
    Correction {
        pos: [f32; 3],
    },
    /// Channel STATE, 20 Hz.
    Snapshot {
        server_time: Ms,
        entities: Vec<EntitySnap>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntityKind {
    Player,
    Dummy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySnap {
    pub id: EntityId,
    pub kind: EntityKind,
    pub pos: [f32; 3],
    pub facing: f32,
    pub hp: f32,
    pub max_hp: f32,
    pub mana: f32,
    pub max_mana: f32,
    /// (ability, elapsed ms, bar ms) of the current cast or channel.
    pub casting: Option<(AbilityId, u32, u32)>,
    /// (aura, remaining ms).
    pub auras: Vec<(AuraId, u32)>,
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bincode::serialize(msg).expect("protocol messages always serialize")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    bincode::deserialize(bytes).ok()
}
