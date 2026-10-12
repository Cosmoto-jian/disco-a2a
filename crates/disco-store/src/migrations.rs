//! Forward-only migrations. Index = target `user_version` - 1. Never edit a shipped step.
use rusqlite::Connection;

use crate::{Result, StoreError};

const STEPS: &[&str] = &[
    // v1: agents, guilds, tasks, audit
    r"
    CREATE TABLE agents (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        transport TEXT NOT NULL CHECK (transport IN ('a2a','acp','openai')),
        endpoint TEXT NOT NULL,
        credential_ref TEXT,             -- Keychain reference, never the secret
        card_json TEXT,
        card_hash TEXT,
        enabled INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL DEFAULT (unixepoch())
    );
    CREATE TABLE guilds (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        budget_cents INTEGER
    );
    CREATE TABLE guild_agents (
        guild_id TEXT NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
        agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        enabled INTEGER NOT NULL DEFAULT 1,
        allowed_channels TEXT NOT NULL DEFAULT '[]',
        PRIMARY KEY (guild_id, agent_id)
    );
    CREATE TABLE tasks (
        id TEXT PRIMARY KEY,
        context_id TEXT NOT NULL,
        agent_id TEXT NOT NULL REFERENCES agents(id),
        parent_task TEXT,
        phase TEXT NOT NULL,
        hops INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL DEFAULT (unixepoch()),
        updated_at INTEGER NOT NULL DEFAULT (unixepoch())
    );
    CREATE INDEX tasks_open ON tasks(phase) WHERE phase NOT IN ('completed','canceled','failed','rejected');
    CREATE TABLE audit_log (
        id INTEGER PRIMARY KEY,
        ts INTEGER NOT NULL DEFAULT (unixepoch()),
        guild_id TEXT, actor TEXT NOT NULL, agent_id TEXT,
        action TEXT NOT NULL, cost_cents INTEGER, result TEXT
    );
    ",
    // v2: memory with FTS5 + sqlite-vec
    r"
    CREATE TABLE memories (
        id INTEGER PRIMARY KEY,
        layer TEXT NOT NULL CHECK (layer IN ('room','agent','team','user')),
        scope_id TEXT NOT NULL,
        author TEXT NOT NULL,
        content TEXT NOT NULL,
        trust INTEGER NOT NULL CHECK (trust BETWEEN 0 AND 3),
        status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','quarantined','archived')),
        version INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL DEFAULT (unixepoch())
    );
    CREATE INDEX memories_scope ON memories(layer, scope_id, status);
    CREATE VIRTUAL TABLE memory_fts USING fts5(content, content='memories', content_rowid='id', tokenize='unicode61');
    CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
        INSERT INTO memory_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
        INSERT INTO memory_fts(memory_fts, rowid, content) VALUES ('delete', old.id, old.content);
    END;
    CREATE TRIGGER memories_au AFTER UPDATE OF content ON memories BEGIN
        INSERT INTO memory_fts(memory_fts, rowid, content) VALUES ('delete', old.id, old.content);
        INSERT INTO memory_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE VIRTUAL TABLE memory_vec USING vec0(embedding float[384]);
    ",
];

pub const LATEST: i64 = STEPS.len() as i64;

pub fn run(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > LATEST {
        return Err(StoreError::TooNew { found: current, supported: LATEST });
    }
    for (i, sql) in STEPS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
