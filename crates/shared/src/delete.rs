//! Delete semantics: how an `Op` maps onto storage, and whether a write
//! may proceed past a tombstone. Shared by the server's apply path and
//! the client's applier so both sides decide identically.
//!
//! A delete is **an op like any other** — `{ table, id, updated_at,
//! data: null }` on the existing Push path. The null payload is the
//! marker: there is no row state to ship, only the fact of removal.

use crate::Op;

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
/// Relies on the protocol invariant that `updated_at` is canonical
/// ISO-8601 text of uniform precision on both sides, so lexicographic
/// compare is chronological.
pub fn tombstone_allows(updated_at: &str, deleted_at: Option<&str>) -> bool {
    match deleted_at {
        None => true,
        Some(deleted_at) => updated_at > deleted_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Table;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn op(data: serde_json::Value, updated_at: &str) -> Op {
        Op {
            table: Table::Notes,
            id: Uuid::now_v7(),
            data,
            updated_at: updated_at.to_string(),
        }
    }

    fn note(title: &str, updated_at: &str) -> Op {
        op(
            serde_json::json!({ "title": title, "body": "", "updated_at": updated_at }),
            updated_at,
        )
    }

    #[test]
    fn upsert_ops_carry_row_state() {
        assert_eq!(note("t", T1).kind(), OpKind::Upsert);
    }

    #[test]
    fn null_payload_is_a_delete() {
        assert_eq!(op(serde_json::Value::Null, T1).kind(), OpKind::Delete);
    }

    #[test]
    fn no_tombstone_allows_every_write() {
        assert!(tombstone_allows(T0, None));
        assert!(tombstone_allows(T2, None));
    }

    #[test]
    fn stale_edit_drops() {
        // Offline client pushes an edit older than the delete: dropped.
        assert!(!tombstone_allows(T1, Some(T2)));
    }

    #[test]
    fn equal_timestamp_drops() {
        // A delete is final unless the write is strictly newer.
        assert!(!tombstone_allows(T2, Some(T2)));
    }

    #[test]
    fn newer_edit_resurrects() {
        assert!(tombstone_allows(T3, Some(T2)));
    }

    // --- reference model: the applier's decision sequence, in memory ---
    //
    // Production applies via SQL (tombstone check + ON CONFLICT WHERE);
    // this simulation pins the same contract in pure code so both sides
    // can be diffed against it.

    const T0: &str = "2026-09-09T10:00:00.000Z";
    const T1: &str = "2026-09-09T10:00:01.000Z";
    const T2: &str = "2026-09-09T10:00:02.000Z";
    const T3: &str = "2026-09-09T10:00:03.000Z";

    #[derive(Default)]
    struct Store {
        rows: HashMap<Uuid, String>,       // id -> updated_at
        tombstones: HashMap<Uuid, String>, // id -> deleted_at
        rejected: usize,
    }

    impl Store {
        /// One event, applied per the LWW + tombstone contract.
        fn apply(&mut self, op: &Op) {
            let deleted_at = self.tombstones.get(&op.id).map(String::as_str);
            if !tombstone_allows(&op.updated_at, deleted_at) {
                self.rejected += 1;
                return;
            }
            match op.kind() {
                OpKind::Delete => {
                    // Deletes are writes too: an older delete loses to a
                    // newer resurrected row entirely — no removal, no
                    // tombstone.
                    if self
                        .rows
                        .get(&op.id)
                        .is_some_and(|row_at| op.updated_at.as_str() <= row_at.as_str())
                    {
                        self.rejected += 1;
                        return;
                    }
                    self.rows.remove(&op.id);
                    self.tombstones.insert(op.id, op.updated_at.clone());
                }
                OpKind::Upsert => {
                    // Row-level LWW: older row state never overwrites newer.
                    if self
                        .rows
                        .get(&op.id)
                        .is_some_and(|row_at| op.updated_at.as_str() <= row_at.as_str())
                    {
                        self.rejected += 1;
                        return;
                    }
                    self.rows.insert(op.id, op.updated_at.clone());
                    // Resurrection clears the tombstone.
                    self.tombstones.remove(&op.id);
                }
            }
        }

        fn alive(&self, id: Uuid) -> bool {
            self.rows.contains_key(&id)
        }
    }

    #[test]
    fn stale_edit_after_delete_drops() {
        let mut s = Store::default();
        let id = Uuid::now_v7();
        s.apply(&note_at(id, T1));
        s.apply(&delete_at(id, T2));
        assert!(!s.alive(id));
        s.apply(&note_at(id, T1)); // same edit arriving late
        assert!(!s.alive(id));
        assert_eq!(s.rejected, 1);
    }

    #[test]
    fn newer_edit_resurrects_and_clears() {
        let mut s = Store::default();
        let id = Uuid::now_v7();
        s.apply(&note_at(id, T1));
        s.apply(&delete_at(id, T2));
        s.apply(&note_at(id, T3));
        assert!(s.alive(id));
        assert!(s.tombstones.is_empty());
        // And once resurrected, the old delete stops biting: a later
        // replay of it is row-LWW'd away, not tombstoned again.
        s.apply(&delete_at(id, T2));
        assert!(s.alive(id));
        assert!(s.tombstones.is_empty());
    }

    #[test]
    fn concurrent_deletes_are_idempotent() {
        let mut s = Store::default();
        let id = Uuid::now_v7();
        s.apply(&note_at(id, T0));
        s.apply(&delete_at(id, T2));
        s.apply(&delete_at(id, T2)); // at-least-once replay
        assert!(!s.alive(id));
        assert_eq!(s.tombstones.get(&id).map(String::as_str), Some(T2));
    }

    #[test]
    fn offline_stretch_delete_then_replay_in_order() {
        // The full offline story: create, edit, go offline, delete, come
        // back — events replay in seq order and land on the same state.
        let mut s = Store::default();
        let id = Uuid::now_v7();
        s.apply(&note_at(id, T0));
        s.apply(&note_at(id, T1));
        s.apply(&delete_at(id, T2));
        s.apply(&note_at(id, T1)); // stale replay of the mid edit
        assert!(!s.alive(id));
        assert_eq!(s.tombstones.get(&id).map(String::as_str), Some(T2));
    }

    fn note_at(id: Uuid, at: &str) -> Op {
        Op {
            table: Table::Notes,
            id,
            data: serde_json::json!({ "title": "n", "body": "", "updated_at": at }),
            updated_at: at.to_string(),
        }
    }

    fn delete_at(id: Uuid, at: &str) -> Op {
        Op {
            table: Table::Notes,
            id,
            data: serde_json::Value::Null,
            updated_at: at.to_string(),
        }
    }
}
