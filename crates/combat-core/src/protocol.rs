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
        /// Ground point (x, z) for aimed abilities. None = at the target.
        aim: Option<[f32; 2]>,
    },
    CancelCast,
    /// Channel COMBAT. Pawns assist whatever their owner has targeted.
    SetTarget {
        target: Option<EntityId>,
    },
    /// Channel COMBAT.
    PawnCommand {
        cmd: PawnCmd,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PawnCmd {
    /// Attack the owner's target now, even if nothing has pulled it.
    Attack,
    /// Break off and return to the owner for a few seconds.
    Regroup,
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
    /// An NPC's basic attack or heal (pawns and mobs have no cast bars).
    Swing {
        source: EntityId,
        target: EntityId,
        amount: f32,
        crit: bool,
        heal: bool,
    },
    Died {
        entity: EntityId,
        killer: Option<EntityId>,
    },
    Correction {
        pos: [f32; 3],
    },
    /// A skillshot left its caster. Clients fly it themselves from here.
    /// `seq` matches the caster's Cast request.
    ProjectileSpawn {
        id: u32,
        caster: EntityId,
        seq: u32,
        ability: AbilityId,
        origin: [f32; 2],
        dir: [f32; 2],
        server_time: Ms,
    },
    /// The server's verdict on a skillshot: whom it hit, if anyone.
    ProjectileEnd {
        id: u32,
        hit: Option<EntityId>,
        /// Where it stopped, on the ground plane.
        at: [f32; 2],
    },
    /// A player's ground ability: lands at `pos` after `delay_ms`.
    GroundBlast {
        caster: EntityId,
        seq: u32,
        ability: AbilityId,
        pos: [f32; 2],
        delay_ms: u32,
    },
    /// A mob's telegraphed attack: anything in the circle when it lands is hit.
    Telegraph {
        source: EntityId,
        pos: [f32; 2],
        radius: f32,
        windup_ms: u32,
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
    Mob,
    Pawn,
}

impl EntityKind {
    /// No PvP: only these can be attacked by players and pawns.
    pub fn hostile(self) -> bool {
        matches!(self, EntityKind::Dummy | EntityKind::Mob)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySnap {
    pub id: EntityId,
    pub kind: EntityKind,
    /// Index into the pack's `mobs` or `pawns`, by kind. 0 otherwise.
    pub def: u16,
    /// The player a pawn belongs to.
    pub owner: Option<EntityId>,
    /// What this entity is attacking (or, for players, has selected).
    pub target: Option<EntityId>,
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
