//! Sync machinery for offline-first apps, both sides of the wire.
//!
//! Client (default features, wasm): the local PGlite database, the
//! websocket sync engine, and the reactive query layer over it. App code
//! provides row mappings ([`query::SyncRow`]) and per-table row sinks;
//! this crate owns everything else — the engine singleton, the connection
//! lifecycle, the offline queue, snapshots, and the PGlite bridge.
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

pub mod delete;

pub mod engine;
pub mod pglite;
pub mod query;

// Cross-tab plumbing (Web Locks + BroadcastChannel): internal, reached
// through the engine's API only.
mod tabs;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "axum")]
pub mod ws;

pub use engine::{Engine, EngineError, STATUS, engine, init, timer_pause};
pub use pglite::{BridgeError, Pglite};
