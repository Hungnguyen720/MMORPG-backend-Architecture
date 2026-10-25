//! The services thread: everything a zone process does that is slow or
//! talks to other machines. One per process, shared by all its zones, on a
//! small tokio runtime so database and Redis latency never touch a tick.
//!
//! - Postgres: load a character on connect, write progress behind, save
//!   inventory at once. Writes for one character go through one queue, so
//!   they land in order (a handoff's save always precedes its placement).
//! - Redis: layer and process heartbeats for the world service, presence,
//!   parties, and the tenant message bus (world chat, whispers, invites).
//! - HTTP: the world service's `/place` for handoffs, and this process's
//!   control endpoint (`/spawn`, `/drain`) that the world service calls to
//!   open and merge layers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use combat_core::ContentPack;
use crossbeam_channel::Sender;
use futures_util::StreamExt;
use persistence::{CharRecord, Db, ItemRow};
use renet::ClientId;
use services::{
    BusMsg, CharEvent, DrainRequest, Env, MAX_PARTY, Party, PlaceRequest, PlaceResponse,
    SpawnLayerRequest, SpawnLayerResponse, TokenUser, keys,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::net::NetIn;

#[derive(Debug, Clone)]
pub enum SocialOp {
    Invite(String),
    Accept,
    Leave,
    PartyChat(String),
    Whisper(String, String),
}

/// What a zone asks of the services thread.
pub enum SvcReq {
    Load {
        client: ClientId,
        user: TokenUser,
    },
    SaveChar {
        record: CharRecord,
    },
    SaveInv {
        character: i64,
        items: Vec<ItemRow>,
        economy: Vec<(String, String, String, i32)>,
    },
    /// Final save and presence cleanup.
    Logout {
        record: CharRecord,
    },
    /// Handoff: ask the world service where `zone` should be entered.
    Place {
        client: ClientId,
        character: i64,
        account: i64,
        zone: String,
        avoid_layer: Option<u16>,
    },
    Social {
        character: i64,
        name: String,
        op: SocialOp,
    },
    WorldChat {
        from: String,
        text: String,
    },
    Heartbeat {
        players: Vec<(i64, String)>,
        count: usize,
        draining: bool,
    },
    Closed,
}

/// Answers delivered back into a zone's input channel.
pub enum SvcEvent {
    Loaded {
        client: ClientId,
        result: Result<(CharRecord, Option<Party>), String>,
    },
    Placed {
        client: ClientId,
        result: Result<PlaceResponse, String>,
    },
    Bus(BusMsg),
    /// Merge this layer away.
    Drain,
}

/// One running zone layer, as the rest of the process sees it.
pub struct ZoneHandle {
    pub zone: String,
    pub layer: u16,
    pub addr: String,
    pub to_sim: Sender<NetIn>,
    /// Started from the command line; never merged away.
    pub permanent: bool,
}

impl ZoneHandle {
    fn key(&self) -> String {
        format!("{}:{}", self.zone, self.layer)
    }
}

pub type Registry = Arc<Mutex<HashMap<(String, u16), Arc<ZoneHandle>>>>;

pub struct Envelope {
    pub zone: Arc<ZoneHandle>,
    pub req: SvcReq,
}

/// Ask the main thread to start a new layer; answers with its address.
pub struct SpawnCmd {
    pub zone: String,
    pub layer: u16,
    pub reply: tokio::sync::oneshot::Sender<Result<String, String>>,
}

#[derive(Clone)]
struct Ctx {
    tenant: String,
    db: Db,
    redis: redis::aio::MultiplexedConnection,
    http: reqwest::Client,
    env: Env,
    registry: Registry,
    proc_name: String,
}

const SHARDS: usize = 8;

/// Start the services thread. Blocks until Postgres and Redis answer, so a
/// misconfigured process fails at startup rather than on first login.
pub fn start(
    env: Env,
    pack: Arc<ContentPack>,
    registry: Registry,
    proc_name: String,
    control_port: u16,
    public_ip: String,
    spawn_tx: std::sync::mpsc::Sender<SpawnCmd>,
) -> UnboundedSender<Envelope> {
    let (tx, rx) = unbounded_channel::<Envelope>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("services".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let ctx = match connect(&env, &pack, registry, proc_name).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                run(ctx, rx, control_port, public_ip, spawn_tx).await;
            });
        })
        .unwrap();
    match ready_rx.recv() {
        Ok(Ok(())) => tx,
        Ok(Err(e)) => panic!("services: {e}"),
        Err(_) => panic!("services thread died during startup"),
    }
}

