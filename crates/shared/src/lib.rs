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
    pub table: Table,
    pub id: Uuid,
    pub data: serde_json::Value,
    pub updated_at: String,
}

/// Client -> server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    Push {
        ops: Vec<Op>,
    },
    /// Ask for all events after this sequence number.
    Pull {
        since: i64,
    },
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

/// Tables that participate in sync. Exhaustive on purpose: the compiler
/// forces every match site (apply, invalidate, subscribe) to handle new
/// tables, and methods can live here. The wire name stays stable via
/// `as_str` (that's what sync_log's `table_name` column stores).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Table {
    Notes,
}

impl Table {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notes => "notes",
        }
    }

    /// Inverse of `as_str`; used when replaying sync_log rows. Unknown
    /// names (newer table from a newer client) are skipped by callers.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "notes" => Some(Self::Notes),
            _ => None,
        }
    }
}

/// The schema migrations, applied in order by both sides from this single
/// source: the server via `sqlx::migrate!` (same directory, embedded at
/// compile time, tracked in `_sqlx_migrations`); each client applies them on
/// first boot against its own PGlite (tracked in the `meta` table).
pub static MIGRATIONS: &[(&str, &str)] = &[
    ("0001_notes", include_str!("../migrations/0001_notes.sql")),
    (
        "0002_sync_log",
        include_str!("../migrations/0002_sync_log.sql"),
    ),
    ("0003_meta", include_str!("../migrations/0003_meta.sql")),
];
