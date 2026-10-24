//! World service: decides which zone layer a character enters and signs the
//! connect token for it. Zone processes register their layers in Redis;
//! this service fills layers up to the tenant's soft cap, opens a new layer
//! on the least busy zone process when all are full, keeps party members
//! together, and merges thin layers back when the crowd leaves.
//!
//! Internal only: every call needs the `x-internal-key` header.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use combat_core::ContentPack;
use redis::aio::MultiplexedConnection;
use serde::Serialize;
use services::{
    ApiError, DrainRequest, Env, Party, PlaceRequest, PlaceResponse, SpawnLayerRequest,
    SpawnLayerResponse, TokenUser, keys, mint_token,
};

#[derive(Clone)]
struct App {
    env: Env,
    tenant: String,
    soft_cap: u32,
    zones: Vec<String>,
    redis: MultiplexedConnection,
    http: reqwest::Client,
    /// Serialises layer spawns per zone inside this process; the Redis lock
    /// covers several world instances.
    spawn_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone, Serialize)]
struct Layer {
    zone: String,
    layer: u16,
    addr: String,
    players: u32,
    reserved: u32,
    proc: String,
    permanent: bool,
    draining: bool,
}

impl Layer {
    fn load(&self) -> u32 {
        self.players + self.reserved
    }
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let env = Env::from_args(&mut args);
    let mut port = 7001u16;
    let mut cap_override = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().unwrap().parse().expect("--port"),
            "--layer-cap" => cap_override = Some(it.next().unwrap().parse().expect("--layer-cap")),
            "-h" | "--help" => {
                println!(
                    "world [--port 7001] [--content dir|pack] [--redis url] [--netcode-key hex] [--internal-key k] [--layer-cap n]"
                );
                return;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let pack = ContentPack::load(&env.content).unwrap_or_else(|e| panic!("{}: {e}", env.content));
    let redis = services::redis_conn(&env.redis)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let app = App {
        tenant: pack.tenant.id.clone(),
        soft_cap: cap_override.unwrap_or(pack.tenant.layer_soft_cap),
        zones: pack.zones.iter().map(|z| z.id.clone()).collect(),
        env,
        redis,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
    };
    println!(
        "world service for \"{}\" on :{port}, layer soft cap {}",
        pack.tenant.name, app.soft_cap
    );
    tokio::spawn(merge_loop(app.clone()));
    let router = Router::new()
        .route("/place", post(place))
        .route("/status", get(status))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap();
    axum::serve(listener, router).await.unwrap();
}

fn authorized(app: &App, h: &HeaderMap) -> bool {
    h.get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == app.env.internal_key)
}

async fn status(State(app): State<App>, h: HeaderMap) -> ApiResult<Vec<Layer>> {
    if !authorized(&app, &h) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad internal key"));
    }
    let mut con = app.redis.clone();
    let mut all = Vec::new();
    for z in &app.zones {
        all.extend(layers(&app, &mut con, z).await);
    }
    Ok(Json(all))
}

/// Live layers of one zone, pruning any whose heartbeat stopped.
async fn layers(app: &App, con: &mut MultiplexedConnection, zone: &str) -> Vec<Layer> {
    let t = &app.tenant;
    let members: Vec<String> = redis::cmd("SMEMBERS")
        .arg(keys::layers(t))
        .query_async(con)
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in members {
        let Some((z, l)) = m.rsplit_once(':') else {
            continue;
        };
        if z != zone {
            continue;
        }
        let Ok(layer) = l.parse::<u16>() else {
            continue;
        };
        let h: HashMap<String, String> = redis::cmd("HGETALL")
            .arg(keys::layer(t, zone, layer))
            .query_async(con)
            .await
            .unwrap_or_default();
        if h.is_empty() {
            let _: Result<i32, _> = redis::cmd("SREM")
                .arg(keys::layers(t))
                .arg(&m)
                .query_async(con)
                .await;
            continue;
        }
        let reserved: Option<u32> = redis::cmd("GET")
            .arg(format!("{t}:resv:{zone}:{layer}"))
            .query_async(con)
            .await
            .unwrap_or(None);
        let draining_key: bool = redis::cmd("EXISTS")
            .arg(keys::draining(t, zone, layer))
            .query_async(con)
            .await
            .unwrap_or(false);
        let flag = |k: &str| h.get(k).is_some_and(|v| v == "1");
        out.push(Layer {
            zone: zone.to_string(),
            layer,
            addr: h.get("addr").cloned().unwrap_or_default(),
            players: h.get("players").and_then(|p| p.parse().ok()).unwrap_or(0),
            reserved: reserved.unwrap_or(0),
            proc: h.get("proc").cloned().unwrap_or_default(),
            permanent: flag("permanent"),
            draining: flag("draining") || draining_key,
        });
    }
    out.sort_by_key(|l| l.layer);
    out
}