async fn connect(
    env: &Env,
    pack: &ContentPack,
    registry: Registry,
    proc_name: String,
) -> Result<Ctx, String> {
    let db = Db::connect(&env.db, 16).map_err(|e| format!("postgres: {e}"))?;
    db.migrate()
        .await
        .map_err(|e| format!("postgres migrate: {e}"))?;
    let client = redis::Client::open(env.redis.as_str()).map_err(|e| format!("redis: {e}"))?;
    let redis = client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| format!("redis: {e}"))?;
    Ok(Ctx {
        tenant: pack.tenant.id.clone(),
        db,
        redis,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        env: env.clone(),
        registry,
        proc_name,
    })
}

async fn run(
    ctx: Ctx,
    mut rx: UnboundedReceiver<Envelope>,
    control_port: u16,
    public_ip: String,
    spawn_tx: std::sync::mpsc::Sender<SpawnCmd>,
) {
    // Ordered per-character queues.
    let mut shards = Vec::new();
    for _ in 0..SHARDS {
        let (stx, srx) = unbounded_channel::<Envelope>();
        tokio::spawn(shard(ctx.clone(), srx));
        shards.push(stx);
    }
    tokio::spawn(bus_listener(ctx.clone()));
    tokio::spawn(proc_heartbeat(
        ctx.clone(),
        format!("http://{public_ip}:{control_port}"),
    ));
    tokio::spawn(control_server(ctx.clone(), control_port, spawn_tx));

    while let Some(env) = rx.recv().await {
        let character = match &env.req {
            SvcReq::SaveChar { record } | SvcReq::Logout { record } => Some(record.id),
            SvcReq::SaveInv { character, .. } | SvcReq::Place { character, .. } => Some(*character),
            _ => None,
        };
        match character {
            Some(c) => {
                let _ = shards[c.rem_euclid(SHARDS as i64) as usize].send(env);
            }
            None => {
                tokio::spawn(handle(ctx.clone(), env));
            }
        }
    }
}

async fn shard(ctx: Ctx, mut rx: UnboundedReceiver<Envelope>) {
    while let Some(env) = rx.recv().await {
        handle(ctx.clone(), env).await;
    }
}

fn deliver(zone: &ZoneHandle, ev: SvcEvent) {
    let _ = zone.to_sim.send(NetIn::Svc(ev));
}

