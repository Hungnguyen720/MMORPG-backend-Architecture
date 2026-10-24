//! Login service: accounts, sessions, characters, and the content pack.
//! The only service the game client talks to over HTTP. "Play" asks the
//! world service for a layer and hands the client a signed connect token.
//!
//! Sessions live in Redis for an hour. Login attempts are rate limited per
//! client address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use combat_core::ContentPack;
use persistence::{Db, ItemRow};
use rand::RngCore;
use redis::aio::MultiplexedConnection;
use services::{
    ApiError, CharacterInfo, CharacterList, ClassInfo, ContentManifest, CreateCharacter,
    Credentials, Env, PlaceRequest, PlaceResponse, SessionResponse, keys, normalize_name,
};

const SESSION_TTL: u64 = 3600;

const MAX_CHARACTERS: usize = 8;

#[derive(Clone)]
struct App {
    env: Env,
    pack: Arc<ContentPack>,
    pack_bytes: Bytes,
    /// The tenant's asset pack (zip), if one was given.
    assets: Option<(Bytes, u64)>,
    db: Db,
    redis: MultiplexedConnection,
    http: reqwest::Client,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<ApiError>) {
    eprintln!("internal error: {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let env = Env::from_args(&mut args);
    let mut port = 7000u16;
    let mut assets_path = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().unwrap().parse().expect("--port"),
            "--assets" => assets_path = it.next(),
            "-h" | "--help" => {
                println!(
                    "login [--port 7000] --db postgres://... [--content dir|pack] [--assets pack.zip] [--redis url] [--internal-key k] [--world url]"
                );
                return;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let pack = ContentPack::load(&env.content).unwrap_or_else(|e| panic!("{}: {e}", env.content));
    let pack_bytes = Bytes::from(pack.to_bytes());
    let assets = assets_path.map(|p| {
        let b = std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        let h = combat_core::content::fnv1a(&b);
        println!("asset pack {p}: {} bytes, hash {h:016x}", b.len());
        (Bytes::from(b), h)
    });
    let db = Db::connect(&env.db, 8).expect("postgres");
    db.migrate()
        .await
        .unwrap_or_else(|e| panic!("postgres {}: {e}", env.db));
    let redis = services::redis_conn(&env.redis)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    println!(
        "login service for \"{}\" on :{port} (pack {:016x}, {} bytes)",
        pack.tenant.name,
        pack.hash,
        pack_bytes.len()
    );
    let app = App {
        env,
        pack: Arc::new(pack),
        pack_bytes,
        assets,
        db,
        redis,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
    };
    let router = Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/characters", get(list_characters).post(create_character))
        .route("/characters/{id}/play", post(play))
        .route("/content/manifest", get(manifest))
        .route("/content/pack", get(content_pack))
        .route("/content/assets", get(content_assets))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap();
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

/// Login and register attempts per client address per minute
/// (`MMO_LOGIN_RATE`, default 10; load tests raise it).
fn attempts_per_minute() -> i64 {
    std::env::var("MMO_LOGIN_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

async fn rate_limited(app: &App, what: &str, who: &str, per_minute: i64) -> bool {
    let mut con = app.redis.clone();
    let key = keys::rate(&app.pack.tenant.id, what, who);
    let (n,): (i64,) = redis::pipe()
        .cmd("INCR")
        .arg(&key)
        .cmd("EXPIRE")
        .arg(&key)
        .arg(60)
        .arg("NX")
        .ignore()
        .query_async(&mut con)
        .await
        .unwrap_or((0,));
    n > per_minute
}

async fn register(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(c): Json<Credentials>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    if rate_limited(
        &app,
        "register",
        &addr.ip().to_string(),
        attempts_per_minute(),
    )
    .await
    {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts, wait a minute",
        ));
    }
    let email = c.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 200 {
        return Err(err(StatusCode::BAD_REQUEST, "enter a valid email"));
    }
    if c.password.len() < 8 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "password needs at least 8 characters",
        ));
    }
    let password = c.password.clone();
    let hash = tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    match app
        .db
        .create_account(&email, &hash)
        .await
        .map_err(internal)?
    {
        Some(_) => Ok(StatusCode::CREATED),
        None => Err(err(
            StatusCode::CONFLICT,
            "that email already has an account",
        )),
    }
}

async fn login(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(c): Json<Credentials>,
) -> ApiResult<SessionResponse> {
    if rate_limited(&app, "login", &addr.ip().to_string(), attempts_per_minute()).await {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts, wait a minute",
        ));
    }
    let bad = || err(StatusCode::UNAUTHORIZED, "wrong email or password");
    let Some((account, hash)) = app.db.account_by_email(&c.email).await.map_err(internal)? else {
        return Err(bad());
    };
    let password = c.password.clone();
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .map_err(internal)?;
    if !ok {
        return Err(bad());
    }
    let mut raw = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut raw);
    let session: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let mut con = app.redis.clone();
    let _: () = redis::cmd("SET")
        .arg(keys::session(&app.pack.tenant.id, &session))
        .arg(account)
        .arg("EX")
        .arg(SESSION_TTL)
        .query_async(&mut con)
        .await
        .map_err(internal)?;
    Ok(Json(SessionResponse { session }))
}

