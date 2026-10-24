//! Postgres is the source of truth: accounts, characters, inventory. One
//! database per tenant. Shared by the login service (accounts, character
//! list) and the zone servers (load on enter, write-behind while playing).
//!
//! Items are stored by their content id, not their index, so a recompiled
//! pack with reordered items never scrambles inventories.

use deadpool_postgres::{Config, Pool, Runtime};
use serde::{Deserialize, Serialize};
use tokio_postgres::NoTls;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Bag slots are 0..bag_size; equipment slot `n` is stored as 100 + n.
pub const EQUIP_SLOT_BASE: i16 = 100;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id          BIGSERIAL PRIMARY KEY,
    email       TEXT NOT NULL UNIQUE,
    pass_hash   TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS characters (
    id          BIGSERIAL PRIMARY KEY,
    account_id  BIGINT NOT NULL REFERENCES accounts(id),
    name        TEXT NOT NULL,
    class       TEXT NOT NULL,
    level       INT NOT NULL DEFAULT 1,
    xp          BIGINT NOT NULL DEFAULT 0,
    zone        TEXT NOT NULL,
    pos_x       REAL NOT NULL,
    pos_y       REAL NOT NULL,
    pos_z       REAL NOT NULL,
    hp          REAL NOT NULL,
    mana        REAL NOT NULL,
    -- Tenant-specific fields: packs can add stats without migrations.
    extra       JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS characters_name ON characters (lower(name));
CREATE INDEX IF NOT EXISTS characters_account ON characters (account_id);
CREATE TABLE IF NOT EXISTS items (
    character_id BIGINT NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    slot         SMALLINT NOT NULL,
    item         TEXT NOT NULL,
    count        INT NOT NULL,
    PRIMARY KEY (character_id, slot)
);
-- Audit trail of economy-critical events (rare drops). The idempotency key
-- makes a retried write harmless.
CREATE TABLE IF NOT EXISTS economy_log (
    key          TEXT PRIMARY KEY,
    character_id BIGINT NOT NULL,
    event        TEXT NOT NULL,
    item         TEXT NOT NULL,
    count        INT NOT NULL,
    at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemRow {
    pub slot: i16,
    pub item: String,
    pub count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CharRecord {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub class: String,
    pub level: i32,
    pub xp: i64,
    pub zone: String,
    pub pos: [f32; 3],
    pub hp: f32,
    pub mana: f32,
    pub items: Vec<ItemRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharSummary {
    pub id: i64,
    pub name: String,
    pub class: String,
    pub level: i32,
    pub zone: String,
}

#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

impl Db {
    /// Connect with a URL like `postgres://user:pass@host/dbname`.
    pub fn connect(url: &str, pool_size: usize) -> Result<Self, Error> {
        let mut cfg = Config::new();
        cfg.url = Some(url.to_string());
        cfg.pool = Some(deadpool_postgres::PoolConfig::new(pool_size));
        let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls)?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<(), Error> {
        let c = self.pool.get().await?;
        // Several services start at once; the advisory lock keeps their
        // CREATE IF NOT EXISTS statements from racing each other.
        c.batch_execute("SELECT pg_advisory_lock(7262)").await?;
        let r = c.batch_execute(SCHEMA).await;
        c.batch_execute("SELECT pg_advisory_unlock(7262)").await?;
        r?;
        Ok(())
    }

    pub async fn create_account(&self, email: &str, pass_hash: &str) -> Result<Option<i64>, Error> {
        let c = self.pool.get().await?;
        let row = c
            .query_opt(
                "INSERT INTO accounts (email, pass_hash) VALUES ($1, $2)
                 ON CONFLICT (email) DO NOTHING RETURNING id",
                &[&email.to_lowercase(), &pass_hash],
            )
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// (account id, password hash)
    pub async fn account_by_email(&self, email: &str) -> Result<Option<(i64, String)>, Error> {
        let c = self.pool.get().await?;
        let row = c
            .query_opt(
                "SELECT id, pass_hash FROM accounts WHERE email = $1",
                &[&email.to_lowercase()],
            )
            .await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    pub async fn list_characters(&self, account: i64) -> Result<Vec<CharSummary>, Error> {
        let c = self.pool.get().await?;
        let rows = c
            .query(
                "SELECT id, name, class, level, zone FROM characters
                 WHERE account_id = $1 ORDER BY id",
                &[&account],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| CharSummary {
                id: r.get(0),
                name: r.get(1),
                class: r.get(2),
                level: r.get(3),
                zone: r.get(4),
            })
            .collect())
    }

    /// Returns None if the name is taken.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_character(
        &self,
        account: i64,
        name: &str,
        class: &str,
        zone: &str,
        pos: [f32; 3],
        hp: f32,
        mana: f32,
        starting_items: &[ItemRow],
    ) -> Result<Option<i64>, Error> {
        let mut c = self.pool.get().await?;
        let tx = c.transaction().await?;
        let row = tx
            .query_opt(
                "INSERT INTO characters (account_id, name, class, zone, pos_x, pos_y, pos_z, hp, mana)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING RETURNING id",
                &[&account, &name, &class, &zone, &pos[0], &pos[1], &pos[2], &hp, &mana],
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: i64 = row.get(0);
        for it in starting_items {
            tx.execute(
                "INSERT INTO items (character_id, slot, item, count) VALUES ($1, $2, $3, $4)",
                &[&id, &it.slot, &it.item, &it.count],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(Some(id))
    }

    pub async fn character_owner(&self, id: i64) -> Result<Option<i64>, Error> {
        let c = self.pool.get().await?;
        let row = c
            .query_opt("SELECT account_id FROM characters WHERE id = $1", &[&id])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    pub async fn load_character(&self, id: i64) -> Result<Option<CharRecord>, Error> {
        let c = self.pool.get().await?;
        let Some(r) = c
            .query_opt(
                "SELECT id, account_id, name, class, level, xp, zone, pos_x, pos_y, pos_z, hp, mana
                 FROM characters WHERE id = $1",
                &[&id],
            )
            .await?
        else {
            return Ok(None);
        };
        let items = c
            .query(
                "SELECT slot, item, count FROM items WHERE character_id = $1 ORDER BY slot",
                &[&id],
            )
            .await?
            .iter()
            .map(|r| ItemRow {
                slot: r.get(0),
                item: r.get(1),
                count: r.get(2),
            })
            .collect();
        Ok(Some(CharRecord {
            id: r.get(0),
            account_id: r.get(1),
            name: r.get(2),
            class: r.get(3),
            level: r.get(4),
            xp: r.get(5),
            zone: r.get(6),
            pos: [r.get(7), r.get(8), r.get(9)],
            hp: r.get(10),
            mana: r.get(11),
            items,
        }))
    }

    /// Progress and position. Inventory is saved separately, immediately.
    pub async fn save_character(&self, ch: &CharRecord) -> Result<(), Error> {
        let c = self.pool.get().await?;
        c.execute(
            "UPDATE characters SET level = $2, xp = $3, zone = $4, pos_x = $5, pos_y = $6,
                    pos_z = $7, hp = $8, mana = $9, updated_at = now()
             WHERE id = $1",
            &[
                &ch.id, &ch.level, &ch.xp, &ch.zone, &ch.pos[0], &ch.pos[1], &ch.pos[2], &ch.hp,
                &ch.mana,
            ],
        )
        .await?;
        Ok(())
    }

    /// Replace a character's whole inventory in one transaction, logging any
    /// economy-critical events with it.
    pub async fn save_inventory(
        &self,
        character: i64,
        items: &[ItemRow],
        economy: &[(String, String, String, i32)],
    ) -> Result<(), Error> {
        let mut c = self.pool.get().await?;
        let tx = c.transaction().await?;
        tx.execute("DELETE FROM items WHERE character_id = $1", &[&character])
            .await?;
        for it in items {
            tx.execute(
                "INSERT INTO items (character_id, slot, item, count) VALUES ($1, $2, $3, $4)",
                &[&character, &it.slot, &it.item, &it.count],
            )
            .await?;
        }
        for (key, event, item, count) in economy {
            tx.execute(
                "INSERT INTO economy_log (key, character_id, event, item, count)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (key) DO NOTHING",
                &[key, &character, event, item, count],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