async fn handle(mut ctx: Ctx, env: Envelope) {
    let t = ctx.tenant.clone();
    let zone = env.zone;
    match env.req {
        SvcReq::Load { client, user } => {
            let result = load(&mut ctx, user, &zone).await;
            if let Err(e) = &result {
                eprintln!("load character {}: {e}", user.character);
            }
            deliver(&zone, SvcEvent::Loaded { client, result });
        }
        SvcReq::SaveChar { record } => {
            retry("save character", || ctx.db.save_character(&record)).await;
        }
        SvcReq::SaveInv {
            character,
            items,
            economy,
        } => {
            retry("save inventory", || {
                ctx.db.save_inventory(character, &items, &economy)
            })
            .await;
        }
        SvcReq::Logout { record } => {
            retry("save character", || ctx.db.save_character(&record)).await;
            // Only clear presence if it is still ours: after a handoff the
            // new zone has already claimed it.
            let script = redis::Script::new(
                r#"if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('DEL', KEYS[1]) end return 0"#,
            );
            let _: Result<i32, _> = script
                .key(keys::presence(&t, record.id))
                .arg(zone.key())
                .invoke_async(&mut ctx.redis)
                .await;
            let _: Result<i32, _> = redis::cmd("DEL")
                .arg(keys::char_by_name(&t, &record.name))
                .query_async(&mut ctx.redis)
                .await;
        }
        SvcReq::Place {
            client,
            character,
            account,
            zone: to,
            avoid_layer,
        } => {
            let req = PlaceRequest {
                character,
                account,
                zone: to,
                avoid_layer,
            };
            let result = place(&ctx, &req).await;
            deliver(&zone, SvcEvent::Placed { client, result });
        }
        SvcReq::Social {
            character,
            name,
            op,
        } => {
            if let Err(e) = social(&mut ctx, character, &name, op).await {
                eprintln!("social: {e}");
            }
        }
        SvcReq::WorldChat { from, text } => {
            publish(&mut ctx, &BusMsg::WorldChat { from, text }).await;
        }
        SvcReq::Heartbeat {
            players,
            count,
            draining,
        } => {
            let lk = keys::layer(&t, &zone.zone, zone.layer);
            let mut p = redis::pipe();
            p.cmd("HSET")
                .arg(&lk)
                .arg("addr")
                .arg(&zone.addr)
                .arg("players")
                .arg(count)
                .arg("proc")
                .arg(&ctx.proc_name)
                .arg("permanent")
                .arg(zone.permanent as u8)
                .arg("draining")
                .arg(draining as u8)
                .ignore()
                .cmd("EXPIRE")
                .arg(&lk)
                .arg(15)
                .ignore()
                .cmd("SADD")
                .arg(keys::layers(&t))
                .arg(zone.key())
                .ignore();
            for (c, name) in &players {
                p.cmd("SET")
                    .arg(keys::presence(&t, *c))
                    .arg(zone.key())
                    .arg("EX")
                    .arg(30)
                    .ignore()
                    .cmd("SET")
                    .arg(keys::char_by_name(&t, name))
                    .arg(*c)
                    .arg("EX")
                    .arg(30)
                    .ignore();
            }
            if let Err(e) = p.query_async::<()>(&mut ctx.redis).await {
                eprintln!("heartbeat: {e}");
            }
        }
        SvcReq::Closed => {
            let mut p = redis::pipe();
            p.cmd("DEL")
                .arg(keys::layer(&t, &zone.zone, zone.layer))
                .ignore()
                .cmd("SREM")
                .arg(keys::layers(&t))
                .arg(zone.key())
                .ignore()
                .cmd("DEL")
                .arg(keys::draining(&t, &zone.zone, zone.layer))
                .ignore();
            let _ = p.query_async::<()>(&mut ctx.redis).await;
        }
    }
}

