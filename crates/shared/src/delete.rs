//! Delete semantics: how an `Op` maps onto storage, and whether a write
//! may proceed past a tombstone. Shared by the server's apply path and
//! the client's applier so both sides decide identically.
//!
//! A delete is **an op like any other** — `{ table, id, updated_at,
//! data: null }` on the existing Push path. The null payload is the
//! marker: there is no row state to ship, only the fact of removal.

use crate::Op;
use crate::timestamp::Timestamp;

/// Whether an `Op` upserts row state or deletes its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Upsert,
    Delete,
}

impl Op {
    /// `data: null` ⇒ delete; anything else is row state (upsert).
    pub fn kind(&self) -> OpKind {
        if self.data.is_null() {
            OpKind::Delete
        } else {
            OpKind::Upsert
        }
    }
}

/// The tombstone guard: may a write with `updated_at` proceed past the
/// row's tombstone (if any)?
///
/// Without it, a stale edit (pushed by a client that was offline before
/// the delete) would resurrect the row: the upsert guard
/// `ON CONFLICT ... WHERE excluded.updated_at > updated_at` cannot fire
/// on an absent row — there is no conflict, it is a plain INSERT.
///
/// Equal timestamps drop (`<=`): a delete is final unless the write is
/// strictly newer. A strictly newer write passes AND clears the
/// tombstone (resurrection) — coherent last-writer-wins.
///
/// Orderings are instant orderings (`Timestamp` = `chrono::DateTime`,
/// mirrored in SQL by the `timestamptz` columns) — no text reasoning
/// anywhere.
pub fn tombstone_allows(updated_at: &Timestamp, deleted_at: Option<&Timestamp>) -> bool {
    match deleted_at {
        None => true,
        Some(deleted_at) => updated_at > deleted_at,
    }
}