/// The account behind the request's bearer session, refreshing its TTL.
async fn account(app: &App, headers: &HeaderMap) -> Result<i64, (StatusCode, Json<ApiError>)> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "log in first"))?;
    let key = keys::session(&app.pack.tenant.id, token);
    let mut con = app.redis.clone();
    let (account,): (Option<i64>,) = redis::pipe()
        .cmd("GET")
        .arg(&key)
        .cmd("EXPIRE")
        .arg(&key)
        .arg(SESSION_TTL)
        .ignore()
        .query_async(&mut con)
        .await
        .map_err(internal)?;
    account.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "session expired, log in again"))
}

async fn list_characters(State(app): State<App>, h: HeaderMap) -> ApiResult<CharacterList> {
    let acc = account(&app, &h).await?;
    let chars = app.db.list_characters(acc).await.map_err(internal)?;
    Ok(Json(CharacterList {
        characters: chars
            .into_iter()
            .map(|c| CharacterInfo {
                id: c.id,
                name: c.name,
                class: c.class,
                level: c.level,
                zone: c.zone,
            })
            .collect(),
        classes: app
            .pack
            .classes
            .iter()
            .map(|c| ClassInfo {
                id: c.id.clone(),
                name: c.name.clone(),
            })
            .collect(),
    }))
}

async fn create_character(
    State(app): State<App>,
    h: HeaderMap,
    Json(req): Json<CreateCharacter>,
) -> ApiResult<CharacterInfo> {
    let acc = account(&app, &h).await?;
    let name = normalize_name(&req.name)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "names are 3 to 16 letters"))?;
    let class = app
        .pack
        .classes
        .iter()
        .find(|c| c.id == req.class)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "unknown class"))?;
    if app.db.list_characters(acc).await.map_err(internal)?.len() >= MAX_CHARACTERS {
        return Err(err(StatusCode::BAD_REQUEST, "character limit reached"));
    }
    let t = &app.pack.tenant;
    let zone = app.pack.zone(&t.start_zone).expect("validated");
    let stats = class.stats_at(1);
    let items: Vec<ItemRow> = t
        .starting_items
        .iter()
        .enumerate()
        .map(|(i, s)| ItemRow {
            slot: i as i16,
            item: s.item.clone(),
            count: s.count as i32,
        })
        .collect();
    let id = app
        .db
        .create_character(
            acc,
            &name,
            &class.id,
            &zone.id,
            zone.player_spawn,
            stats.max_hp,
            stats.max_mana,
            &items,
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::CONFLICT, "that name is taken"))?;
    Ok(Json(CharacterInfo {
        id,
        name,
        class: class.id.clone(),
        level: 1,
        zone: zone.id.clone(),
    }))
}

async fn play(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<PlaceResponse> {
    let acc = account(&app, &h).await?;
    let rec = app
        .db
        .load_character(id)
        .await
        .map_err(internal)?
        .filter(|r| r.account_id == acc)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such character"))?;
    let mut con = app.redis.clone();
    let online: Option<String> = redis::cmd("GET")
        .arg(keys::presence(&app.pack.tenant.id, id))
        .query_async(&mut con)
        .await
        .map_err(internal)?;
    if online.is_some() {
        return Err(err(
            StatusCode::CONFLICT,
            "that character is still online; try again in half a minute",
        ));
    }
    let resp = app
        .http
        .post(format!("{}/place", app.env.world_url))
        .header("x-internal-key", &app.env.internal_key)
        .json(&PlaceRequest {
            character: id,
            account: acc,
            zone: rec.zone,
            avoid_layer: None,
        })
        .send()
        .await
        .map_err(|e| {
            err(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("world service: {e}"),
            )
        })?;
    if !resp.status().is_success() {
        let body: ApiError = resp.json().await.unwrap_or(ApiError {
            error: "world service error".into(),
        });
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, body.error));
    }
    Ok(Json(resp.json().await.map_err(internal)?))
}

async fn manifest(State(app): State<App>) -> Json<ContentManifest> {
    Json(ContentManifest {
        tenant: app.pack.tenant.id.clone(),
        name: app.pack.tenant.name.clone(),
        hash: format!("{:016x}", app.pack.hash),
        size: app.pack_bytes.len(),
        assets_hash: app
            .assets
            .as_ref()
            .map_or(String::new(), |a| format!("{:016x}", a.1)),
        assets_size: app.assets.as_ref().map_or(0, |a| a.0.len()),
    })
}

async fn content_assets(State(app): State<App>) -> impl IntoResponse {
    match &app.assets {
        Some((bytes, _)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/zip")],
            bytes.clone(),
        ),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            Bytes::from_static(b"no asset pack"),
        ),
    }
}

async fn content_pack(State(app): State<App>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        app.pack_bytes.clone(),
    )
}