/// Retry a database write a few times; a write that keeps failing is
/// logged loudly (the in-memory state is still authoritative and the next
/// save carries it).
async fn retry<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), persistence::Error>>,
{
    for attempt in 0..4 {
        match f().await {
            Ok(()) => return,
            Err(e) => {
                eprintln!("{what} failed (attempt {}): {e}", attempt + 1);
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
        }
    }
}

async fn load(
    ctx: &mut Ctx,
    user: TokenUser,
    zone: &ZoneHandle,
) -> Result<(CharRecord, Option<Party>), String> {
    let t = ctx.tenant.clone();
    let rec = ctx
        .db
        .load_character(user.character)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no such character")?;
    if rec.account_id != user.account {
        return Err("character belongs to another account".into());
    }
    let party = get_party(ctx, user.character).await;
    let mut p = redis::pipe();
    p.cmd("SET")
        .arg(keys::presence(&t, rec.id))
        .arg(zone.key())
        .arg("EX")
        .arg(30)
        .ignore()
        .cmd("SET")
        .arg(keys::char_by_name(&t, &rec.name))
        .arg(rec.id)
        .arg("EX")
        .arg(30)
        .ignore();
    let _ = p.query_async::<()>(&mut ctx.redis).await;
    Ok((rec, party))
}

async fn place(ctx: &Ctx, req: &PlaceRequest) -> Result<PlaceResponse, String> {
    let resp = ctx
        .http
        .post(format!("{}/place", ctx.env.world_url))
        .header("x-internal-key", &ctx.env.internal_key)
        .json(req)
        .send()
        .await
        .map_err(|e| format!("world service: {e}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("world service {s}: {body}"));
    }
    resp.json().await.map_err(|e| e.to_string())
}

async fn publish(ctx: &mut Ctx, msg: &BusMsg) {
    let body = serde_json::to_string(msg).unwrap();
    let r: Result<i64, _> = redis::cmd("PUBLISH")
        .arg(keys::bus(&ctx.tenant))
        .arg(body)
        .query_async(&mut ctx.redis)
        .await;
    if let Err(e) = r {
        eprintln!("publish: {e}");
    }
}

async fn to_char(ctx: &mut Ctx, character: i64, payload: CharEvent) {
    publish(ctx, &BusMsg::ToChar { character, payload }).await;
}

async fn get_str(ctx: &mut Ctx, key: String) -> Option<String> {
    redis::cmd("GET")
        .arg(key)
        .query_async::<Option<String>>(&mut ctx.redis)
        .await
        .ok()
        .flatten()
}

async fn get_party(ctx: &mut Ctx, character: i64) -> Option<Party> {
    let t = ctx.tenant.clone();
    let pid = get_str(ctx, keys::char_party(&t, character)).await?;
    let json = get_str(ctx, keys::party(&t, &pid)).await?;
    serde_json::from_str(&json).ok()
}

async fn put_party(ctx: &mut Ctx, pid: &str, party: &Party) {
    let t = ctx.tenant.clone();
    let mut p = redis::pipe();
    p.cmd("SET")
        .arg(keys::party(&t, pid))
        .arg(serde_json::to_string(party).unwrap())
        .ignore();
    for (c, _) in &party.members {
        p.cmd("SET").arg(keys::char_party(&t, *c)).arg(pid).ignore();
    }
    let _ = p.query_async::<()>(&mut ctx.redis).await;
}

async fn party_changed(ctx: &mut Ctx, party: &Party) {
    for (c, _) in party.members.clone() {
        to_char(
            ctx,
            c,
            CharEvent::PartyChanged {
                party: Some(party.clone()),
            },
        )
        .await;
    }
}

async fn social(ctx: &mut Ctx, me: i64, my_name: &str, op: SocialOp) -> Result<(), String> {
    let t = ctx.tenant.clone();
    let sys = |text: String| CharEvent::System { text };
    match op {
        SocialOp::Invite(name) => {
            let Some(target) = get_str(ctx, keys::char_by_name(&t, &name))
                .await
                .and_then(|s| s.parse::<i64>().ok())
            else {
                to_char(ctx, me, sys(format!("{name} is not online."))).await;
                return Ok(());
            };
            if target == me {
                return Ok(());
            }
            if get_party(ctx, target).await.is_some() {
                to_char(ctx, me, sys(format!("{name} is already in a party."))).await;
                return Ok(());
            }
            if let Some(p) = get_party(ctx, me).await
                && p.members.len() >= MAX_PARTY
            {
                to_char(ctx, me, sys("Your party is full.".into())).await;
                return Ok(());
            }
            let _: Result<(), _> = redis::cmd("SET")
                .arg(format!("{t}:invite:{target}"))
                .arg(format!("{me}:{my_name}"))
                .arg("EX")
                .arg(60)
                .query_async(&mut ctx.redis)
                .await;
            to_char(
                ctx,
                target,
                CharEvent::PartyInvite {
                    from_name: my_name.to_string(),
                    party: me.to_string(),
                },
            )
            .await;
            to_char(ctx, me, sys(format!("You invited {name} to your party."))).await;
        }
        SocialOp::Accept => {
            let key = format!("{t}:invite:{me}");
            let Some(inv) = get_str(ctx, key.clone()).await else {
                to_char(ctx, me, sys("You have no pending invite.".into())).await;
                return Ok(());
            };
            let _: Result<i32, _> = redis::cmd("DEL")
                .arg(&key)
                .query_async(&mut ctx.redis)
                .await;
            let (leader, leader_name) = inv.split_once(':').ok_or("bad invite")?;
            let leader: i64 = leader.parse().map_err(|_| "bad invite")?;
            let pid = get_str(ctx, keys::char_party(&t, leader))
                .await
                .unwrap_or_else(|| leader.to_string());
            let mut party = match get_str(ctx, keys::party(&t, &pid)).await {
                Some(j) => serde_json::from_str(&j).map_err(|e| e.to_string())?,
                None => Party {
                    leader,
                    members: vec![(leader, leader_name.to_string())],
                },
            };
            if party.members.len() >= MAX_PARTY {
                to_char(ctx, me, sys("That party is full.".into())).await;
                return Ok(());
            }
            if !party.members.iter().any(|m| m.0 == me) {
                party.members.push((me, my_name.to_string()));
            }
            put_party(ctx, &pid, &party).await;
            party_changed(ctx, &party).await;
        }
        SocialOp::Leave => {
            let Some(pid) = get_str(ctx, keys::char_party(&t, me)).await else {
                return Ok(());
            };
            let Some(mut party) = get_party(ctx, me).await else {
                return Ok(());
            };
            party.members.retain(|m| m.0 != me);
            let _: Result<i32, _> = redis::cmd("DEL")
                .arg(keys::char_party(&t, me))
                .query_async(&mut ctx.redis)
                .await;
            to_char(ctx, me, CharEvent::PartyChanged { party: None }).await;
            if party.members.len() <= 1 {
                let mut p = redis::pipe();
                p.cmd("DEL").arg(keys::party(&t, &pid)).ignore();
                for (c, _) in &party.members {
                    p.cmd("DEL").arg(keys::char_party(&t, *c)).ignore();
                }
                let _ = p.query_async::<()>(&mut ctx.redis).await;
                for (c, _) in party.members.clone() {
                    to_char(ctx, c, CharEvent::PartyChanged { party: None }).await;
                    to_char(ctx, c, sys("Your party has disbanded.".into())).await;
                }
            } else {
                if party.leader == me {
                    party.leader = party.members[0].0;
                }
                put_party(ctx, &pid, &party).await;
                party_changed(ctx, &party).await;
            }
        }
        SocialOp::PartyChat(text) => {
            let Some(party) = get_party(ctx, me).await else {
                to_char(ctx, me, sys("You are not in a party.".into())).await;
                return Ok(());
            };
            for (c, _) in party.members {
                to_char(
                    ctx,
                    c,
                    CharEvent::PartyChat {
                        from: my_name.to_string(),
                        text: text.clone(),
                    },
                )
                .await;
            }
        }
        SocialOp::Whisper(name, text) => {
            match get_str(ctx, keys::char_by_name(&t, &name))
                .await
                .and_then(|s| s.parse::<i64>().ok())
            {
                Some(target) => {
                    to_char(
                        ctx,
                        target,
                        CharEvent::Whisper {
                            from: my_name.to_string(),
                            text,
                        },
                    )
                    .await
                }
                None => to_char(ctx, me, sys(format!("{name} is not online."))).await,
            }
        }
    }
    Ok(())
}

/// Fan the tenant bus out to every zone in this process.
async fn bus_listener(ctx: Ctx) {
    loop {
        let client = match redis::Client::open(ctx.env.redis.as_str()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("bus: {e}");
                return;
            }
        };
        match client.get_async_pubsub().await {
            Ok(mut ps) => {
                if ps.subscribe(keys::bus(&ctx.tenant)).await.is_ok() {
                    let mut stream = ps.on_message();
                    while let Some(msg) = stream.next().await {
                        let Ok(body) = msg.get_payload::<String>() else {
                            continue;
                        };
                        let Ok(bus) = serde_json::from_str::<BusMsg>(&body) else {
                            continue;
                        };
                        let zones: Vec<Arc<ZoneHandle>> =
                            ctx.registry.lock().unwrap().values().cloned().collect();
                        for z in zones {
                            deliver(&z, SvcEvent::Bus(bus.clone()));
                        }
                    }
                }
            }
            Err(e) => eprintln!("bus: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn proc_heartbeat(mut ctx: Ctx, control_url: String) {
    let t = ctx.tenant.clone();
    loop {
        let mut p = redis::pipe();
        p.cmd("SET")
            .arg(keys::proc(&t, &ctx.proc_name))
            .arg(&control_url)
            .arg("EX")
            .arg(15)
            .ignore()
            .cmd("SADD")
            .arg(keys::procs(&t))
            .arg(&ctx.proc_name)
            .ignore();
        if let Err(e) = p.query_async::<()>(&mut ctx.redis).await {
            eprintln!("proc heartbeat: {e}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[derive(Clone)]
struct ControlState {
    ctx: Ctx,
    spawn_tx: Arc<Mutex<std::sync::mpsc::Sender<SpawnCmd>>>,
}

fn authorized(ctx: &Ctx, headers: &HeaderMap) -> bool {
    headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == ctx.env.internal_key)
}

async fn control_server(ctx: Ctx, port: u16, spawn_tx: std::sync::mpsc::Sender<SpawnCmd>) {
    let state = ControlState {
        ctx,
        spawn_tx: Arc::new(Mutex::new(spawn_tx)),
    };
    let app = Router::new()
        .route("/spawn", post(spawn_layer))
        .route("/drain", post(drain_layer))
        .with_state(state);
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => panic!("control endpoint: cannot bind TCP {port}: {e}"),
    };
    println!("control endpoint on http://0.0.0.0:{port}");
    let _ = axum::serve(listener, app).await;
}

async fn spawn_layer(
    State(s): State<ControlState>,
    headers: HeaderMap,
    Json(req): Json<SpawnLayerRequest>,
) -> Result<Json<SpawnLayerResponse>, (StatusCode, String)> {
    if !authorized(&s.ctx, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "bad internal key".into()));
    }
    if let Some(z) = s
        .ctx
        .registry
        .lock()
        .unwrap()
        .get(&(req.zone.clone(), req.layer))
    {
        return Ok(Json(SpawnLayerResponse {
            addr: z.addr.clone(),
        }));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    s.spawn_tx
        .lock()
        .unwrap()
        .send(SpawnCmd {
            zone: req.zone,
            layer: req.layer,
            reply: tx,
        })
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "shutting down".into()))?;
    match rx.await {
        Ok(Ok(addr)) => Ok(Json(SpawnLayerResponse { addr })),
        Ok(Err(e)) => Err((StatusCode::CONFLICT, e)),
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "spawn failed".into())),
    }
}

async fn drain_layer(
    State(s): State<ControlState>,
    headers: HeaderMap,
    Json(req): Json<DrainRequest>,
) -> StatusCode {
    if !authorized(&s.ctx, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let z = s
        .ctx
        .registry
        .lock()
        .unwrap()
        .get(&(req.zone.clone(), req.layer))
        .cloned();
    match z {
        Some(z) if !z.permanent => {
            deliver(&z, SvcEvent::Drain);
            StatusCode::OK
        }
        Some(_) => StatusCode::CONFLICT,
        None => StatusCode::NOT_FOUND,
    }
}
