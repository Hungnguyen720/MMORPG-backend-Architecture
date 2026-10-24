//! Shared pieces of the backend services: configuration, Redis key layout,
//! connect tokens, and the world service's HTTP API types (also used by the
//! zone servers).

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use renetcode::{ConnectToken, NETCODE_KEY_BYTES, NETCODE_USER_DATA_BYTES};

/// Settings every backend process takes, from flags or environment.
#[derive(Debug, Clone)]
pub struct Env {
    /// Tenant content: a source directory or a compiled `.pack`.
    pub content: String,
    pub db: String,
    pub redis: String,
    /// 32-byte key that signs connect tokens, shared by world and zones.
    pub netcode_key: [u8; NETCODE_KEY_BYTES],
    /// Shared secret for service-to-service HTTP calls.
    pub internal_key: String,
    pub world_url: String,
}

impl Env {
    /// Reads `--content --db --redis --netcode-key --internal-key --world`
    /// from `args` (removing them) with `MMO_*` environment fallbacks.
    pub fn from_args(args: &mut Vec<String>) -> Self {
        let mut take = |flag: &str, env: &str, default: &str| -> String {
            if let Some(i) = args.iter().position(|a| a == flag) {
                let v = args.get(i + 1).cloned().unwrap_or_else(|| panic!("{flag} needs a value"));
                args.drain(i..=i + 1);
                return v;
            }
            std::env::var(env).unwrap_or_else(|_| default.to_string())
        };
        let content = take("--content", "MMO_CONTENT", "tenants/default");
        let db = take("--db", "MMO_DB", "");
        let redis = take("--redis", "MMO_REDIS", "redis://127.0.0.1/");
        let key_hex = take("--netcode-key", "MMO_NETCODE_KEY", "");
        let internal_key = take("--internal-key", "MMO_INTERNAL_KEY", "dev-internal-key");
        let world_url = take("--world", "MMO_WORLD_URL", "http://127.0.0.1:7001");
        let netcode_key = if key_hex.is_empty() {
            eprintln!("warning: no --netcode-key / MMO_NETCODE_KEY; using the insecure dev key");
            *b"dev-only-netcode-key-32-bytes!!!"
        } else {
            parse_key(&key_hex).expect("netcode key must be 64 hex characters")
        };
        Self {
            content,
            db,
            redis,
            netcode_key,
            internal_key,
            world_url,
        }
    }
}

pub fn parse_key(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(k)
}

pub fn unix_now() -> std::time::Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
}

/// Redis key layout. Every key is prefixed with the tenant id, so several
/// games can share one Redis.
pub mod keys {
    pub fn session(t: &str, token: &str) -> String {
        format!("{t}:session:{token}")
    }
    pub fn rate(t: &str, what: &str, who: &str) -> String {
        format!("{t}:rl:{what}:{who}")
    }
    /// Where a character is online: "zone:layer". Refreshed by its zone.
    pub fn presence(t: &str, character: i64) -> String {
        format!("{t}:presence:{character}")
    }
    /// Set of "zone:layer" strings that have registered.
    pub fn layers(t: &str) -> String {
        format!("{t}:layers")
    }
    /// Hash: addr, players, control. Expires unless refreshed.
    pub fn layer(t: &str, zone: &str, layer: u16) -> String {
        format!("{t}:layer:{zone}:{layer}")
    }
    pub fn draining(t: &str, zone: &str, layer: u16) -> String {
        format!("{t}:draining:{zone}:{layer}")
    }
    /// Zone server processes: control URL, refreshed.
    pub fn procs(t: &str) -> String {
        format!("{t}:procs")
    }
    pub fn proc(t: &str, name: &str) -> String {
        format!("{t}:proc:{name}")
    }
    pub fn party(t: &str, id: &str) -> String {
        format!("{t}:party:{id}")
    }
    pub fn char_party(t: &str, character: i64) -> String {
        format!("{t}:charparty:{character}")
    }
    /// Character id by lower-case name, for invites and whispers.
    pub fn char_by_name(t: &str, name: &str) -> String {
        format!("{t}:charname:{}", name.to_lowercase())
    }
    /// Pub/sub channel every zone process listens on.
    pub fn bus(t: &str) -> String {
        format!("{t}:bus")
    }
}

/// What a connect token says about who is connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUser {
    pub character: i64,
    pub account: i64,
}