async fn place(
    State(app): State<App>,
    h: HeaderMap,
    Json(req): Json<PlaceRequest>,
) -> ApiResult<PlaceResponse> {
    if !authorized(&app, &h) {
        return Err(err(StatusCode::UNAUTHORIZED, "bad internal key"));
    }
    if !app.zones.contains(&req.zone) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("unknown zone {}", req.zone),
        ));
    }
    let mut con = app.redis.clone();
    let layer = choose_layer(&app, &mut con, &req).await?;
    let t = &app.tenant;
    let rk = format!("{t}:resv:{}:{}", req.zone, layer.layer);
    let _: Result<(), _> = redis::pipe()
        .cmd("INCR")
        .arg(&rk)
        .ignore()
        .cmd("EXPIRE")
        .arg(&rk)
        .arg(20)
        .ignore()
        .query_async(&mut con)
        .await;
    let addr = layer.addr.parse().map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("layer address: {e}"),
        )
    })?;
    let token = mint_token(
        &app.env.netcode_key,
        addr,
        TokenUser {
            character: req.character,
            account: req.account,
        },
    )
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(PlaceResponse {
        zone: req.zone,
        layer: layer.layer,
        addr: layer.addr,
        token: base64::engine::general_purpose::STANDARD.encode(token),
    }))
}

async fn choose_layer(
    app: &App,
    con: &mut MultiplexedConnection,
    req: &PlaceRequest,
) -> Result<Layer, (StatusCode, Json<ApiError>)> {
    let t = &app.tenant;
    let hard_cap = app.soft_cap + app.soft_cap / 5;
    for _attempt in 0..20 {
        let all = layers(app, con, &req.zone).await;
        let open: Vec<&Layer> = all
            .iter()
            .filter(|l| !l.draining && Some(l.layer) != req.avoid_layer)
            .collect();
        // Party members first: join a member's layer while under the hard cap.
        if let Some(party) = party_of(app, con, req.character).await {
            for (member, _) in &party.members {
                if *member == req.character {
                    continue;
                }
                let at: Option<String> = redis::cmd("GET")
                    .arg(keys::presence(t, *member))
                    .query_async(con)
                    .await
                    .unwrap_or(None);
                if let Some(l) = at.and_then(|at| {
                    open.iter()
                        .find(|l| format!("{}:{}", l.zone, l.layer) == at && l.load() < hard_cap)
                }) {
                    return Ok((*l).clone());
                }
            }
        }
        // Fill the busiest layer that still has room, so the world feels
        // populated and spare layers empty out for merging.
        if let Some(l) = open
            .iter()
            .filter(|l| l.load() < app.soft_cap)
            .max_by_key(|l| (l.load(), std::cmp::Reverse(l.layer)))
        {
            return Ok((*l).clone());
        }
        // Everything is full: open a new layer.
        let _guard = app.spawn_lock.lock().await;
        let lock_key = format!("{t}:spawnlock:{}", req.zone);
        let got: Option<String> = redis::cmd("SET")
            .arg(&lock_key)
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(10)
            .query_async(con)
            .await
            .unwrap_or(None);
        if got.is_none() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let next = all.iter().map(|l| l.layer).max().unwrap_or(0) + 1;
        let result = spawn(app, con, &req.zone, next, &all).await;
        let _: Result<i32, _> = redis::cmd("DEL").arg(&lock_key).query_async(con).await;
        match result {
            Ok(l) => return Ok(l),
            Err(e) => {
                eprintln!("spawning {} layer {next}: {e}", req.zone);
                if open.is_empty() {
                    return Err(err(StatusCode::SERVICE_UNAVAILABLE, e));
                }
                // Over the soft cap beats not getting in at all.
                let l = open.iter().min_by_key(|l| l.load()).unwrap();
                return Ok((*l).clone());
            }
        }
    }
    Err(err(StatusCode::SERVICE_UNAVAILABLE, "no layer available"))
}

async fn party_of(app: &App, con: &mut MultiplexedConnection, character: i64) -> Option<Party> {
    let t = &app.tenant;
    let pid: Option<String> = redis::cmd("GET")
        .arg(keys::char_party(t, character))
        .query_async(con)
        .await
        .ok()?;
    let json: Option<String> = redis::cmd("GET")
        .arg(keys::party(t, &pid?))
        .query_async(con)
        .await
        .ok()?;
    serde_json::from_str(&json?).ok()
}

