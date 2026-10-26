//! Engine-agnostic game client: networking, fake lag, combat prediction and
//! snapshot interpolation. The Godot extension is a thin wrapper over
//! `ClientSession`, and the headless `bot` binary drives the same type.

mod transport;

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Instant, SystemTime};

use combat_core::movement::dist_xz;
use combat_core::projectile::{self, BODY_RADIUS, Flight, xz};
use combat_core::protocol::{self, ClientMsg, EntityKind, PawnCmd, ServerMsg, StopReason};
use combat_core::{
    AbilityId, AuraId, CastCtx, CasterState, ContentPack, Delivery, EntityId, FailReason, Ms,
    PROTOCOL_ID, Started, channel, connection_config,
};
use renet::RenetClient;
use renetcode::ClientAuthentication;

pub use transport::LagTransport;

const MOVE_SEND_MS: Ms = 50;
/// Remote entities are drawn this far in the past, between two snapshots.
/// The server's rewind for aimed hits assumes this same value.
pub const INTERP_DELAY_MS: f64 = projectile::INTERP_DELAY_MS as f64;
/// Ground markers stay up this long after landing, as a flash.
const MARKER_LINGER_MS: Ms = 250;

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Hit {
        source: EntityId,
        target: EntityId,
        ability: AbilityId,
        aura: Option<AuraId>,
        amount: f32,
        crit: bool,
        mine: bool,
    },
    /// A pawn's or mob's basic attack or heal. `mine` if the source is us
    /// or one of our pawns.
    Swing {
        source: EntityId,
        target: EntityId,
        amount: f32,
        crit: bool,
        heal: bool,
        mine: bool,
    },
    Died {
        entity: EntityId,
    },
    Error(String),
    Log(String),
    /// Server confirmed one of our casts; `queue_gap_us` set if it was queued.
    Confirmed {
        seq: u32,
        ability: AbilityId,
        queue_gap_us: Option<u32>,
    },
    /// One of our dashes left: the engine moves the body `distance` metres
    /// along `dir` (ground plane x, z).
    Dash {
        dir: [f32; 2],
        distance: f32,
    },
    /// One of our skillshots has both verdicts: what it hit on our screen and
    /// what it hit on the server. They should almost always agree.
    ShotResolved {
        predicted: Option<EntityId>,
        server: Option<EntityId>,
    },
}

/// A skillshot as this client flies it, on the local clock.
struct LocalShot {
    /// Server id, once its spawn message has arrived.
    id: Option<u32>,
    /// Set for our own shots: the Cast request's seq.
    seq: Option<u32>,
    ability: AbilityId,
    flight: Flight,
    swept_to: Ms,
    /// What it hit (or None for a miss) once it stopped on our screen.
    local: Option<Option<EntityId>>,
    server: Option<Option<EntityId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// Our own ground ability.
    Mine,
    /// Another player's ground ability.
    Ally,
    /// A mob's telegraphed attack: get out.
    Danger,
}

/// A circle on the ground that lands at `lands_at` (local clock).
#[derive(Debug, Clone)]
pub struct Marker {
    pub kind: MarkerKind,
    pub pos: [f32; 2],
    pub radius: f32,
    pub placed_at: Ms,
    pub lands_at: Ms,
    /// Our predicted blasts, to drop the server's echo.
    seq: Option<u32>,
}

/// A skillshot to draw this frame.
#[derive(Debug, Clone)]
pub struct ShotView {
    pub ability: AbilityId,
    pub pos: [f32; 2],
    pub dir: [f32; 2],
    pub mine: bool,
}

struct LocalQueued {
    seq: u32,
    ability: AbilityId,
    target: EntityId,
    /// Aim the player gave at the press (None = soft target).
    aim: Option<[f32; 2]>,
}

/// State to restore if the server rejects a predicted cast.
struct Rollback {
    ability: AbilityId,
    gcd: (Ms, Ms),
    cooldown: (Ms, Ms),
}

