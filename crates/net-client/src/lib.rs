//! Engine-agnostic game client: networking, fake lag, combat prediction and
//! snapshot interpolation. The Godot extension is a thin wrapper over
//! `ClientSession`, and the headless bots drive the same type.

pub mod brain;
pub mod login;
mod transport;

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Instant, SystemTime};

use combat_core::codec::{self, EntState, MoveInput, dequantize_facing, dequantize_pos, flags};
use combat_core::movement::{ZoneGeometry, dist_xz};
use combat_core::protocol::{
    self, ChatChannel, ClientMsg, ItemStack, PartyMember, ServerMsg, StopReason,
};
use combat_core::{
    AbilityId, AuraId, CastCtx, CasterState, ContentPack, EntityId, EntityKind, FailReason, Ms,
    PROTOCOL_ID, TargetKind, channel, connection_config,
};
use renet::RenetClient;
use renetcode::{ClientAuthentication, ConnectToken};

pub use transport::LagTransport;

const MOVE_SEND_MS: Ms = 50;
/// Remote entities are drawn this far in the past, between two snapshots.
pub const INTERP_DELAY_MS: f64 = 100.0;
/// Past the newest sample, keep moving along the last velocity this long,
/// then freeze.
pub const EXTRAPOLATE_MS: f64 = 150.0;
/// Snapshot states kept as possible delta baselines.
const SNAP_HISTORY: usize = 32;
/// Ability id the server uses for mob melee swings.
pub const MELEE: AbilityId = u16::MAX;

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Hit {
        source: EntityId,
        target: EntityId,
        ability: AbilityId,
        aura: Option<AuraId>,
        amount: f32,
        crit: bool,
        heal: bool,
        mine: bool,
        on_me: bool,
    },
    Died {
        entity: EntityId,
        killer: EntityId,
    },
    Error(String),
    Log(String),
    /// Server confirmed one of our casts; `queue_gap_us` set if it was queued.
    Confirmed {
        seq: u32,
        ability: AbilityId,
        queue_gap_us: Option<u32>,
    },
    Chat {
        channel: ChatChannel,
        from: String,
        text: String,
    },
    Xp {
        gained: u32,
        leveled: bool,
    },
    Looted {
        items: Vec<(u16, u32)>,
    },
    InventoryChanged,
    PartyChanged,
    PartyInvite {
        from: String,
    },
    /// We moved to another zone server (portal or layer merge). The world
    /// state was cleared; `Welcome` from the new server follows.
    ZoneChange {
        zone: u16,
        layer: u16,
    },
}

/// How to authenticate with the zone server.
pub enum Auth {
    /// Development: no login service, just a display name.
    Dev { name: String },
    /// A signed connect token from the login or world service.
    Token(Vec<u8>),
}

struct LocalQueued {
    seq: u32,
    ability: AbilityId,
    target: EntityId,
}

/// State to restore if the server rejects a predicted cast.
struct Rollback {
    ability: AbilityId,
    gcd: (Ms, Ms),
    cooldown: (Ms, Ms),
}

pub struct RemoteEntity {
    pub state: EntState,
    pub name: String,
    /// (server time, position, facing) samples for interpolation.
    buffer: VecDeque<(f64, [f32; 3], f32)>,
}

impl RemoteEntity {
    pub fn kind(&self) -> EntityKind {
        self.state.kind
    }
    pub fn dead(&self) -> bool {
        self.state.flags & flags::DEAD != 0
    }
    pub fn has_flag(&self, f: u8) -> bool {
        self.state.flags & f != 0
    }
}

