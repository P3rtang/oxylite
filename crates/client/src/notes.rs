//! App-level note glue: the client's row type, its mapping, op building,
//! and the row sink the engine dispatches to. All SQL comes from the
//! [`SyncRow`] contract — this file declares the mapping and the table's
//! LWW column. The engine stays generic: it never names a note.

use std::rc::Rc;

use serde::{Deserialize, Serialize};
use shared::timestamp::Timestamp;
use shared::{Op, Table};
use sync::engine::RowSink;
use sync::pglite::{Pglite, str_field};
use sync::query::{FromRow, Row, RowError, SyncRow, apply_ops};
use uuid::Uuid;

/// The client's note row. Defined app-side (not in `shared`) because the
/// library's traits are implemented for local types only — the wire
/// payload is an untyped JSON value whose shape is the contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub updated_at: Timestamp,
}

/// The engine sinks `Table::Notes` ops here (Events and Snapshots,
/// deletes included). Registered in main at startup — ownership requires
/// the mapping to live app-side, since it names `Note`.
pub fn notes_sink() -> RowSink {
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

/// Rows come back as JS objects; Note knows how to build itself.
impl FromRow for Note {
    fn from_row(row: &Row) -> Result<Self, RowError> {
        Ok(Note {
            id: str_field(row, "id")
                .ok_or_else(|| RowError::MissingColumn("id".into()))?
                .parse()
                .map_err(|_| RowError::BadUuid("id".into()))?,
            title: str_field(row, "title")
                .ok_or_else(|| RowError::MissingColumn("title".into()))?,
            body: str_field(row, "body").ok_or_else(|| RowError::MissingColumn("body".into()))?,
            updated_at: Timestamp::parse(
                str_field(row, "updated_at")
                    .ok_or_else(|| RowError::MissingColumn("updated_at".into()))?
                    .as_str(),
            )
            .map_err(|e| RowError::BadTimestamp("updated_at".into(), e))?,
        })
    }
}

/// One declaration feeds every generated statement: local inserts, the
/// list query, and the engine's batched LWW upserts all derive from this.
impl SyncRow for Note {
    const TABLE: Table = Table::Notes;
    const COLUMNS: &'static [&'static str] = &["id", "title", "body", "updated_at"];
    const LWW: Option<&'static str> = Some("updated_at");

    fn params(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            self.title.clone(),
            self.body.clone(),
            self.updated_at.canonical_text(),
        ]
    }

    fn pk(&self) -> Uuid {
        self.id
    }
}
