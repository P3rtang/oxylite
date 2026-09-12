//! Delete semantics, pinned in pure code: how an `Op` maps onto storage
//! (`data: null` ⇒ delete) and the tombstone guard's decisions, plus the
//! in-memory reference model every real applier (server SQL, client
//! bridge) must agree with. No database — the contract is decision
//! logic, and the SQL sides are pinned elsewhere (server sqlx::tests,
//! client e2e, client SQL prepare-checks).
//!
//! Runs against a LOCAL fixture table enum (#31): the lib no longer
//! depends on the app's `shared` crate, and this suite doubles as the
//! proof that a consuming app can instantiate the whole protocol with
//! nothing but `sync` itself.

use enum_iterator::Sequence;
use oxylite::contract::table::SyncTable;
use oxylite::protocol::Op;
use oxylite::protocol::delete::{OpExt, OpKind, tombstone_allows};
use oxylite::protocol::timestamp::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// The consuming app's table enum, fixture flavor. Same shape the notes
/// app's `shared::Table` has: Sequence-derived, lowercase wire names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FixtureTable {
    Notes,
}

impl SyncTable for FixtureTable {
    fn as_str(self) -> &'static str {
        "notes"
    }

    fn from_name(name: &str) -> Option<Self> {
        (name == "notes").then_some(Self::Notes)
    }
}

/// Parse in the helper: test fixtures get the same validation the wire
/// does — a malformed fixture fails here, not downstream.
fn ts(s: &str) -> Timestamp {
    Timestamp::parse(s).unwrap()
}

fn op(data: serde_json::Value, updated_at: &str) -> Op<FixtureTable> {
    Op {
        table: FixtureTable::Notes,
        id: Uuid::now_v7(),
        data,
        updated_at: ts(updated_at),
    }
}

fn note(title: &str, updated_at: &str) -> Op<FixtureTable> {
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
    assert!(tombstone_allows(&ts(T0), None));
    assert!(tombstone_allows(&ts(T2), None));
}

#[test]
fn stale_edit_drops() {
    // Offline client pushes an edit older than the delete: dropped.
    assert!(!tombstone_allows(&ts(T1), Some(&ts(T2))));
}

#[test]
fn equal_timestamp_drops() {
    // A delete is final unless the write is strictly newer.
    assert!(!tombstone_allows(&ts(T2), Some(&ts(T2))));
}

#[test]
fn newer_edit_resurrects() {
    assert!(tombstone_allows(&ts(T3), Some(&ts(T2))));
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
    rows: HashMap<Uuid, Timestamp>,       // id -> updated_at
    tombstones: HashMap<Uuid, Timestamp>, // id -> deleted_at
    rejected: usize,
}

impl Store {
    /// One event, applied per the LWW + tombstone contract.
    fn apply(&mut self, op: &Op<FixtureTable>) {
        let deleted_at = self.tombstones.get(&op.id);
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
                    .is_some_and(|row_at| op.updated_at <= *row_at)
                {
                    self.rejected += 1;
                    return;
                }
                self.rows.remove(&op.id);
                self.tombstones.insert(op.id, op.updated_at);
            }
            OpKind::Upsert => {
                // Row-level LWW: older row state never overwrites newer.
                if self
                    .rows
                    .get(&op.id)
                    .is_some_and(|row_at| op.updated_at <= *row_at)
                {
                    self.rejected += 1;
                    return;
                }
                self.rows.insert(op.id, op.updated_at);
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
    assert_eq!(s.tombstones.get(&id), Some(&ts(T2)));
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
    assert_eq!(s.tombstones.get(&id), Some(&ts(T2)));
}

fn note_at(id: Uuid, at: &str) -> Op<FixtureTable> {
    Op {
        table: FixtureTable::Notes,
        id,
        data: serde_json::json!({ "title": "n", "body": "", "updated_at": at }),
        updated_at: ts(at),
    }
}

fn delete_at(id: Uuid, at: &str) -> Op<FixtureTable> {
    Op {
        table: FixtureTable::Notes,
        id,
        data: serde_json::Value::Null,
        updated_at: ts(at),
    }
}