pub struct ClientSession {
    pub pack: ContentPack,
    epoch: Instant,
    last_update: Instant,
    transport: LagTransport,
    client: RenetClient,
    pub me: Option<EntityId>,
    pub my_name: String,
    pub class: u16,
    pub zone: u16,
    pub layer: u16,
    pub geo: ZoneGeometry,
    /// Ability ids on the action bar, in key order.
    pub bar: Vec<AbilityId>,
    pub content_mismatch: bool,
    pub caster: CasterState,
    queued: Option<LocalQueued>,
    pending: HashMap<u32, Rollback>,
    /// Casts we already rejected locally; ignore the server's echo.
    locally_failed: Vec<u32>,
    seq: u32,
    move_seq: u32,
    pub pos: [f32; 3],
    pub facing: f32,
    pub moving: bool,
    last_move_sent: Ms,
    last_sent_moving: bool,
    pub target: Option<EntityId>,
    pub entities: HashMap<EntityId, RemoteEntity>,
    snapshots: VecDeque<(u32, Vec<(EntityId, EntState)>)>,
    last_snap_id: u32,
    last_snap_time: f64,
    server_offset: Option<f64>,
    events: Vec<ClientEvent>,
    correction: Option<[f32; 3]>,
    pub last_queue_gap_us: Option<u32>,
    /// Snapshots that arrived against a baseline we no longer had.
    pub snapshot_misses: u32,
    /// Something was queued for sending since the last flush.
    dirty: bool,
    pub level: u32,
    pub xp: u32,
    pub xp_next: u32,
    pub bag: Vec<ItemStack>,
    pub equip: Vec<ItemStack>,
    pub party: Vec<PartyMember>,
    handoff: Option<(String, Vec<u8>, u16, u16)>,
    /// Connections made so far: 1 + zone changes.
    pub connections: u32,
}

/// A renet client and its UDP transport for one zone server.
fn open(server: SocketAddr, auth: Auth) -> Result<(LagTransport, RenetClient), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let auth = match auth {
        Auth::Dev { name } => {
            let mut user_data = [0u8; 256];
            let n = name.len().min(32);
            user_data[..n].copy_from_slice(&name.as_bytes()[..n]);
            ClientAuthentication::Unsecure {
                protocol_id: PROTOCOL_ID,
                client_id: unix.as_nanos() as u64
                    ^ ((std::process::id() as u64) << 20)
                    ^ fastrand_u64(),
                server_addr: server,
                user_data: Some(user_data),
            }
        }
        Auth::Token(bytes) => {
            let token = ConnectToken::read(&mut bytes.as_slice())
                .map_err(|e| format!("bad connect token: {e}"))?;
            ClientAuthentication::Secure {
                connect_token: token,
            }
        }
    };
    let transport = LagTransport::new(unix, auth, socket).map_err(|e| e.to_string())?;
    Ok((transport, RenetClient::new(connection_config())))
}

impl ClientSession {
    pub fn connect(pack: ContentPack, server: SocketAddr, auth: Auth) -> Result<Self, String> {
        let (transport, client) = open(server, auth)?;
        let geo = ZoneGeometry::new(&pack.zones[0]);
        let caster = CasterState::new(pack.abilities.len(), pack.classes[0].max_mana);
        let bar = pack.class_bar(0);
        let now = Instant::now();
        Ok(Self {
            pack,
            epoch: now,
            last_update: now,
            transport,
            client,
            me: None,
            my_name: String::new(),
            class: 0,
            zone: 0,
            layer: 0,
            geo,
            bar,
            content_mismatch: false,
            caster,
            queued: None,
            pending: HashMap::new(),
            locally_failed: Vec::new(),
            seq: 0,
            move_seq: 0,
            pos: [0.0; 3],
            facing: 0.0,
            moving: false,
            last_move_sent: 0,
            last_sent_moving: false,
            target: None,
            entities: HashMap::new(),
            snapshots: VecDeque::with_capacity(SNAP_HISTORY),
            last_snap_id: 0,
            last_snap_time: 0.0,
            server_offset: None,
            events: Vec::new(),
            correction: None,
            last_queue_gap_us: None,
            snapshot_misses: 0,
            dirty: true,
            level: 1,
            xp: 0,
            xp_next: 0,
            bag: Vec::new(),
            equip: Vec::new(),
            party: Vec::new(),
            handoff: None,
            connections: 1,
        })
    }

