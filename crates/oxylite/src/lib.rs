//! Sync machinery for offline-first apps, both sides of the wire.
//!
//! The protocol is the lib's own now (#31): the wire DTOs
//! ([`protocol::Op`] and friends) and the timestamp type live here,
//! generic over the app's table enum — the dependency arrow points the
//! right way: the lib owns the protocol, tables, and machinery; apps
//! implement [`SyncTable`] for their table enum and [`SyncRow`] for
//! their row types (one declaration feeds both appliers).
//!
//! Client (default features, wasm): the local PGlite database, the
//! websocket sync engine, and the reactive query layer over it. This
//! crate owns the engine singleton, the connection lifecycle, the
//! offline queue, snapshots, and the PGlite bridge.
//!
//! Server (feature `server`, native only): the protocol machinery every
//! backend needs — push/pull, the per-connection session decisions, and
//! the snapshot pipeline, generic over the app's table type. The app
//! provides the per-table SQL ([`server::OpApply`] and
//! [`server::SnapshotSource`], where the checked SQL lives) and derives
//! `Sequence` on its table enum so the lib can iterate every variant.
//!
//! Transport (feature `axum`, implies `server`): one drop-in WS route
//! for axum backends ([`ws::sync_router`]). Framework mounts are
//! separate features by design — a future actix backend would add its
//! own module over the same core.

// ---- the protocol core: ungated, pure (serde/uuid/chrono only) ----

pub mod delete;
pub mod protocol;
pub mod sync_row;
pub mod table;
pub mod timestamp;

#[cfg(feature = "client")]
pub mod from_row;

#[cfg(feature = "client")]
pub mod engine;
#[cfg(feature = "client")]
pub mod pglite;
#[cfg(feature = "client")]
pub mod query;
#[cfg(feature = "client")]
mod tabs;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "axum")]
pub mod ws;

pub use protocol::{ClientMsg, Op, ServerMsg, TableData, Tombstone};
pub use sync_row::SyncRow;
pub use table::{SyncTable, SyncTableWire};
pub use timestamp::{Timestamp, TimestampError};

/// The Postgres schema (namespace) the lib owns on BOTH sides — the
/// server's real Postgres and the client's PGlite (#32). Every protocol
/// table the lib creates lives here, so an app's own tables can never
/// collide with it and the lib's footprint in the app's database is ONE
/// namespace. The app's replicated tables are the app's business — the
/// lib never names where they live (boundary rule 1). Renames are one
/// migration; the wire never sees this name.
pub const SCHEMA: &str = "oxylite";

#[cfg(feature = "client")]
pub use engine::{Engine, EngineError, STATUS, engine, init, timer_pause};
#[cfg(feature = "client")]
pub use from_row::{FromRow, Row, RowError, Type, str_field, str_field_req};
#[cfg(feature = "client")]
pub use pglite::{BridgeError, Pglite};

// The lib's own migration list is generated at build time (see
// build.rs): a scan of migrations/, sorted lexicographically (= applied
// order, the same contract sqlx::migrate! follows in `server::migrate`),
// each file embedded with include_str!. Apps concatenate this with
// their own list for the client's apply-once boot.
include!(concat!(env!("OUT_DIR"), "/migrations.rs"));
