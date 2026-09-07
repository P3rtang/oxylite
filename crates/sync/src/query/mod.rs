#![allow(unused_imports)]
//! Reactive query layer: firestore-shaped reads over the local DB.
//!
//! Components call [`use_query`] with a [`Query`] and get a signal of typed
//! rows that re-runs whenever the query's [`Dep`]s fire — a base-table change
//! or a specific row change (row-level listeners). No component ever touches
//! the websocket; the Engine owns that and bumps query revisions here.
//!
//! This module is structure only: every file declares its items, `mod.rs`
//! is imports/re-exports. Error types live in `engine.rs` (they cross the
//! whole client), the row contract (`SyncRow`) and its generic batch sink are
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
pub use dep::Dep;
pub use from_row::{FromRow, Row, RowError};
pub use hook::{use_query, use_select_all};
pub use statement::Query;
pub use sub::SubscriptionGuard;
pub use sync_row::SyncRow;

pub(crate) use sub::{Subscription, SubscriptionId};
pub use sync_row::bulk_upsert;