    /// Leave this zone server and join the one in a handoff, keeping the
    /// fake-lag settings. World state is rebuilt from the new snapshots.
    fn switch_server(&mut self, addr: &str, token: Vec<u8>, zone: u16, layer: u16) {
        let target: SocketAddr = match addr.parse() {
            Ok(a) => a,
            Err(e) => {
                self.events
                    .push(ClientEvent::Error(format!("bad handoff address: {e}")));
                return;
            }
        };
        let (latency, loss) = (self.transport.latency_ms, self.transport.loss);
        self.client.disconnect();
        let _ = self.transport.send_packets(&mut self.client);
        self.transport.disconnect();
        match open(target, Auth::Token(token)) {
            Ok((t, c)) => {
                self.transport = t;
                self.client = c;
            }
            Err(e) => {
                self.events.push(ClientEvent::Error(e));
                return;
            }
        }
        self.transport.latency_ms = latency;
        self.transport.loss = loss;
        self.connections += 1;
        self.me = None;
        self.entities.clear();
        self.snapshots.clear();
        self.last_snap_id = 0;
        self.last_snap_time = 0.0;
        self.server_offset = None;
        self.target = None;
        self.caster.cancel();
        self.queued = None;
        self.pending.clear();
        self.locally_failed.clear();
        self.moving = false;
        self.dirty = true;
        self.events.push(ClientEvent::ZoneChange { zone, layer });
    }

    /// Between leaving one zone server and the next one's Welcome.
    pub fn changing_zone(&self) -> bool {
        self.me.is_none() && self.connections > 1 && self.transport.disconnect_reason().is_none()
    }

    pub fn now(&self) -> Ms {
        self.epoch.elapsed().as_millis() as Ms
    }

    /// Estimated current server time in ms.
    pub fn server_now(&self) -> f64 {
        self.now() as f64 + self.server_offset.unwrap_or(0.0)
    }

    pub fn is_connected(&self) -> bool {
        self.transport.is_connected() && self.me.is_some()
    }

    pub fn disconnect_reason(&self) -> Option<String> {
        self.transport.disconnect_reason()
    }

    pub fn set_fake_lag(&mut self, latency_ms: u32, loss: f32) {
        self.transport.latency_ms = latency_ms;
        self.transport.loss = loss.clamp(0.0, 1.0);
    }

    pub fn rtt_ms(&self) -> f64 {
        self.client.rtt() * 1000.0
    }

    pub fn packet_loss(&self) -> f64 {
        self.client.packet_loss()
    }

    /// (bytes in, bytes out, packets in, packets out) on the UDP socket.
    pub fn wire_counters(&self) -> (u64, u64, u64, u64) {
        let t = &self.transport;
        (t.bytes_in, t.bytes_out, t.packets_in, t.packets_out)
    }

    pub fn take_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn take_correction(&mut self) -> Option<[f32; 3]> {
        self.correction.take()
    }

    pub fn queued_slot(&self) -> Option<usize> {
        let q = self.queued.as_ref()?;
        self.bar.iter().position(|a| *a == q.ability)
    }

    pub fn my_state(&self) -> Option<&EntState> {
        self.me
            .and_then(|m| self.entities.get(&m))
            .map(|e| &e.state)
    }

    pub fn dead(&self) -> bool {
        self.my_state().is_some_and(|s| s.flags & flags::DEAD != 0)
    }

    fn send(&mut self, ch: u8, msg: &ClientMsg) {
        self.client.send_message(ch, protocol::encode(msg));
        self.dirty = true;
    }

    /// Run once per frame.
    pub fn update(&mut self) {
        let now_i = Instant::now();
        let dt = now_i - self.last_update;
        self.last_update = now_i;
        self.client.update(dt);
        if let Err(e) = self.transport.update(dt, &mut self.client)
            && self.me.take().is_some()
        {
            self.events
                .push(ClientEvent::Error(format!("Lost connection ({e})")));
        }
        while let Some(bytes) = self.client.receive_message(channel::STATE) {
            self.on_snapshot(&bytes);
        }
        for ch in [channel::COMBAT, channel::BULK] {
            while let Some(bytes) = self.client.receive_message(ch) {
                if let Some(msg) = protocol::decode::<ServerMsg>(&bytes) {
                    self.on_message(msg);
                }
            }
        }
        if let Some((addr, token, zone, layer)) = self.handoff.take() {
            self.switch_server(&addr, token, zone, layer);
            return;
        }
        let now = self.now();
        self.advance_prediction(now);
        if self.me.is_some()
            && (now >= self.last_move_sent + MOVE_SEND_MS || self.moving != self.last_sent_moving)
        {
            self.send_move(now);
        }
        // renet adds an ack packet to every flush; flushing only when there
        // is something to say keeps it to the 20 Hz move rate.
        if self.dirty || !self.transport.is_connected() {
            self.dirty = false;
            let _ = self.transport.send_packets(&mut self.client);
        }
    }

