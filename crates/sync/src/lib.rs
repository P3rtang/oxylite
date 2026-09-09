//! Sync machinery for offline-first apps, both sides of the wire.
//!
//! Client (default features, wasm): the local PGlite database, the
//! websocket sync engine, and the reactive query layer over it. App code
//! provides row mappings ([`query::SyncRow`]) and per-table row sinks;
//! this crate owns everything else — the engine singleton, the connection
//! lifecycle, the offline queue, snapshots, and the PGlite bridge.
//!
//! Server (feature `server`, native only): the snapshot pipeline every
//! backend needs, generic over the app's table type — freshness policy,
//! rebuild loop, and serving. The app provides the per-table extraction
//! ([`server::SnapshotSource`], where the checked SQL lives) and derives
//! `Sequence` on its table enum so the lib can iterate every variant.

pub mod engine;
pub mod pglite;
pub mod query;

#[cfg(feature = "server")]
pub mod server;

pub use engine::{Engine, EngineError, STATUS, engine, init, timer_pause};
pub use pglite::{BridgeError, Pglite};
