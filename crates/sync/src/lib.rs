//! Client-side sync machinery: the local PGlite database, the websocket
//! sync engine, and the reactive query layer over it. App code provides
//! row mappings ([`query::SyncRow`]) and per-table row sinks; this crate
//! owns everything else — the engine singleton, the connection lifecycle,
//! the offline queue, snapshots, and the PGlite bridge.

pub mod engine;
pub mod pglite;
pub mod query;

pub use engine::{Engine, EngineError, STATUS, engine, init};
pub use pglite::Pglite;