    fn send_move(&mut self, now: Ms) {
        self.move_seq += 1;
        self.last_move_sent = now;
        self.last_sent_moving = self.moving;
        let m = MoveInput {
            seq: self.move_seq,
            ack: self.last_snap_id,
            pos: self.pos,
            facing: self.facing,
            moving: self.moving,
            target: self.target.unwrap_or(0),
        };
        self.client.send_message(channel::STATE, m.encode());
        self.dirty = true;
    }

    /// Local position from the engine, every frame. The client owns its
    /// position; the server only corrects it if it is implausible.
    pub fn set_local_move(&mut self, pos: [f32; 3], facing: f32, moving: bool) {
        if self.dead() {
            self.moving = false;
            return;
        }
        self.pos = pos;
        self.facing = facing;
        self.moving = moving;
        if moving {
            let breaks = self
                .caster
                .cast
                .as_ref()
                .is_some_and(|c| !self.pack.abilities[c.ability as usize].cast_while_moving);
            if breaks {
                self.caster.cancel();
                self.queued = None;
                self.send(channel::COMBAT, &ClientMsg::CancelCast);
                self.events
                    .push(ClientEvent::Log("Cast cancelled by movement".into()));
            }
        }
    }

    /// Move by `delta` metres this frame, sliding along walls. Returns the
    /// new position. Used by the Godot client and the bots alike.
    pub fn walk(&mut self, delta: [f32; 2]) -> [f32; 3] {
        let next = self.geo.slide(self.pos, delta);
        let moving = next != self.pos;
        let facing = if moving {
            delta[0].atan2(delta[1])
        } else {
            self.facing
        };
        self.set_local_move(next, facing, moving);
        self.pos
    }

    pub fn move_speed(&self) -> f32 {
        self.pack.classes[self.class as usize].move_speed
    }

    pub fn is_hostile(&self, id: EntityId) -> bool {
        self.entities
            .get(&id)
            .is_some_and(|e| e.kind() == EntityKind::Mob && !e.dead())
    }

    fn ctx(&self, def: &combat_core::AbilityDef, target: Option<EntityId>) -> CastCtx {
        let (distance, los) = if def.target == TargetKind::Caster {
            (Some(0.0), true)
        } else {
            match target
                .filter(|t| self.is_hostile(*t))
                .and_then(|t| self.entity_pos(t))
            {
                Some(tp) => (Some(dist_xz(self.pos, tp)), self.geo.los(self.pos, tp)),
                None => (None, true),
            }
        };
        CastCtx {
            distance,
            moving: self.moving,
            target_moving: false,
            los,
            dead: self.dead(),
        }
    }

    /// The player pressed action bar slot `slot`. Responds on this frame:
    /// starts the bar and GCD locally, or queues, or shows an error.
    pub fn press(&mut self, slot: usize) {
        let Some(&ability) = self.bar.get(slot) else {
            return;
        };
        let def = self.pack.abilities[ability as usize].clone();
        if self.me.is_none() {
            self.events.push(ClientEvent::Error("Not connected".into()));
            return;
        }
        let now = self.now();
        let target = if def.target == TargetKind::Caster {
            self.me
        } else {
            self.target
        };
        let Some(target) = target else {
            self.events
                .push(ClientEvent::Error(FailReason::NoTarget.text().into()));
            return;
        };
        if self.caster.ready_at(ability, &def) > now && !self.dead() {
            if self.caster.in_queue_window(ability, &def, now) {
                self.seq += 1;
                let seq = self.seq;
                if let Some(old) = self.queued.replace(LocalQueued {
                    seq,
                    ability,
                    target,
                }) {
                    self.locally_failed.push(old.seq);
                }
                let msg = ClientMsg::Cast {
                    seq,
                    ability,
                    target,
                    queued: true,
                    move_seq: self.move_seq,
                };
                self.send(channel::COMBAT, &msg);
            } else {
                let reason = self
                    .caster
                    .check(ability, &def, now, self.ctx(&def, Some(target)))
                    .unwrap_err();
                self.events.push(ClientEvent::Error(reason.text().into()));
            }
            return;
        }
        if let Err(reason) = self
            .caster
            .check(ability, &def, now, self.ctx(&def, Some(target)))
        {
            self.events.push(ClientEvent::Error(reason.text().into()));
            return;
        }
        self.seq += 1;
        let seq = self.seq;
        self.predict_start(seq, ability, target, now);
        let msg = ClientMsg::Cast {
            seq,
            ability,
            target,
            queued: false,
            move_seq: self.move_seq,
        };
        self.send(channel::COMBAT, &msg);
    }

