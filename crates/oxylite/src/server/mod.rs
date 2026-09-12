//! Server-side sync machinery (feature `server`): the snapshot pipeline
//! every backend of this protocol needs, generic over the app's table
//! type. Tables are defined by the implementing repo — they derive
//! `enum_iterator::Sequence`, implement [`crate::contract::table::SyncTable`] for
//! the wire names, and implement [`SnapshotSource`] for the per-table
//! extraction (which stays app-side: that's where the sqlx macros can
//! see the SQL literals, and where the exhaustive match forces new
//! tables to be handled). The `sync_log`/`snapshots` schema comes from
//! the lib's own migrations.
//!
//! Layout: [`ops`] the op pipeline (sync_log reads/writes, the
//! [`OpApply`] hook, retention, the runtime `apply_one`); [`snapshot`]
//! the bulk-load pipeline (extract, rebuild, serve); [`session`] the
//! per-connection protocol state behind a transport loop ([`crate::ws`]
//! is the axum flavor); [`wake`] the PgListener→bus adapter (2.2).

mod ops;
mod session;
mod snapshot;
mod wake;

pub use ops::{OpApply, apply_one, current_cursor, log_floor, prune_sync_log, pull_since, push};
pub use session::Session;
pub use snapshot::{
    SNAPSHOT_AFTER_OPS, SnapshotSource, SyncRowSnapshots, TableRows, load_or_build_snapshot,
    rebuild_snapshots,
};
pub use wake::{OPS_CHANNEL, spawn_wake_adapter};

use thiserror::Error;

/// Why a server sync operation failed. Typed so callers match on the shape
/// of the failure; sources convert with `#[from]` (errors-spec shape —
/// moved here with `pull_since`/`push`, which own these failure kinds).
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
