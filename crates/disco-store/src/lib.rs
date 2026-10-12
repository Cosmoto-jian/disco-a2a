//! SQLite storage: WAL, forward-only migrations via `PRAGMA user_version`,
//! FTS5 full-text and sqlite-vec vector search, fused with reciprocal rank fusion.
//! Secrets never live here: credential columns hold Keychain references only.

use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

mod migrations;

pub const EMBED_DIM: usize = 384;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("embedding must have {EMBED_DIM} dims, got {0}")]
    BadDim(usize),
    #[error("database schema v{found} is newer than this build (v{supported})")]
    TooNew { found: i64, supported: i64 },
}

pub type Result<T> = std::result::Result<T, StoreError>;

static VEC_INIT: Once = Once::new();

#[allow(unsafe_code)]
fn register_sqlite_vec() {
    VEC_INIT.call_once(|| {
        // SAFETY: sqlite3_vec_init has the standard SQLite extension entry-point
        // signature; sqlite-vec is statically linked into the same bundled SQLite.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(sqlite_vec::sqlite3_vec_init as *const ())));
        }
    });
}

/// Memory layer, see design v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Room,
    Agent,
    Team,
    User,
}

impl Layer {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Room => "room",
            Self::Agent => "agent",
            Self::Team => "team",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewMemory<'a> {
    pub layer: Layer,
    /// Isolation boundary: context id, agent id, guild id or user id.
    pub scope_id: &'a str,
    pub author: &'a str,
    pub content: &'a str,
    /// 0 = untrusted external, 1 = unreviewed agent, 2 = reviewed, 3 = human.
    pub trust: u8,
    pub embedding: Option<&'a [f32]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: i64,
    pub content: String,
    pub trust: u8,
    pub score: f64,
}

/// One registered agent. `credential_ref` names a Keychain item, never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    /// `a2a`, `acp` or `openai`.
    pub transport: String,
    pub endpoint: String,
    pub credential_ref: Option<String>,
    pub card_json: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        register_sqlite_vec();
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        register_sqlite_vec();
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut s = Self { conn };
        migrations::run(&mut s.conn)?;
        Ok(s)
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn vec_version(&self) -> Result<String> {
        Ok(self.conn.query_row("select vec_version()", [], |r| r.get(0))?)
    }

    pub fn insert_memory(&mut self, m: &NewMemory<'_>) -> Result<i64> {
        if let Some(e) = m.embedding
            && e.len() != EMBED_DIM
        {
            return Err(StoreError::BadDim(e.len()));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO memories(layer, scope_id, author, content, trust) VALUES (?1,?2,?3,?4,?5)",
            params![m.layer.as_str(), m.scope_id, m.author, m.content, m.trust],
        )?;
        let id = tx.last_insert_rowid();
        if let Some(e) = m.embedding {
            tx.execute(
                "INSERT INTO memory_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, f32_bytes(e)],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Hybrid search inside one isolation scope. Quarantined rows never return.
    pub fn search(
        &self,
        layer: Layer,
        scope_id: &str,
        query: &str,
        query_vec: Option<&[f32]>,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        const RRF_K: f64 = 60.0;
        let pool = (limit * 4).max(20) as i64;
        let mut scores: std::collections::HashMap<i64, f64> = Default::default();

        let fts_q = fts_quote(query);
        if !fts_q.is_empty() {
            let mut stmt = self.conn.prepare_cached(
                "SELECT m.id FROM memory_fts f JOIN memories m ON m.id = f.rowid
                 WHERE memory_fts MATCH ?1 AND m.layer = ?2 AND m.scope_id = ?3 AND m.status = 'active'
                 ORDER BY bm25(memory_fts) LIMIT ?4",
            )?;
            let ids = stmt.query_map(params![fts_q, layer.as_str(), scope_id, pool], |r| r.get::<_, i64>(0))?;
            for (rank, id) in ids.enumerate() {
                *scores.entry(id?).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }

        if let Some(v) = query_vec {
            if v.len() != EMBED_DIM {
                return Err(StoreError::BadDim(v.len()));
            }
            // KNN over a wider pool, then filter to the scope.
            let mut stmt = self.conn.prepare_cached(
                "SELECT m.id FROM (SELECT rowid, distance FROM memory_vec WHERE embedding MATCH ?1 AND k = ?2) v
                 JOIN memories m ON m.id = v.rowid
                 WHERE m.layer = ?3 AND m.scope_id = ?4 AND m.status = 'active'
                 ORDER BY v.distance",
            )?;
            let ids = stmt.query_map(params![f32_bytes(v), pool * 8, layer.as_str(), scope_id], |r| r.get::<_, i64>(0))?;
            for (rank, id) in ids.take(pool as usize).enumerate() {
                *scores.entry(id?).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);

        let mut stmt = self.conn.prepare_cached("SELECT content, trust FROM memories WHERE id = ?1")?;
        let mut out = Vec::with_capacity(ranked.len());
        for (id, score) in ranked {
            if let Some((content, trust)) = stmt
                .query_row([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u8>(1)?)))
                .optional()?
            {
                out.push(Hit { id, content, trust, score });
            }
        }
        Ok(out)
    }

    /// Insert or update an agent by unique name.
    pub fn upsert_agent(&mut self, a: &AgentRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agents (id, name, transport, endpoint, credential_ref, card_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET transport = excluded.transport,
               endpoint = excluded.endpoint, credential_ref = excluded.credential_ref,
               card_json = excluded.card_json",
            params![a.id, a.name, a.transport, a.endpoint, a.credential_ref, a.card_json],
        )?;
        Ok(())
    }

    pub fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, name, transport, endpoint, credential_ref, card_json FROM agents
             WHERE enabled = 1 ORDER BY name",
        )?;
        let rows = st.query_map([], |r| {
            Ok(AgentRow {
                id: r.get(0)?, name: r.get(1)?, transport: r.get(2)?,
                endpoint: r.get(3)?, credential_ref: r.get(4)?, card_json: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_agent(&mut self, name: &str) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM agents WHERE name = ?1", [name])? > 0)
    }

    pub fn quarantine(&mut self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE memories SET status = 'quarantined' WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Turn free text into a safe FTS5 OR-query of quoted terms.
fn fts_quote(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests;
