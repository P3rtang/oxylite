//! App-level note glue: the client's row type, its mapping, op building,
//! and the applier the engine dispatches to. All SQL comes from the
//! [`SyncRow`] contract — this file declares the mapping and the table's
//! LWW column. The engine stays generic: it never names a note.

use std::rc::Rc;

use serde::{Deserialize, Serialize};
use shared::{Op, Table};
use sync::engine::Applier;
use sync::pglite::{Pglite, str_field};
use sync::query::{FromRow, Row, SyncRow, bulk_upsert};
use uuid::Uuid;

/// The client's note row. Defined app-side (not in `shared`) because the
/// library's traits are implemented for local types only — the wire
/// payload is an untyped JSON value whose shape is the contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub updated_at: String, // ISO 8601
}

/// The engine dispatches this for `Table::Notes` payloads (Events and
/// Snapshots). Registered in main at startup — ownership requires the
/// mapping to live app-side, since it names `Note`.
pub fn notes_applier() -> Applier {
    Rc::new(|db: &Pglite, rows: &[serde_json::Value]| Box::pin(bulk_upsert::<Note>(db, rows)))
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
            self.updated_at.clone(),
        ]
    }

    fn pk(&self) -> Uuid {
        self.id
    }
}