/// Ask the zone process hosting the fewest layers to start a new one.
async fn spawn(
    app: &App,
    con: &mut MultiplexedConnection,
    zone: &str,
    layer: u16,
    existing: &[Layer],
) -> Result<Layer, String> {
    let t = &app.tenant;
    let procs: Vec<String> = redis::cmd("SMEMBERS")
        .arg(keys::procs(t))
        .query_async(con)
        .await
        .map_err(|e| e.to_string())?;
    let mut best: Option<(usize, String, String)> = None;
    for p in procs {
        let url: Option<String> = redis::cmd("GET")
            .arg(keys::proc(t, &p))
            .query_async(con)
            .await
            .unwrap_or(None);
        let Some(url) = url else {
            let _: Result<i32, _> = redis::cmd("SREM")
                .arg(keys::procs(t))
                .arg(&p)
                .query_async(con)
                .await;
            continue;
        };
        let hosted = existing.iter().filter(|l| l.proc == p).count();
        if best.as_ref().is_none_or(|b| hosted < b.0) {
            best = Some((hosted, p, url));
        }
    }
    let (_, proc, url) = best.ok_or("no zone processes are running")?;
    let resp = app
        .http
        .post(format!("{url}/spawn"))
        .header("x-internal-key", &app.env.internal_key)
        .json(&SpawnLayerRequest {
            zone: zone.to_string(),
            layer,
        })
        .send()
        .await
        .map_err(|e| format!("{proc}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{proc}: {}", resp.text().await.unwrap_or_default()));
    }
    let r: SpawnLayerResponse = resp.json().await.map_err(|e| e.to_string())?;
    // Register it now so the next placement sees it before its heartbeat.
    let lk = keys::layer(t, zone, layer);
    let _: Result<(), _> = redis::pipe()
        .cmd("HSET")
        .arg(&lk)
        .arg("addr")
        .arg(&r.addr)
        .arg("players")
        .arg(0)
        .arg("proc")
        .arg(&proc)
        .arg("permanent")
        .arg(0)
        .arg("draining")
        .arg(0)
        .ignore()
        .cmd("EXPIRE")
        .arg(&lk)
        .arg(15)
        .ignore()
        .cmd("SADD")
        .arg(keys::layers(t))
        .arg(format!("{zone}:{layer}"))
        .ignore()
        .query_async(con)
        .await;
    println!("opened {zone} layer {layer} on {proc} at {}", r.addr);
    Ok(Layer {
        zone: zone.to_string(),
        layer,
        addr: r.addr,
        players: 0,
        reserved: 0,
        proc,
        permanent: false,
        draining: false,
    })
}

/// Every 10 s, per zone: if the players on the emptiest temporary layer
/// fit comfortably into the others, drain it. Its zone hands each player
/// off to another layer and the thread exits once empty.
async fn merge_loop(app: App) {
    let mut con = app.redis.clone();
    let t = app.tenant.clone();
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        for zone in &app.zones {
            let all = layers(&app, &mut con, zone).await;
            let open: Vec<&Layer> = all.iter().filter(|l| !l.draining).collect();
            if open.len() < 2 {
                continue;
            }
            let Some(victim) = open
                .iter()
                .filter(|l| !l.permanent)
                .min_by_key(|l| (l.load(), std::cmp::Reverse(l.layer)))
            else {
                continue;
            };
            // Room left in the others at 80 % of the soft cap.
            let target = app.soft_cap * 4 / 5;
            let room: u32 = open
                .iter()
                .filter(|l| l.layer != victim.layer)
                .map(|l| target.saturating_sub(l.load()))
                .sum();
            if victim.load() > room || victim.load() > app.soft_cap / 2 {
                continue;
            }
            let url: Option<String> = redis::cmd("GET")
                .arg(keys::proc(&t, &victim.proc))
                .query_async(&mut con)
                .await
                .unwrap_or(None);
            let Some(url) = url else {
                continue;
            };
            let _: Result<(), _> = redis::cmd("SET")
                .arg(keys::draining(&t, zone, victim.layer))
                .arg(1)
                .arg("EX")
                .arg(300)
                .query_async(&mut con)
                .await;
            let r = app
                .http
                .post(format!("{url}/drain"))
                .header("x-internal-key", &app.env.internal_key)
                .json(&DrainRequest {
                    zone: zone.clone(),
                    layer: victim.layer,
                })
                .send()
                .await;
            match r {
                Ok(r) if r.status().is_success() => println!(
                    "merging {zone} layer {} ({} players) into the other {} layers",
                    victim.layer,
                    victim.players,
                    open.len() - 1
                ),
                Ok(r) => eprintln!("drain {zone} layer {}: {}", victim.layer, r.status()),
                Err(e) => eprintln!("drain {zone} layer {}: {e}", victim.layer),
            }
        }
    }
}