    pub fn release(&mut self) {
        if self.dead() {
            self.send(channel::COMBAT, &ClientMsg::Release);
        }
    }

    /// Use or equip the item in a bag slot.
    pub fn use_item(&mut self, slot: usize) {
        self.send(channel::COMBAT, &ClientMsg::UseItem { slot: slot as u16 });
    }

    pub fn unequip(&mut self, equip_slot: usize) {
        self.send(
            channel::COMBAT,
            &ClientMsg::Unequip {
                equip_slot: equip_slot as u8,
            },
        );
    }

    pub fn destroy_item(&mut self, slot: usize) {
        self.send(channel::COMBAT, &ClientMsg::Destroy { slot: slot as u16 });
    }

    pub fn chat(&mut self, channel: ChatChannel, text: &str) {
        self.send(
            channel::BULK,
            &ClientMsg::Chat {
                channel,
                text: text.to_string(),
            },
        );
    }

    pub fn party_invite(&mut self, name: &str) {
        self.send(channel::BULK, &ClientMsg::PartyInvite { name: name.into() });
    }

    pub fn party_accept(&mut self) {
        self.send(channel::BULK, &ClientMsg::PartyAccept);
    }

    pub fn party_leave(&mut self) {
        self.send(channel::BULK, &ClientMsg::PartyLeave);
    }

    /// Display name of an item index.
    pub fn item_name(&self, item: u16) -> String {
        self.pack
            .items
            .get(item as usize)
            .map_or("?".into(), |i| i.name.clone())
    }

    fn predict_start(&mut self, seq: u32, ability: AbilityId, target: EntityId, t: Ms) {
        let def = &self.pack.abilities[ability as usize];
        self.pending.insert(
            seq,
            Rollback {
                ability,
                gcd: (self.caster.gcd_start, self.caster.gcd_until),
                cooldown: self.caster.cooldowns[ability as usize],
            },
        );
        // Instants finish immediately; their damage arrives from the server.
        self.caster.start(ability, def, seq, target, t, 0);
    }

    fn advance_prediction(&mut self, now: Ms) {
        loop {
            if let Some(cast) = &self.caster.cast
                && cast.ends_at <= now
            {
                let def = self.pack.abilities[cast.ability as usize].clone();
                self.caster.complete_cast(&def);
                continue;
            }
            if let Some(q) = &self.queued {
                let def = self.pack.abilities[q.ability as usize].clone();
                let ready = self.caster.ready_at(q.ability, &def);
                if ready <= now {
                    let q = self.queued.take().unwrap();
                    match self
                        .caster
                        .check(q.ability, &def, ready, self.ctx(&def, Some(q.target)))
                    {
                        Ok(()) => self.predict_start(q.seq, q.ability, q.target, ready),
                        Err(reason) => {
                            self.locally_failed.push(q.seq);
                            self.events.push(ClientEvent::Error(reason.text().into()));
                        }
                    }
                    continue;
                }
            }
            break;
        }
    }

