//! App-level note glue: op building and the row sink the engine
//! dispatches to. The row type is `shared::Note` — ONE declaration
//! (trait impl + decode) feeds both sides (#30); this file stays
//! client-only glue. All SQL comes from the [`SyncRow`] contract; the
//! engine stays generic: it never names a note.

use std::rc::Rc;

use oxylite::engine::RowSink;
use oxylite::pglite::Pglite;
use oxylite::query::apply_ops;
pub use shared::Note;
use shared::Timestamp;
use shared::{Op, Table};
use uuid::Uuid;

/// The engine sinks `Table::Notes` ops here (Events and Snapshots,
/// deletes included). Registered in main at startup — ownership requires
/// the mapping to live app-side, since it names the row type.
pub fn notes_sink() -> RowSink<Table> {
    Rc::new(|db: &Pglite, ops: &[Op]| Box::pin(apply_ops::<Note>(db, ops)))
}

pub fn new_note(title: &str) -> Note {
    Note {
        // UUIDv7: time-ordered, so both the local and remote primary-key
        // indexes stay hot and rows sort by creation.
        id: Uuid::now_v7(),
        title: title.into(),
        body: String::new(),
        updated_at: now_timestamp(),
    }
}

/// The platform clock, as a `Timestamp`: `Date.now()` epoch millis →
/// `from_epoch_millis`. No strings, no parse, no failure path on a write
/// (real clock values are always inside chrono's range).
fn now_timestamp() -> Timestamp {
    Timestamp::from_epoch_millis(js_sys::Date::now() as i64)
        .expect("js clock value out of chrono's range")
}

/// Display form: `2026-09-09T12:34:56.789Z` renders as
/// `2026-09-09 12:34:56`. Display-only — storage stays canonical.
pub fn display_time(ts: &Timestamp) -> String {
    let iso = ts.canonical_text();
    iso.get(..19)
        .unwrap_or(&iso)
        .replacen('T', " ", 1)
        .to_string()
}

pub fn op_for_note(note: &Note) -> Op {
    Op {
        table: Table::Notes,
        id: note.id,
        data: serde_json::to_value(note).unwrap(),
        updated_at: note.updated_at,
    }
}

/// A delete is an op like any other: the null payload is the marker.
pub fn op_for_delete(id: Uuid) -> Op {
    Op {
        table: Table::Notes,
        id,
        data: serde_json::Value::Null,
        updated_at: now_timestamp(),
    }
}
