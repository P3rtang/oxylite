pub mod from_row;
pub mod note;
pub mod sync_row;
pub mod timestamp;

pub use note::Note;
pub use sync_row::SyncRow;
pub use timestamp::{Timestamp, TimestampError};

use enum_iterator::Sequence;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An operation the client pushes, or the server replays from sync_log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub table: Table,
    pub id: Uuid,
    pub data: serde_json::Value,
    pub updated_at: Timestamp,
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
    /// Full table state at `seq`, replacing per-op replay for clients that
    /// are too far behind for replay to be cheap (fresh IndexedDB, or a
    /// long offline stretch). Applied with bulk upserts; rows use the same
    /// payload shape as `Op.data`. Tombstones ride along so deletions
    /// behind the client's cursor are not lost (see `Tombstone`).
    Snapshot {
        seq: i64,
        tables: Vec<TableData>,
        tombstones: Vec<Tombstone>,
    },
}

/// One table's snapshot: every live row, payload-shaped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableData {
    pub table: Table,
    pub rows: Vec<serde_json::Value>,
}

/// A recorded deletion, riding the Snapshot: a far-behind client never
/// replays the delete op (its backlog starts after it), so tombstones
/// must ship with the snapshot or the row would resurrect on the next
/// stale replay. Lib-owned and generic — deleted rows carry no data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub table: Table,
    pub id: Uuid,
    pub deleted_at: Timestamp,
}

/// Tables that participate in sync. Exhaustive on purpose: the compiler
/// forces every match site (apply, invalidate, subscribe) to handle new
/// tables, and methods can live here. The wire name stays stable via
/// `as_str` (that's what sync_log's `table_name` column stores).
/// Iteration over all variants is derived (`Sequence`), so new variants
/// join `enum_iterator::all::<Table>()` automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
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

// The migration list is generated at build time (see build.rs): a scan
// of migrations/, sorted lexicographically (= applied order, the same
// contract sqlx::migrate! follows server-side), each file embedded with
// include_str!. Adding a migration file is the whole job — naming
// violations fail the build.
include!(concat!(env!("OUT_DIR"), "/migrations.rs"));
