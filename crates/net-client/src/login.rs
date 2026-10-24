//! Blocking client for the login service. Used by the Godot client on its
//! login and character screens, and by bots in `--login` mode.

use base64::Engine;
use combat_core::ContentPack;
use services::{
    ApiError, CharacterInfo, CharacterList, ContentManifest, CreateCharacter, Credentials,
    PlaceResponse, SessionResponse,
};

pub struct LoginClient {
    pub base: String,
    session: Option<String>,
    agent: ureq::Agent,
}

/// Where to connect for a character, from "play".
pub struct Ticket {
    pub addr: std::net::SocketAddr,
    pub token: Vec<u8>,
    pub zone: String,
    pub layer: u16,
}

fn api_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, r) => match r.into_json::<ApiError>() {
            Ok(a) => a.error,
            Err(_) => format!("HTTP {code}"),
        },
        ureq::Error::Transport(t) => format!("cannot reach the login server ({t})"),
    }
}

impl LoginClient {
    /// `base` like `http://127.0.0.1:7000`.
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            session: None,
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(10))
                .build(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn authed(&self, method: &str, path: &str) -> Result<ureq::Request, String> {
        let s = self.session.as_ref().ok_or("not logged in")?;
        Ok(self
            .agent
            .request(method, &self.url(path))
            .set("Authorization", &format!("Bearer {s}")))
    }

    pub fn register(&self, email: &str, password: &str) -> Result<(), String> {
        self.agent
            .post(&self.url("/register"))
            .send_json(Credentials {
                email: email.into(),
                password: password.into(),
            })
            .map_err(api_err)?;
        Ok(())
    }

    pub fn login(&mut self, email: &str, password: &str) -> Result<(), String> {
        let r: SessionResponse = self
            .agent
            .post(&self.url("/login"))
            .send_json(Credentials {
                email: email.into(),
                password: password.into(),
            })
            .map_err(api_err)?
            .into_json()
            .map_err(|e| e.to_string())?;
        self.session = Some(r.session);
        Ok(())
    }

    pub fn logged_in(&self) -> bool {
        self.session.is_some()
    }

    pub fn characters(&self) -> Result<CharacterList, String> {
        self.authed("GET", "/characters")?
            .call()
            .map_err(api_err)?
            .into_json()
            .map_err(|e| e.to_string())
    }

    pub fn create_character(&self, name: &str, class: &str) -> Result<CharacterInfo, String> {
        self.authed("POST", "/characters")?
            .send_json(CreateCharacter {
                name: name.into(),
                class: class.into(),
            })
            .map_err(api_err)?
            .into_json()
            .map_err(|e| e.to_string())
    }

    pub fn play(&self, character: i64) -> Result<Ticket, String> {
        let r: PlaceResponse = self
            .authed("POST", &format!("/characters/{character}/play"))?
            .call()
            .map_err(api_err)?
            .into_json()
            .map_err(|e| e.to_string())?;
        Ok(Ticket {
            addr: r.addr.parse().map_err(|e| format!("server address: {e}"))?,
            token: base64::engine::general_purpose::STANDARD
                .decode(r.token)
                .map_err(|e| e.to_string())?,
            zone: r.zone,
            layer: r.layer,
        })
    }

    pub fn manifest(&self) -> Result<ContentManifest, String> {
        self.agent
            .get(&self.url("/content/manifest"))
            .call()
            .map_err(api_err)?
            .into_json()
            .map_err(|e| e.to_string())
    }

    /// Download the server's compiled content pack.
    pub fn fetch_pack(&self) -> Result<(ContentPack, Vec<u8>), String> {
        let mut bytes = Vec::new();
        self.agent
            .get(&self.url("/content/pack"))
            .call()
            .map_err(api_err)?
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let pack = ContentPack::from_bytes(&bytes)?;
        Ok((pack, bytes))
    }

    /// The tenant's asset pack, cached in `cache_dir` by hash. Returns the
    /// local path, or None if the server has no asset pack.
    pub fn sync_assets(&self, cache_dir: &str) -> Result<Option<String>, String> {
        let m = self.manifest()?;
        if m.assets_hash.is_empty() {
            return Ok(None);
        }
        let path = std::path::Path::new(cache_dir).join(format!("{}-{}.zip", m.tenant, m.assets_hash));
        if path.exists() {
            return Ok(Some(path.to_string_lossy().into_owned()));
        }
        let mut bytes = Vec::new();
        self.agent
            .get(&self.url("/content/assets"))
            .call()
            .map_err(api_err)?
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        Ok(Some(path.to_string_lossy().into_owned()))
    }

    /// Use `local` if it matches the server's pack, otherwise download it.
    /// Returns the pack and whether it was downloaded.
    pub fn sync_pack(&self, local: Option<ContentPack>) -> Result<(ContentPack, bool), String> {
        let m = self.manifest()?;
        if let Some(p) = local
            && format!("{:016x}", p.hash) == m.hash
        {
            return Ok((p, false));
        }
        let (p, _) = self.fetch_pack()?;
        Ok((p, true))
    }
}

use std::io::Read;
