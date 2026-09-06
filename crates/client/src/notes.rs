//! App-level note glue: row mapping, op building, and the per-table event
//! applier. All SQL comes from the [`SyncRow`] contract (query.rs) — this
//! file declares the mapping and the table's LWW column, nothing more.
//! The engine layer stays generic — it never names `Note`.

use crate::engine::ApplyOp;
use crate::pglite::{Pglite, str_field};
use crate::query::{FromRow, Row, SyncRow, bulk_upsert};
use shared::{Note, Op, Table};
use uuid::Uuid;
use wasm_bindgen::JsValue;

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
        table: Note::TABLE,
        id: note.pk(),
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

/// Rows come back as JS objects; the mapping above covers them.
impl ApplyOp for Table {
    /// Batched apply for Events payloads and Snapshots alike — the generic
    /// path handles parse, last-writer dedup and chunked upserts; this impl
    /// only maps the enum variant to its row type.
    async fn apply_rows(
        self,
        db: &Pglite,
        rows: &[serde_json::Value],
    ) -> Result<Vec<Uuid>, JsValue> {
        match self {
            Table::Notes => bulk_upsert::<Note>(db, rows)
                .await
                .map_err(|e| JsValue::from_str(&e)),
        }
    }
}
