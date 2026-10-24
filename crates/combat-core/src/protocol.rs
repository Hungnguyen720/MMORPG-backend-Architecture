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
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bincode::serialize(msg).expect("protocol messages always serialize")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    bincode::deserialize(bytes).ok()
}
