#![allow(unused_imports)]
//! Reactive query layer: firestore-shaped reads over the local DB.
//!
//! Components call [`use_query`] with a [`Query`] and get a signal of typed
//! rows that re-runs whenever the query's [`Dep`]s fire — a base-table change
//! or a specific row change (row-level listeners). No component ever touches
//! the websocket; the Engine owns that and bumps query revisions here.
//!
//! The row contract itself (`SyncRow`) and the JS-row decode (`FromRow`)
//! live in `shared` — one declaration feeds both sides (#30); this module
//! is the client half: the reactive plumbing and the generic batch sink.

mod apply;
mod dep;
mod hook;
mod statement;
mod sub;

// The module's public API, exported as one unit; a binary crate lints
// unused re-exports even though this is the intended surface.
pub use apply::{apply_ops, apply_tombstones};
pub use dep::Dep;
pub use hook::{use_query, use_select_all};
pub use statement::Query;
pub use sub::SubscriptionGuard;

pub(crate) use sub::{Subscription, SubscriptionId};
