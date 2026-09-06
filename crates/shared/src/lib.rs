use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub updated_at: String, // ISO 8601
}

/// An operation the client pushes, or the server replays from sync_log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub table: String,
    pub id: Uuid,
    pub data: serde_json::Value,
    pub updated_at: String,
}

/// Client -> server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    Push { ops: Vec<Op> },
    /// Ask for all events after this sequence number.
    Pull { since: i64 },
}

/// Server -> client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMsg {
    /// Result of a Push: server-assigned cursor so far.
    Ack { cursor: i64 },
    /// New events the client should apply.
    Events { events: Vec<Op>, cursor: i64 },
}

pub const NOTES_TABLE: &str = "notes";

/// The schema migrations, applied in order by both sides from this single
/// source: the server via `sqlx::migrate!` (same directory, embedded at
/// compile time, tracked in `_sqlx_migrations`); each client applies them on
/// first boot against its own PGlite (tracked in the `meta` table).
pub static MIGRATIONS: &[(&str, &str)] = &[
    ("0001_notes", include_str!("../migrations/0001_notes.sql")),
    ("0002_sync_log", include_str!("../migrations/0002_sync_log.sql")),
    ("0003_meta", include_str!("../migrations/0003_meta.sql")),
];