    fn on_snapshot(&mut self, bytes: &[u8]) {
        let Some(base_id) = codec::snapshot_baseline(bytes) else {
            return;
        };
        let baseline: &[(EntityId, EntState)] = if base_id == 0 {
            &[]
        } else {
            match self.snapshots.iter().find(|(id, _)| *id == base_id) {
                Some((_, s)) => s,
                None => {
                    self.snapshot_misses += 1;
                    return;
                }
            }
        };
        let Some(d) = codec::decode_snapshot(bytes, baseline) else {
            return;
        };
        if d.snap_id <= self.last_snap_id {
            return; // late, out of order: the newer one already applied
        }
        let now = self.now() as f64;
        let st = d.server_time as f64;
        let sample = st - now;
        self.server_offset = Some(match self.server_offset {
            // Track the fastest-arriving snapshots; drift down slowly.
            Some(o) if sample < o => o * 0.98 + sample * 0.02,
            _ => sample,
        });
        let prev_snap_time = self.last_snap_time;
        self.last_snap_id = d.snap_id;
        self.last_snap_time = st;

        let names: HashMap<EntityId, String> = d.entered.into_iter().collect();
        let mut present = HashMap::with_capacity(d.entities.len());
        for (id, s) in &d.entities {
            present.insert(*id, ());
            let e = self.entities.entry(*id).or_insert_with(|| RemoteEntity {
                state: *s,
                name: String::new(),
                buffer: VecDeque::new(),
            });
            if let Some(n) = names.get(id) {
                e.name = n.clone();
                e.buffer.clear();
            }
            e.state = *s;
            let pos = dequantize_pos(s.pos);
            let facing = dequantize_facing(s.facing);
            if e.buffer.is_empty() {
                e.buffer.push_back((st, pos, facing));
            } else if d.moved.binary_search(id).is_ok() {
                // It was standing still until now: anchor the start of the
                // motion one snapshot back so it does not glide from afar.
                if let Some(&(t, p, f)) = e.buffer.back()
                    && t < prev_snap_time - 1.0
                {
                    e.buffer.push_back((prev_snap_time, p, f));
                }
                e.buffer.push_back((st, pos, facing));
            } else if let Some(last) = e.buffer.back_mut() {
                last.2 = facing;
            }
            while e.buffer.len() > 12 {
                e.buffer.pop_front();
            }
        }
        self.entities.retain(|id, _| present.contains_key(id));
        if let Some(me) = self.me
            && let Some(s) = self.entities.get(&me).map(|e| e.state)
        {
            self.caster.mana = s.mana as f32;
            self.caster.max_mana = s.max_mana as f32;
            if s.flags & flags::DEAD != 0 && self.caster.cast.is_some() {
                self.caster.cancel();
                self.queued = None;
            }
        }
        if self.target.is_some_and(|t| !self.entities.contains_key(&t)) {
            self.target = None;
        }
        if self.snapshots.len() >= SNAP_HISTORY {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back((d.snap_id, d.entities));
    }

    fn on_message(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome {
                you,
                content_hash,
                server_time,
                zone,
                layer,
                class,
                name,
            } => {
                self.me = Some(you);
                self.my_name = name;
                self.zone = zone;
                self.layer = layer;
                self.class = class;
                self.geo = ZoneGeometry::new(&self.pack.zones[zone as usize]);
                self.bar = self.pack.class_bar(class);
                let stats = self.pack.classes[class as usize].stats_at(1);
                self.caster = CasterState::new(self.pack.abilities.len(), stats.max_mana);
                self.server_offset = Some(server_time as f64 - self.now() as f64);
                self.content_mismatch = content_hash != self.pack.hash;
                self.events.push(ClientEvent::Log(format!(
                    "Entered {} (layer {layer})",
                    self.pack.zones[zone as usize].name
                )));
                if self.content_mismatch {
                    self.events.push(ClientEvent::Error(
                        "Content pack differs from the server's".into(),
                    ));
                }
            }
            ServerMsg::Correction { pos } => {
                self.pos = pos;
                self.correction = Some(pos);
            }
            ServerMsg::CastStart {
                caster,
                seq,
                ability,
                queue_gap_us,
                ..
            } => {
                if Some(caster) == self.me {
                    self.pending.remove(&seq);
                    if queue_gap_us.is_some() {
                        self.last_queue_gap_us = queue_gap_us;
                    }
                    self.events.push(ClientEvent::Confirmed {
                        seq,
                        ability,
                        queue_gap_us,
                    });
                }
            }
            ServerMsg::CastFailed { seq, reason } => {
                if let Some(rb) = self.pending.remove(&seq) {
                    self.caster.gcd_start = rb.gcd.0;
                    self.caster.gcd_until = rb.gcd.1;
                    self.caster.cooldowns[rb.ability as usize] = rb.cooldown;
                    if self.caster.cast.as_ref().is_some_and(|c| c.seq == seq) {
                        self.caster.cancel();
                    }
                }
                if self.queued.as_ref().is_some_and(|q| q.seq == seq) {
                    self.queued = None;
                }
                if let Some(i) = self.locally_failed.iter().position(|s| *s == seq) {
                    self.locally_failed.swap_remove(i);
                } else {
                    self.events.push(ClientEvent::Error(reason.text().into()));
                }
            }
            ServerMsg::CastStop {
                caster,
                seq,
                reason,
            } => {
                if Some(caster) == self.me
                    && reason != StopReason::Completed
                    && self.caster.cast.as_ref().is_some_and(|c| c.seq == seq)
                {
                    self.caster.cancel();
                    self.events.push(ClientEvent::Error("Interrupted".into()));
                }
            }
            ServerMsg::Hit {
                source,
                target,
                ability,
                aura,
                amount,
                crit,
                heal,
            } => {
                self.events.push(ClientEvent::Hit {
                    source,
                    target,
                    ability,
                    aura,
                    amount,
                    crit,
                    heal,
                    mine: Some(source) == self.me,
                    on_me: Some(target) == self.me,
                });
            }
            ServerMsg::Xp {
                level,
                xp,
                next,
                gained,
            } => {
                let leveled = level > self.level && gained > 0;
                self.level = level;
                self.xp = xp;
                self.xp_next = next;
                if gained > 0 {
                    self.events.push(ClientEvent::Xp { gained, leveled });
                }
            }
            ServerMsg::Inventory { bag, equip } => {
                self.bag = bag;
                self.equip = equip;
                self.events.push(ClientEvent::InventoryChanged);
            }
            ServerMsg::Looted { items } => self.events.push(ClientEvent::Looted { items }),
            ServerMsg::Chat {
                channel,
                from,
                text,
            } => self.events.push(ClientEvent::Chat {
                channel,
                from,
                text,
            }),
            ServerMsg::PartyInvite { from } => self.events.push(ClientEvent::PartyInvite { from }),
            ServerMsg::Party { members } => {
                self.party = members;
                self.events.push(ClientEvent::PartyChanged);
            }
            ServerMsg::Handoff {
                addr,
                token,
                zone,
                layer,
            } => {
                self.handoff = Some((addr, token, zone, layer));
            }
            ServerMsg::Died { entity, killer } => {
                if Some(entity) == self.me {
                    self.caster.cancel();
                    self.queued = None;
                }
                if Some(entity) == self.target && Some(entity) != self.me {
                    self.target = None;
                }
                self.events.push(ClientEvent::Died { entity, killer });
            }
        }
    }

