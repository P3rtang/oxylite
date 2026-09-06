//! Reactive query layer: firestore-shaped reads over the local DB.
//!
//! Components call [`use_query`] with a [`Query`] and get a signal of typed
//! rows that re-runs whenever the query's [`Dep`]s fire — a base-table change
//! or a specific row change (row-level listeners). No component ever touches
//! the websocket; the Engine owns that and bumps query revisions here.
//!
//! This module is structure only: every file declares its items, `mod.rs`
//! is imports/re-exports. Error types live in `engine.rs` (they cross the
//! whole client), the row contract (`SyncRow`) and its generic applier are
//! in `sync_row`, the reactive plumbing (`Query`/`Sub`/`Subscription`,
//! `use_query`/`use_all`) in their own files.

mod dep;
mod from_row;
mod hook;
mod statement;
mod sub;
mod sync_row;

// The module's public API, exported as one unit; a binary crate lints
// unused re-exports even though this is the intended surface.
#[allow(unused_imports)]
pub use dep::Dep;
#[allow(unused_imports)]
pub use from_row::{FromRow, Row};
#[allow(unused_imports)]
pub use hook::{use_all, use_query};
pub use statement::Query;
pub use sub::Subscription;
pub use sync_row::SyncRow;

pub(crate) use sub::{Sub, SubId};
pub(crate) use sync_row::bulk_upsert;
