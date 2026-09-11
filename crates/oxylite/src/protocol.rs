//! The wire protocol's message shapes — the lib's own contract now
//! (#31): the DTOs moved out of the app's `shared` crate and went
//! generic over the app's table enum, so nothing in the machinery
//! names a concrete table anymore. `T` serializes through its own
//! serde impls (the app enum keeps `rename_all = "lowercase"`), so the
//! wire bytes are IDENTICAL to the pre-generic protocol —
//! `spec/sync-protocol.md` describes the same bytes.

use crate::table::SyncTable;
use crate::timestamp::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An operation the client pushes, or the server replays from sync_log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op<T: SyncTable> {
    pub table: T,
    pub id: Uuid,
    pub data: serde_json::Value,
    pub updated_at: Timestamp,
}

/// Client -> server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg<T: SyncTable> {
    Push {
        ops: Vec<Op<T>>,
        /// Client-generated batch identity for durable-delivery: the
        /// client persists its ops (with this id) before sending and
        /// deletes them only when the Ack echoes the id — a socket that
        /// accepts a send and dies before the server reads it loses
        /// nothing, the next connect resends the batch (LWW makes the
        /// retry a no-op). Server-side this is pure echo: the id is
        /// never stored or interpreted.
        batch: Uuid,
    },
    /// Ask for all events after this sequence number.
    Pull { since: i64 },
}

/// Server -> client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMsg<T: SyncTable> {
    /// Result of a Push: server-assigned cursor so far, plus the push's
    /// batch id so the client can retire exactly the ops the server
    /// confirmed (durable delivery, see `ClientMsg::Push`).
    Ack { cursor: i64, batch: Uuid },
    /// New events the client should apply.
    Events { events: Vec<Op<T>>, cursor: i64 },
    /// Full table state at `seq`, replacing per-op replay for clients that
    /// are too far behind for replay to be cheap (fresh IndexedDB, or a
    /// long offline stretch). Applied with bulk upserts; rows use the same
    /// payload shape as `Op.data`. Tombstones ride along so deletions
    /// behind the client's cursor are not lost (see `Tombstone`).
    Snapshot {
        seq: i64,
        tables: Vec<TableData<T>>,
        tombstones: Vec<Tombstone<T>>,
    },
}

/// One table's snapshot: every live row, payload-shaped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableData<T: SyncTable> {
    pub table: T,
    pub rows: Vec<serde_json::Value>,
}

/// A recorded deletion, riding the Snapshot: a far-behind client never
/// replays the delete op (its backlog starts after it), so tombstones
/// must ship with the snapshot or the row would resurrect on the next
/// stale replay. Lib-owned and generic — deleted rows carry no data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone<T: SyncTable> {
    pub table: T,
    pub id: Uuid,
    pub deleted_at: Timestamp,
}
