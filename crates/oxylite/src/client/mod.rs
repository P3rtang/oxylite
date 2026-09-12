//! The client side of the lib (feature `client`, wasm): the PGlite
//! bridge ([`pglite`] — open, migrate, exec, the #34 local-compat epoch
//! gate), the sync engine ([`engine`] — singleton, connection
//! lifecycle, durable op log, cross-tab relay), and the reactive query
//! layer over the local DB ([`query`]). The server side mirrors this
//! under [`crate::server`]; the transport mounts ([`crate::ws`], future
//! actix) stay top-level siblings by design (#28 r2).

pub mod engine;
pub mod pglite;
pub mod query;