impl TokenUser {
    pub fn to_bytes(self) -> [u8; NETCODE_USER_DATA_BYTES] {
        let mut d = [0u8; NETCODE_USER_DATA_BYTES];
        d[..8].copy_from_slice(&self.character.to_le_bytes());
        d[8..16].copy_from_slice(&self.account.to_le_bytes());
        d[16] = 1; // version
        d
    }
    pub fn from_bytes(d: &[u8; NETCODE_USER_DATA_BYTES]) -> Option<Self> {
        (d[16] == 1).then(|| Self {
            character: i64::from_le_bytes(d[..8].try_into().unwrap()),
            account: i64::from_le_bytes(d[8..16].try_into().unwrap()),
        })
    }
}

/// Mint a connect token for one zone server, valid for 30 s.
pub fn mint_token(
    key: &[u8; NETCODE_KEY_BYTES],
    server: SocketAddr,
    user: TokenUser,
) -> Result<Vec<u8>, String> {
    let client_id = rand::random::<u64>();
    let token = ConnectToken::generate(
        unix_now(),
        combat_core::PROTOCOL_ID,
        30,
        client_id,
        15,
        vec![server],
        Some(&user.to_bytes()),
        key,
    )
    .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    token.write(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// World service API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceRequest {
    pub character: i64,
    pub account: i64,
    pub zone: String,
    /// Don't put the character back on this layer (draining).
    #[serde(default)]
    pub avoid_layer: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceResponse {
    pub zone: String,
    pub layer: u16,
    pub addr: String,
    /// Base64 netcode connect token.
    pub token: String,
}

/// A zone process asks for a new layer to be started (world → zone).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnLayerRequest {
    pub zone: String,
    pub layer: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnLayerResponse {
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainRequest {
    pub zone: String,
    pub layer: u16,
}

/// Messages on the tenant's Redis pub/sub bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BusMsg {
    /// World chat, delivered to everyone online.
    WorldChat { from: String, text: String },
    /// Delivered to one character wherever it is.
    ToChar { character: i64, payload: CharEvent },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CharEvent {
    PartyInvite { from_name: String, party: String },
    /// The character's party now looks like this (None: no party).
    PartyChanged { party: Option<Party> },
    PartyChat { from: String, text: String },
    Whisper { from: String, text: String },
    System { text: String },
}

/// Party record in Redis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Party {
    pub leader: i64,
    /// (character id, name)
    pub members: Vec<(i64, String)>,
}

pub const MAX_PARTY: usize = 5;

// ---------------------------------------------------------------------------
// Login service API (the game client calls these)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResponse {
    pub session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterInfo {
    pub id: i64,
    pub name: String,
    pub class: String,
    pub level: i32,
    pub zone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterList {
    pub characters: Vec<CharacterInfo>,
    pub classes: Vec<ClassInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCharacter {
    pub name: String,
    pub class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentManifest {
    pub tenant: String,
    pub name: String,
    /// FNV-1a of the compiled pack, as in the zone's Welcome message.
    pub hash: String,
    pub size: usize,
    /// FNV-1a of the asset pack (models, icons, theme), "" if none.
    #[serde(default)]
    pub assets_hash: String,
    #[serde(default)]
    pub assets_size: usize,
}

/// A JSON error body: `{"error": "..."}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

/// Valid character names: 3 to 16 letters, first one capital after
/// normalising ("aRwen" becomes "Arwen").
pub fn normalize_name(name: &str) -> Option<String> {
    let name = name.trim();
    let n = name.chars().count();
    if !(3..=16).contains(&n) || !name.chars().all(|c| c.is_alphabetic()) {
        return None;
    }
    let lower = name.to_lowercase();
    let mut c = lower.chars();
    let first = c.next()?;
    Some(first.to_uppercase().chain(c).collect())
}

#[cfg(feature = "server")]
pub async fn redis_conn(url: &str) -> Result<redis::aio::MultiplexedConnection, String> {
    let client = redis::Client::open(url).map_err(|e| format!("redis: {e}"))?;
    client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| format!("redis {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(normalize_name("aRwen").as_deref(), Some("Arwen"));
        assert_eq!(normalize_name("ab"), None);
        assert_eq!(normalize_name("bad name"), None);
        assert_eq!(normalize_name("x1y2z"), None);
    }

    #[test]
    fn token_user_round_trip() {
        let u = TokenUser {
            character: 42,
            account: 7,
        };
        assert_eq!(TokenUser::from_bytes(&u.to_bytes()), Some(u));
        assert_eq!(TokenUser::from_bytes(&[0u8; 256]), None);
    }
}