    /// Interpolated position of an entity; our own is the predicted one.
    pub fn entity_pos(&self, id: EntityId) -> Option<[f32; 3]> {
        if Some(id) == self.me {
            return Some(self.pos);
        }
        self.interpolated(id).map(|(p, _)| p)
    }

    /// Position and facing of a remote entity, drawn INTERP_DELAY_MS in the
    /// past between the two samples around that time. Past the newest
    /// sample it extrapolates briefly, then holds.
    pub fn interpolated(&self, id: EntityId) -> Option<([f32; 3], f32)> {
        let e = self.entities.get(&id)?;
        let render = self.server_now() - INTERP_DELAY_MS;
        let b = &e.buffer;
        let (first, last) = (b.front()?, b.back()?);
        if render <= first.0 || b.len() == 1 {
            return Some((first.1, first.2));
        }
        if render >= last.0 {
            let prev = b[b.len() - 2];
            let span = (last.0 - prev.0).max(1.0);
            // Only extrapolate an entity that was actually moving.
            if !e.has_flag(flags::MOVING) || span > 300.0 {
                return Some((last.1, last.2));
            }
            let k = ((render - last.0).min(EXTRAPOLATE_MS) / span) as f32;
            let ext = |i: usize| last.1[i] + (last.1[i] - prev.1[i]) * k;
            let p = self.geo.clamp([ext(0), ext(1), ext(2)]);
            return Some((p, last.2));
        }
        let i = b.partition_point(|s| s.0 <= render).clamp(1, b.len() - 1);
        let (a, c) = (b[i - 1], b[i]);
        let k = ((render - a.0) / (c.0 - a.0).max(1.0)) as f32;
        let lerp = |j: usize| a.1[j] + (c.1[j] - a.1[j]) * k;
        Some(([lerp(0), lerp(1), lerp(2)], lerp_angle(a.2, c.2, k)))
    }

