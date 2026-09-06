//! App-level note glue: SQL, op building, row mapping, and the per-table
//! event applier. The engine layer stays generic — it never names `Note`
//! (SYNC_API.md, "Killing the hardcoded Note").

use crate::engine::ApplyOp;
use crate::pglite::{Pglite, str_field};
use crate::query::{Dep, FromRow, Query, Row};
use shared::{Note, Op, Table};
use uuid::Uuid;
use wasm_bindgen::JsValue;

/// Latest-first list of all notes.
pub const LIST_SQL: &str =
    "SELECT id, title, body, updated_at FROM notes ORDER BY updated_at DESC";

pub const INSERT_SQL: &str = "
    INSERT INTO notes (id, title, body, updated_at)
    VALUES ($1, $2, $3, $4)";

/// LWW upsert: a remote event only wins if it is strictly newer than what
/// we already hold (applies offline too, keeping local edits until a newer
/// remote edit arrives).
const UPSERT_NOTE_SQL: &str = "
    INSERT INTO notes (id, title, body, updated_at)
    VALUES ($1, $2, $3, $4)
    ON CONFLICT (id) DO UPDATE
      SET title = EXCLUDED.title,
          body = EXCLUDED.body,
          updated_at = EXCLUDED.updated_at
    WHERE EXCLUDED.updated_at > notes.updated_at";

/// The one live query the app needs today: all notes, re-run whenever the
/// notes table changes (any change) or a row's own dep fires.
pub fn list_query() -> Query {
    Query::new(LIST_SQL).dep(Dep::Table(Table::Notes))
}

pub fn new_note(title: &str) -> Note {
    Note {
        // UUIDv7: time-ordered, so both the local and remote primary-key
        // indexes stay hot and rows sort by creation.
        id: Uuid::now_v7(),
        title: title.into(),
        body: String::new(),
        updated_at: now_iso(),
    }
}

fn now_iso() -> String {
    js_sys::Date::new_0().to_iso_string().into()
}

pub fn op_for_note(note: &Note) -> Op {
    Op {
        table: Table::Notes,
        id: note.id,
        data: serde_json::to_value(note).unwrap(),
        updated_at: note.updated_at.clone(),
    }
}

/// Rows come back as JS objects; Note knows how to build itself.
impl FromRow for Note {
    fn from_row(row: &Row) -> Result<Self, String> {
        Ok(Note {
            id: str_field(row, "id")
                .ok_or("missing id")?
                .parse()
                .map_err(|_| "bad uuid".to_string())?,
            title: str_field(row, "title").ok_or("missing title")?,
            body: str_field(row, "body").ok_or("missing body")?,
            updated_at: str_field(row, "updated_at").ok_or("missing updated_at")?,
        })
    }
}

/// Remote events for the notes table become LWW upserts locally.
impl ApplyOp for Table {
    async fn apply_op(self, db: &Pglite, op: &Op) -> Result<(), JsValue> {
        match self {
            Table::Notes => {
                let note: Note = serde_json::from_value(op.data.clone())
                    .map_err(|e| JsValue::from_str(&e.to_string()))?;
                db.query(
                    UPSERT_NOTE_SQL,
                    &[note.id.to_string(), note.title, note.body, note.updated_at],
                )
                .await
                .map(|_| ())
            }
        }
    }
}
