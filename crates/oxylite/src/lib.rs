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
//! Pub/sub (feature `pubsub`, implied by `server`): the in-process wake
//! bus ([`pubsub::Bus`]) backends fan server-side events out over —
//! transport-free and sqlx-free, so it stands apart from the sync
//! machinery entirely (ROADMAP 2.2).
//!
//! Transport (feature `axum`, implies `server`): one drop-in WS route
//! for axum backends ([`ws::sync_router`]). Framework mounts are
//! separate features by design — a future actix backend would add its
//! own module over the same core.

// ---- the protocol core: wire DTOs + op semantics + timestamp, pure
// ---- (serde/uuid/chrono only)

pub mod contract;
pub mod protocol;

#[cfg(feature = "client")]
pub mod client;

#[cfg(feature = "pubsub")]
pub mod pubsub;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "axum")]
pub mod ws;

pub use contract::sync_row::{SqlType, SyncRow};
pub use contract::table::{SyncTable, SyncTableWire};
pub use protocol::timestamp::{Timestamp, TimestampError};
pub use protocol::{ClientMsg, Op, ServerMsg, TableData, Tombstone};

mod migration_list;
pub use migration_list::{migrations_merged, migrations_total};

// The migration standard's compile-time half: `oxylite::migrations!(
// "migrations")` embeds the consumer's `migrations/` dir as a list of
// `(NNNN_name, sql)` pairs — the APP's list only. The lib's protocol
// migrations merge in at the boot/migrator entry points (the runtime
// half above); a consumer never names them.
pub use oxylite_migrations::migrations;

/// The Postgres schema (namespace) the lib owns on BOTH sides — the
/// server's real Postgres and the client's PGlite (#32). Every protocol
/// table the lib creates lives here, so an app's own tables can never
/// collide with it and the lib's footprint in the app's database is ONE
/// namespace. The app's replicated tables are the app's business — the
/// lib never names where they live (boundary rule 1). Renames are one
/// migration; the wire never sees this name.
pub const SCHEMA: &str = "oxylite";

#[cfg(feature = "client")]
pub use client::engine::{Engine, EngineError, STATUS, engine, init, timer_pause};
#[cfg(feature = "client")]
pub use client::pglite::{BridgeError, Pglite};
#[cfg(feature = "client")]
pub use contract::from_row::{FromJs, FromRow, Row, RowError, Type};

// The lib's own migration list is generated at build time (see
// build.rs): a scan of migrations/, sorted lexicographically (= applied
// order, the same contract sqlx::migrate! follows in `server::migrator`),
// each file embedded with include_str!. It is NOT the consumer's
// business — `Pglite::init` / `server::migrator` merge it with the app's
// `migrations!` list automatically (migration_list.rs); apps never
// concatenate by hand.
include!(concat!(env!("OUT_DIR"), "/migrations.rs"));