    /// Nearest living hostile within `max_range`, skipping the current
    /// target so repeated presses cycle (Tab). `passive` includes training
    /// dummies.
    pub fn next_target(&self, max_range: f32, passive: bool) -> Option<EntityId> {
        let mut best: Option<(EntityId, f32)> = None;
        let mut fallback: Option<(EntityId, f32)> = None;
        for (id, e) in &self.entities {
            if e.kind() != EntityKind::Mob
                || e.dead()
                || e.has_flag(flags::EVADING)
                || (!passive && e.has_flag(flags::PASSIVE))
            {
                continue;
            }
            let Some(p) = self.entity_pos(*id) else {
                continue;
            };
            let d = dist_xz(self.pos, p);
            if d > max_range {
                continue;
            }
            if fallback.is_none_or(|f| d < f.1) {
                fallback = Some((*id, d));
            }
            if Some(*id) != self.target && best.is_none_or(|b| d < b.1) {
                best = Some((*id, d));
            }
        }
        best.or(fallback).map(|b| b.0)
    }

    /// (ability, progress 0..1, remaining ms, is channel) of our predicted bar.
    pub fn my_cast_bar(&self) -> Option<(AbilityId, f32, Ms, bool)> {
        let c = self.caster.cast.as_ref()?;
        let now = self.now();
        let elapsed = now.saturating_sub(c.started_at) as f32;
        let progress = (elapsed / c.bar_ms as f32).clamp(0.0, 1.0);
        Some((
            c.ability,
            progress,
            (c.started_at + c.bar_ms).saturating_sub(now),
            c.channel,
        ))
    }

    /// (ability, progress 0..1) of a remote entity's cast, from snapshots.
    pub fn remote_cast(&self, id: EntityId) -> Option<(AbilityId, f32)> {
        let e = self.entities.get(&id)?;
        let (ab, started) = e.state.cast?;
        let def = self.pack.ability(ab)?;
        let elapsed = (self.server_now() - started as f64).max(0.0) as f32;
        Some((ab, (elapsed / def.bar_ms().max(1) as f32).clamp(0.0, 1.0)))
    }

    pub fn swipe(&self, slot: usize) -> f32 {
        let Some(&ability) = self.bar.get(slot) else {
            return 0.0;
        };
        let def = &self.pack.abilities[ability as usize];
        self.caster.swipe(ability, def, self.now())
    }

    /// Display name of an entity.
    pub fn entity_name(&self, id: EntityId) -> String {
        match self.entities.get(&id) {
            Some(e) if e.kind() == EntityKind::Mob => self
                .pack
                .mobs
                .get(e.state.def as usize)
                .map_or("?".into(), |m| m.name.clone()),
            Some(e) => e.name.clone(),
            None => String::new(),
        }
    }

    /// Name of what dealt a hit: ability, aura, or melee.
    pub fn hit_name(&self, ability: AbilityId, aura: Option<AuraId>) -> String {
        if let Some(a) = aura.and_then(|a| self.pack.aura(a)) {
            return a.name.clone();
        }
        if ability == MELEE {
            return "Melee".into();
        }
        if ability == combat_core::ITEM_ABILITY {
            return "Item".into();
        }
        self.pack
            .ability(ability)
            .map_or(String::new(), |a| a.name.clone())
    }

    pub fn disconnect(&mut self) {
        self.client.disconnect();
        self.transport.disconnect();
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.transport.disconnect();
    }
}

fn lerp_angle(a: f32, b: f32, k: f32) -> f32 {
    let mut d = (b - a) % std::f32::consts::TAU;
    if d > std::f32::consts::PI {
        d -= std::f32::consts::TAU;
    } else if d < -std::f32::consts::PI {
        d += std::f32::consts::TAU;
    }
    a + d * k
}

fn fastrand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(0);
    h.finish()
}
