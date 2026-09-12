//! The wire protocol's message shapes — the lib's own contract now
//! (#31): the DTOs moved out of the app's `shared` crate and went
//! generic over the app's table enum, so nothing in the machinery
//! names a concrete table anymore. `T` serializes through its own
//! serde impls (the app enum keeps `rename_all = "lowercase"`), so the
//! wire bytes are IDENTICAL to the pre-generic protocol —
//! `spec/sync-protocol.md` describes the same bytes.

use crate::table::SyncTable;
use crate::timestamp::Timestamp;
use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The schema version both sides declare on the wire (semver text,
/// `MAJOR.MINOR.PATCH`). Maintained by the app — ONE const, both sides
/// build from the same crate. Compared MAJOR.MINOR only: patch is always
/// compatible; a major/minor mismatch walks a stale tab to a reload
/// instead of letting mixed-version windows drift (#34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaVersionError {
    pub input: String,
}

impl std::fmt::Display for SchemaVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} is not a MAJOR.MINOR.PATCH schema version",
            self.input
        )
    }
}

impl std::error::Error for SchemaVersionError {}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_text())
    }
}

impl SchemaVersion {
    pub fn parse(text: &str) -> Result<Self, SchemaVersionError> {
        let bad = || SchemaVersionError {
            input: text.to_string(),
        };
        let mut parts = text.split('.');
        let major = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        let minor = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        let patch = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    pub fn as_text(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }

    /// The wire compat rule (D2): same major AND minor — the patch field
    /// never gates a connection.
    pub fn wire_compatible(&self, other: &Self) -> bool {
        self.major == other.major && self.minor == other.minor
    }
}

// The wire carries the semver text; deserialization is the parse
// boundary (the same pattern as Timestamp).
impl Serialize for SchemaVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_text())
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        SchemaVersion::parse(&text).map_err(|e| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&e.input),
                &"a MAJOR.MINOR.PATCH version",
            )
        })
    }
}

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
    /// First frame of a session: the client's schema version. The server
    /// answers `Ready` (compatible) or `Incompatible` (the stale tab is
    /// walked to a reload — #34).
    Hello { version: SchemaVersion },
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
    /// Handshake accepted: the client's major.minor matches; the patch
    /// field never gates (#34).
    Ready { version: SchemaVersion },
    /// The client's schema version differs in major/minor: this session
    /// cannot proceed — the server closes right after sending, and the
    /// client reloads itself (the stale bundle is minutes old at most;
    /// a reload applies the new migrations on boot).
    Incompatible {
        server: SchemaVersion,
        client: SchemaVersion,
    },
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
