//! Why a sync-engine call failed, surfaced to call sites via
//! `Signal<Result<..>>` and the [`LAST_ERROR`](super::LAST_ERROR) global.
//! Serializable: apply failures relay to subordinate tabs over
//! BroadcastChannel (the notice overlay must work in every tab).

use crate::from_row::RowError;
use crate::pglite::BridgeError;
use crate::protocol::SchemaVersionError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum EngineError {
    /// The app's `SCHEMA_VERSION` const is not MAJOR.MINOR.PATCH — a
    /// setup bug surfaced at `init` instead of a hidden panic the
    /// embedding app cannot avoid or parse. The offending input rides
    /// along (TimestampError pattern); serializable, so the relay keeps
    /// working.
    #[error("invalid schema version: {0}")]
    BadVersion(#[from] SchemaVersionError),
    /// The local DB could not be opened or migrated (storage, wasm heap).
    #[error("db init failed: {0}")]
    DbInit(BridgeError),
    /// The statement itself failed at the PGlite/JS boundary.
    #[error("sql failed: {0}")]
    Sql(#[from] BridgeError),
    /// Rows came back but didn't map to the result type.
    #[error("row mapping failed: {0}")]
    Mapping(#[from] RowError),
    /// The app never registered a row sink for this table — a setup bug.
    /// Surfaced instead of faking an empty success: the cursor would
    /// otherwise advance past data the client silently dropped. The table
    /// is its wire name (`SyncTable::as_str`) — errors are T-free so the
    /// LAST_ERROR global signal stays possible (no generic statics).
    #[error("no row sink registered for {0:?}")]
    NoSink(String),
    /// A registered row sink failed to write its batch (payload didn't
    /// parse, or the upsert SQL failed).
    #[error("sink failed: {0}")]
    Sink(String),
}
