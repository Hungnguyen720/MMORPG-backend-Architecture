//! Shared combat simulation. Linked by the zone server and, through the
//! GDExtension, by the Godot client, so client prediction and server
//! resolution run the exact same rules.

pub mod caster;
pub mod codec;
pub mod content;
pub mod movement;
pub mod protocol;

/// Milliseconds on whichever clock the caller owns (server clock on the
/// server, local clock on the client). Rules never mix the two.
pub type Ms = u64;
pub type EntityId = u32;
pub type AbilityId = u16;
pub type AuraId = u16;

/// A key pressed this close to the end of a cast or GCD is queued and fired
/// by the server the instant the caster becomes ready.
pub const QUEUE_WINDOW_MS: Ms = 400;
/// Upper bound on how much earlier the server finishes a cast to make up for
/// the one-way trip of the request.
pub const MAX_LATENCY_ALLOWANCE_MS: Ms = 100;
/// Extra range granted when either side is moving.
pub const RANGE_LEEWAY: f32 = 2.0;

/// Ability id in `Hit` messages for mob melee swings.
pub const MELEE_ABILITY: AbilityId = u16::MAX;
/// Ability id in `Hit` messages for item effects (potions).
pub const ITEM_ABILITY: AbilityId = u16::MAX - 1;

pub const PROTOCOL_ID: u64 = 0x4d4d_4f45_4e47_0002; // "MMOENG" v2
pub const DEFAULT_PORT: u16 = 5000;

/// Channel ids, matching the architecture doc.
pub mod channel {
    /// Unreliable: movement in, snapshots out.
    pub const STATE: u8 = 0;
    /// Reliable ordered: everything combat.
    pub const COMBAT: u8 = 1;
    /// Reliable unordered: chat, inventory, party, zone changes.
    pub const BULK: u8 = 2;
}

pub use caster::{ActiveCast, CastCtx, CasterState, FailReason, Started};
pub use codec::{EntState, EntityKind};
pub use content::{AbilityDef, AuraDef, ContentPack, Effect, TargetKind};

/// renet channel layout shared by both ends. Combat resends fast: a lost
/// cast request should cost one short resend, not a 300 ms default timeout.
pub fn connection_config() -> renet::ConnectionConfig {
    use renet::{ChannelConfig, SendType};
    use std::time::Duration;
    let channels = vec![
        ChannelConfig {
            channel_id: channel::STATE,
            max_memory_usage_bytes: 1 << 20,
            send_type: SendType::Unreliable,
        },
        ChannelConfig {
            channel_id: channel::COMBAT,
            max_memory_usage_bytes: 1 << 20,
            send_type: SendType::ReliableOrdered {
                resend_time: Duration::from_millis(60),
            },
        },
        ChannelConfig {
            channel_id: channel::BULK,
            max_memory_usage_bytes: 1 << 20,
            send_type: SendType::ReliableUnordered {
                resend_time: Duration::from_millis(200),
            },
        },
    ];
    renet::ConnectionConfig {
        available_bytes_per_tick: 60_000,
        server_channels_config: channels.clone(),
        client_channels_config: channels,
    }
}