pub struct RemoteEntity {
    pub kind: EntityKind,
    /// Index into the pack's `mobs` or `pawns`.
    pub def: u16,
    pub owner: Option<EntityId>,
    pub target: Option<EntityId>,
    buffer: VecDeque<(f64, [f32; 3], f32)>,
    pub hp: f32,
    pub max_hp: f32,
    pub mana: f32,
    pub max_mana: f32,
    /// (ability, elapsed ms, bar ms) at snapshot time.
    pub casting: Option<(AbilityId, u32, u32)>,
    pub auras: Vec<(AuraId, u32)>,
}

pub struct ClientSession {
    pub pack: ContentPack,
    epoch: Instant,
    last_update: Instant,
    transport: LagTransport,
    client: RenetClient,
    pub me: Option<EntityId>,
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
    /// Our selection. Change it with `set_target` so the server (and our
    /// pawns) hear about it.
    pub target: Option<EntityId>,
    pub hp: f32,
    pub max_hp: f32,
    pub entities: HashMap<EntityId, RemoteEntity>,
    server_offset: Option<f64>,
    events: Vec<ClientEvent>,
    correction: Option<[f32; 3]>,
    pub last_queue_gap_us: Option<u32>,
    /// Resolved aim of our predicted cast in progress: (seq, ground point).
    cast_aim: Option<(u32, [f32; 2])>,
    shots: Vec<LocalShot>,
    markers: Vec<Marker>,
    /// Our skillshots: (screen and server agreed, both verdicts known).
    pub shot_agreement: (u32, u32),
}

impl ClientSession {
    pub fn connect(pack: ContentPack, server: SocketAddr) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        let unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let auth = ClientAuthentication::Unsecure {
            protocol_id: PROTOCOL_ID,
            client_id: unix.as_nanos() as u64 ^ std::process::id() as u64,
            server_addr: server,
            user_data: None,
        };
        let transport = LagTransport::new(unix, auth, socket).map_err(|e| e.to_string())?;
        let caster = CasterState::new(pack.abilities.len(), pack.class.max_mana);
        let now = Instant::now();
        Ok(Self {
            pack,
            epoch: now,
            last_update: now,
            transport,
            client: RenetClient::new(connection_config()),
            me: None,
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
            hp: 1.0,
            max_hp: 1.0,
            entities: HashMap::new(),
            server_offset: None,
            events: Vec::new(),
            correction: None,
            last_queue_gap_us: None,
            cast_aim: None,
            shots: Vec::new(),
            markers: Vec::new(),
            shot_agreement: (0, 0),
        })
    }

    pub fn now(&self) -> Ms {
        self.epoch.elapsed().as_millis() as Ms
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

    pub fn take_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn take_correction(&mut self) -> Option<[f32; 3]> {
        self.correction.take()
    }

    pub fn queued_ability(&self) -> Option<AbilityId> {
        self.queued.as_ref().map(|q| q.ability)
    }

    pub fn is_dead(&self) -> bool {
        self.me.is_some() && self.hp <= 0.0
    }

    pub fn set_target(&mut self, target: Option<EntityId>) {
        if target != self.target {
            self.target = target;
            if self.me.is_some() {
                self.send(channel::COMBAT, &ClientMsg::SetTarget { target });
            }
        }
    }

    pub fn pawn_command(&mut self, cmd: PawnCmd) {
        if self.me.is_some() {
            self.send(channel::COMBAT, &ClientMsg::PawnCommand { cmd });
        }
    }

    /// True if `id` is something we may attack right now.
    pub fn attackable(&self, id: EntityId) -> bool {
        self.entities
            .get(&id)
            .is_some_and(|e| e.kind.hostile() && e.hp > 0.0)
    }

    pub fn dummy(&self) -> Option<EntityId> {
        self.entities
            .iter()
            .find(|(_, e)| e.kind == EntityKind::Dummy)
            .map(|(id, _)| *id)
    }

    fn send(&mut self, ch: u8, msg: &ClientMsg) {
        self.client.send_message(ch, protocol::encode(msg));
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
        for ch in [channel::STATE, channel::COMBAT] {
            while let Some(bytes) = self.client.receive_message(ch) {
                if let Some(msg) = protocol::decode::<ServerMsg>(&bytes) {
                    self.on_message(msg);
                }
            }
        }
        let now = self.now();
        self.advance_prediction(now);
        self.advance_shots(now);
        self.markers.retain(|m| now < m.lands_at + MARKER_LINGER_MS);
        if self.me.is_some()
            && (now >= self.last_move_sent + MOVE_SEND_MS || self.moving != self.last_sent_moving)
        {
            self.send_move(now);
        }
        let _ = self.transport.send_packets(&mut self.client);
    }

    fn send_move(&mut self, now: Ms) {
        self.move_seq += 1;
        self.last_move_sent = now;
        self.last_sent_moving = self.moving;
        let msg = ClientMsg::Move {
            seq: self.move_seq,
            pos: self.pos,
            facing: self.facing,
            moving: self.moving,
        };
        self.send(channel::STATE, &msg);
    }

    /// Local position from the engine, every frame. The client owns its
    /// position; the server only corrects it if it is implausible.
    pub fn set_local_move(&mut self, pos: [f32; 3], facing: f32, moving: bool) {
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

    fn ctx(&self, target: Option<EntityId>) -> CastCtx {
        let distance = target
            .filter(|t| self.attackable(*t))
            .and_then(|t| self.entity_pos(t))
            .map(|tp| dist_xz(self.pos, tp));
        CastCtx {
            distance,
            moving: self.moving,
            target_moving: false,
        }
    }

    /// The player pressed the button for `ability`. Responds on this frame:
    /// starts the bar and GCD locally, or queues, or shows an error.
    pub fn press(&mut self, ability: AbilityId) {
        self.press_at(ability, None);
    }

    /// Like `press`, with a ground point (x, z) for aimed abilities: the
    /// cursor on PC, the drag point on mobile. None fires at the target.
    pub fn press_at(&mut self, ability: AbilityId, aim: Option<[f32; 2]>) {
        let Some(def) = self.pack.ability(ability).cloned() else {
            return;
        };
        if self.me.is_none() {
            self.events.push(ClientEvent::Error("Not connected".into()));
            return;
        }
        let now = self.now();
        if self.is_dead() {
            self.events
                .push(ClientEvent::Error(FailReason::Dead.text().into()));
            return;
        }
        let aimed = def.delivery.aimed();
        let target = match self.target {
            Some(t) => t,
            // Aimed abilities need no target; 0 is never an entity.
            None if aimed => 0,
            None => {
                self.events
                    .push(ClientEvent::Error(FailReason::NoTarget.text().into()));
                return;
            }
        };
        if aimed {
            if let Err(reason) = self.local_aim(&def, target, aim) {
                self.events.push(ClientEvent::Error(reason.text().into()));
                return;
            }
        } else if self.entities.contains_key(&target) && !self.attackable(target) {
            self.events
                .push(ClientEvent::Error(FailReason::InvalidTarget.text().into()));
            return;
        }
        if self.caster.ready_at(ability, &def) > now {
            if self.caster.in_queue_window(ability, &def, now) {
                self.seq += 1;
                let seq = self.seq;
                if let Some(old) = self.queued.replace(LocalQueued {
                    seq,
                    ability,
                    target,
                    aim,
                }) {
                    self.locally_failed.push(old.seq);
                }
                // A queued aimed press keeps the player's own aim; the server
                // falls back to the target when it fires.
                let msg = ClientMsg::Cast {
                    seq,
                    ability,
                    target,
                    queued: true,
                    move_seq: self.move_seq,
                    aim,
                };
                self.send(channel::COMBAT, &msg);
            } else {
                let reason = self
                    .caster
                    .check(ability, &def, now, self.ctx_for(&def, Some(target)))
                    .unwrap_err();
                self.events.push(ClientEvent::Error(reason.text().into()));
            }
            return;
        }
        if let Err(reason) = self
            .caster
            .check(ability, &def, now, self.ctx_for(&def, Some(target)))
        {
            self.events.push(ClientEvent::Error(reason.text().into()));
            return;
        }
        self.seq += 1;
        let seq = self.seq;
        // Send the aim as resolved on our screen, so a soft-targeted shot
        // goes where we saw the target, not where the server has it now.
        let resolved = if aimed {
            self.local_aim(&def, target, aim).ok()
        } else {
            None
        };
        self.predict_start(seq, ability, target, now, resolved);
        let msg = ClientMsg::Cast {
            seq,
            ability,
            target,
            queued: false,
            move_seq: self.move_seq,
            aim: resolved,
        };
        self.send(channel::COMBAT, &msg);
    }

    /// The same aim rules as the server's `resolve_aim`, on what we see.
    fn local_aim(
        &self,
        def: &combat_core::AbilityDef,
        target: EntityId,
        aim: Option<[f32; 2]>,
    ) -> Result<[f32; 2], FailReason> {
        let from = xz(self.pos);
        let wanted = aim.or_else(|| self.lead_target(def, target));
        Ok(match def.delivery {
            Delivery::Target => unreachable!("only aimed abilities have an aim"),
            Delivery::Skillshot => wanted.unwrap_or(from),
            Delivery::Ground => {
                projectile::clamp_aim(from, wanted.ok_or(FailReason::NoTarget)?, def.range)
            }
            Delivery::Dash => {
                let dir = projectile::direction(from, wanted.unwrap_or(from), self.facing);
                [from[0] + dir[0] * def.dash_m, from[1] + dir[1] * def.dash_m]
            }
        })
    }

    /// Aim assist for a tap with no aim: where the target will be when the
    /// ability gets there, from its velocity on our screen.
    fn lead_target(&self, def: &combat_core::AbilityDef, target: EntityId) -> Option<[f32; 2]> {
        if !self.attackable(target) {
            return None;
        }
        let p = xz(self.entity_pos(target)?);
        let v = self.velocity(target);
        let windup = def.cast_ms as f32 / 1000.0;
        let mut lead = match def.delivery {
            Delivery::Ground => def.delay_ms as f32 / 1000.0 + windup,
            Delivery::Skillshot => windup,
            _ => return Some(p),
        };
        let at = |lead: f32| [p[0] + v[0] * lead, p[1] + v[1] * lead];
        if def.delivery == Delivery::Skillshot {
            for _ in 0..2 {
                let a = at(lead);
                let d = dist_xz([a[0], 0.0, a[1]], self.pos);
                lead = windup + d / def.speed;
            }
        }
        Some(at(lead))
    }

    /// Ground-plane velocity (m/s) of a remote entity, from its last two
    /// snapshots.
    pub fn velocity(&self, id: EntityId) -> [f32; 2] {
        let Some(e) = self.entities.get(&id) else {
            return [0.0; 2];
        };
        let n = e.buffer.len();
        if n < 2 {
            return [0.0; 2];
        }
        let (a, b) = (&e.buffer[n - 2], &e.buffer[n - 1]);
        let dt = ((b.0 - a.0) / 1000.0) as f32;
        if dt <= 0.0 {
            return [0.0; 2];
        }
        [(b.1[0] - a.1[0]) / dt, (b.1[2] - a.1[2]) / dt]
    }

    fn ctx_for(&self, def: &combat_core::AbilityDef, target: Option<EntityId>) -> CastCtx {
        if def.delivery.aimed() {
            CastCtx {
                distance: Some(0.0),
                moving: self.moving,
                target_moving: false,
            }
        } else {
            self.ctx(target)
        }
    }

    /// Our aimed ability leaves us now: start drawing it at once.
    fn launch_local(&mut self, seq: u32, ability: AbilityId, aim: [f32; 2], t: Ms) {
        let def = &self.pack.abilities[ability as usize];
        let from = xz(self.pos);
        match def.delivery {
            Delivery::Target => {}
            Delivery::Skillshot => {
                let flight =
                    Flight::new(from, aim, self.facing, def.speed, def.range, def.width, t);
                self.shots.push(LocalShot {
                    id: None,
                    seq: Some(seq),
                    ability,
                    flight,
                    swept_to: t,
                    local: None,
                    server: None,
                });
            }
            Delivery::Ground => self.markers.push(Marker {
                kind: MarkerKind::Mine,
                pos: aim,
                radius: def.radius,
                placed_at: t,
                lands_at: t + def.delay_ms as Ms,
                seq: Some(seq),
            }),
            Delivery::Dash => {
                let dir = projectile::direction(from, aim, self.facing);
                let (dx, dz) = (aim[0] - from[0], aim[1] - from[1]);
                self.events.push(ClientEvent::Dash {
                    dir,
                    distance: (dx * dx + dz * dz).sqrt(),
                });
            }
        }
    }

    /// Fly every skillshot on our screen against hostiles as we draw them.
    fn advance_shots(&mut self, now: Ms) {
        let hostiles: Vec<(EntityId, [f32; 2], f32)> = self
            .entities
            .iter()
            .filter(|(_, e)| e.kind.hostile() && e.hp > 0.0)
            .filter_map(|(id, e)| {
                let scale = match e.kind {
                    EntityKind::Mob => self.pack.mobs.get(e.def as usize).map_or(1.0, |m| m.scale),
                    _ => 1.0,
                };
                self.entity_pos(*id)
                    .map(|p| (*id, xz(p), BODY_RADIUS * scale))
            })
            .collect();
        for s in &mut self.shots {
            if s.local.is_some() {
                continue;
            }
            let end = s.flight.ends_at();
            let b = now.min(end);
            let mut best: Option<(f32, EntityId)> = None;
            for &(id, pos, r) in &hostiles {
                if let Some(d) = s.flight.sweep(s.swept_to, b, pos, r)
                    && best.is_none_or(|(bd, _)| d < bd)
                {
                    best = Some((d, id));
                }
            }
            s.swept_to = b;
            if let Some((_, id)) = best {
                s.local = Some(Some(id));
            } else if b >= end {
                s.local = Some(None);
            }
        }
        let mut resolved = Vec::new();
        self.shots.retain(|s| {
            let (Some(local), own) = (s.local, s.seq.is_some()) else {
                return true;
            };
            if !own {
                return false; // someone else's: done once it stops on screen
            }
            match s.server {
                Some(server) => {
                    resolved.push((local, server));
                    false
                }
                // Wait for the server's verdict, but not forever.
                None => now < s.flight.ends_at() + 3000,
            }
        });
        for (predicted, server) in resolved {
            self.shot_agreement.1 += 1;
            if predicted == server {
                self.shot_agreement.0 += 1;
            }
            self.events
                .push(ClientEvent::ShotResolved { predicted, server });
        }
    }

    /// Skillshots to draw: still flying on our screen.
    pub fn shots(&self) -> Vec<ShotView> {
        let now = self.now();
        self.shots
            .iter()
            .filter(|s| s.local.is_none())
            .map(|s| ShotView {
                ability: s.ability,
                pos: s.flight.pos_at(now),
                dir: s.flight.dir,
                mine: s.seq.is_some(),
            })
            .collect()
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    fn predict_start(
        &mut self,
        seq: u32,
        ability: AbilityId,
        target: EntityId,
        t: Ms,
        aim: Option<[f32; 2]>,
    ) {
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
        let started = self.caster.start(ability, def, seq, target, t, 0);
        match (started, aim) {
            (Started::Instant, Some(aim)) => self.launch_local(seq, ability, aim, t),
            (Started::Bar, Some(aim)) => self.cast_aim = Some((seq, aim)),
            _ => {}
        }
    }

    fn advance_prediction(&mut self, now: Ms) {
        loop {
            if let Some(cast) = &self.caster.cast
                && cast.ends_at <= now
            {
                let def = self.pack.abilities[cast.ability as usize].clone();
                let done = self.caster.complete_cast(&def).unwrap();
                if let Some((seq, aim)) = self.cast_aim.take()
                    && seq == done.seq
                {
                    self.launch_local(seq, done.ability, aim, done.ends_at);
                }
                continue;
            }
            if let Some(q) = &self.queued {
                let def = self.pack.abilities[q.ability as usize].clone();
                let ready = self.caster.ready_at(q.ability, &def);
                if ready <= now {
                    let q = self.queued.take().unwrap();
                    let aim = if def.delivery.aimed() {
                        self.local_aim(&def, q.target, q.aim).ok()
                    } else {
                        None
                    };
                    match self.caster.check(
                        q.ability,
                        &def,
                        ready,
                        self.ctx_for(&def, Some(q.target)),
                    ) {
                        Ok(()) => self.predict_start(q.seq, q.ability, q.target, ready, aim),
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

    fn on_message(&mut self, msg: ServerMsg) {
        let now = self.now();
        match msg {
            ServerMsg::Welcome {
                you, content_hash, ..
            } => {
                self.me = Some(you);
                self.content_mismatch = content_hash != self.pack.hash;
                self.events
                    .push(ClientEvent::Log(format!("Connected as entity {you}")));
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
                    // A predicted shot or rune the server refused never happened.
                    self.shots.retain(|s| s.seq != Some(seq));
                    self.markers.retain(|m| m.seq != Some(seq));
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
            } => {
                let mine = Some(source) == self.me;
                self.events.push(ClientEvent::Hit {
                    source,
                    target,
                    ability,
                    aura,
                    amount,
                    crit,
                    mine,
                });
            }
            ServerMsg::Swing {
                source,
                target,
                amount,
                crit,
                heal,
            } => {
                let mine = Some(source) == self.me
                    || self
                        .entities
                        .get(&source)
                        .is_some_and(|e| e.owner.is_some() && e.owner == self.me);
                self.events.push(ClientEvent::Swing {
                    source,
                    target,
                    amount,
                    crit,
                    heal,
                    mine,
                });
            }
            ServerMsg::Died { entity, .. } => {
                if Some(entity) == self.me {
                    self.caster.cancel();
                    self.queued = None;
                }
                self.events.push(ClientEvent::Died { entity });
            }
            ServerMsg::AuraApplied { .. } | ServerMsg::AuraRemoved { .. } => {}
            ServerMsg::ProjectileSpawn {
                id,
                caster,
                seq,
                ability,
                origin,
                dir,
                ..
            } => {
                if Some(caster) == self.me {
                    // Already flying on our screen since the press.
                    if let Some(s) = self.shots.iter_mut().find(|s| s.seq == Some(seq)) {
                        s.id = Some(id);
                    }
                } else if let Some(def) = self.pack.ability(ability) {
                    // Fly it from now: it left the caster about one way ago.
                    let aim = [origin[0] + dir[0], origin[1] + dir[1]];
                    let flight =
                        Flight::new(origin, aim, 0.0, def.speed, def.range, def.width, now);
                    self.shots.push(LocalShot {
                        id: Some(id),
                        seq: None,
                        ability,
                        flight,
                        swept_to: now,
                        local: None,
                        server: None,
                    });
                }
            }
            ServerMsg::ProjectileEnd { id, hit, .. } => {
                if let Some(s) = self.shots.iter_mut().find(|s| s.id == Some(id)) {
                    s.server = Some(hit);
                }
            }
            ServerMsg::GroundBlast {
                caster,
                ability,
                pos,
                delay_ms,
                ..
            } => {
                if Some(caster) != self.me
                    && let Some(def) = self.pack.ability(ability)
                {
                    self.markers.push(Marker {
                        kind: MarkerKind::Ally,
                        pos,
                        radius: def.radius,
                        placed_at: now,
                        lands_at: now + delay_ms as Ms,
                        seq: None,
                    });
                }
            }
            ServerMsg::Telegraph {
                pos,
                radius,
                windup_ms,
                ..
            } => {
                // It lands on the server one way after it reaches us; the
                // server waits for our dodge by about that much, so show the
                // full wind-up from now.
                self.markers.push(Marker {
                    kind: MarkerKind::Danger,
                    pos,
                    radius,
                    placed_at: now,
                    lands_at: now + windup_ms as Ms,
                    seq: None,
                });
            }
            ServerMsg::Snapshot {
                server_time,
                entities,
            } => {
                let sample = server_time as f64 - now as f64;
                self.server_offset = Some(match self.server_offset {
                    // Track the fastest-arriving snapshots; drift down slowly.
                    Some(o) if sample < o => o * 0.98 + sample * 0.02,
                    _ => sample,
                });
                let present: Vec<EntityId> = entities.iter().map(|e| e.id).collect();
                self.entities.retain(|id, _| present.contains(id));
                for s in entities {
                    if Some(s.id) == self.me {
                        self.caster.mana = s.mana;
                        self.caster.max_mana = s.max_mana;
                        self.hp = s.hp;
                        self.max_hp = s.max_hp;
                    }
                    let e = self.entities.entry(s.id).or_insert_with(|| RemoteEntity {
                        kind: s.kind,
                        def: s.def,
                        owner: s.owner,
                        target: None,
                        buffer: VecDeque::new(),
                        hp: 0.0,
                        max_hp: 0.0,
                        mana: 0.0,
                        max_mana: 0.0,
                        casting: None,
                        auras: Vec::new(),
                    });
                    e.buffer.push_back((server_time as f64, s.pos, s.facing));
                    while e.buffer.len() > 16 {
                        e.buffer.pop_front();
                    }
                    e.target = s.target;
                    e.hp = s.hp;
                    e.max_hp = s.max_hp;
                    e.mana = s.mana;
                    e.max_mana = s.max_mana;
                    e.casting = s.casting;
                    e.auras = s.auras;
                }
            }
        }
    }

    /// Interpolated position of another entity, drawn INTERP_DELAY_MS in the past.
    pub fn entity_pos(&self, id: EntityId) -> Option<[f32; 3]> {
        if Some(id) == self.me {
            return Some(self.pos);
        }
        self.interpolated(id).map(|(p, _)| p)
    }

    pub fn interpolated(&self, id: EntityId) -> Option<([f32; 3], f32)> {
        let e = self.entities.get(&id)?;
        let render = self.now() as f64 + self.server_offset? - INTERP_DELAY_MS;
        let (first, last) = (e.buffer.front()?, e.buffer.back()?);
        if render <= first.0 {
            return Some((first.1, first.2));
        }
        if render >= last.0 {
            return Some((last.1, last.2)); // no newer data: hold
        }
        for w in e.buffer.iter().collect::<Vec<_>>().windows(2) {
            let (a, b) = (w[0], w[1]);
            if render >= a.0 && render <= b.0 {
                let k = ((render - a.0) / (b.0 - a.0).max(1.0)) as f32;
                let lerp = |i: usize| a.1[i] + (b.1[i] - a.1[i]) * k;
                return Some(([lerp(0), lerp(1), lerp(2)], b.2));
            }
        }
        Some((last.1, last.2))
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

    pub fn swipe(&self, ability: AbilityId) -> f32 {
        let def = &self.pack.abilities[ability as usize];
        self.caster.swipe(ability, def, self.now())
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
