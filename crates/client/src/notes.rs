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
pub const LIST_SQL: &str = "SELECT id, title, body, updated_at FROM notes ORDER BY updated_at DESC";

pub const INSERT_SQL: &str = "
    INSERT INTO notes (id, title, body, updated_at)
    VALUES ($1, $2, $3, $4)";

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

impl ApplyOp for Table {
    /// Batched apply for Events payloads and Snapshots alike: parse, dedup
    /// to the last row per id (log order wins — that's the LWW tiebreak),
    /// then one multi-row upsert per chunk. A fresh IndexedDB used to apply
    /// the whole history one awaited statement at a time.
    async fn apply_rows(
        self,
        db: &Pglite,
        rows: &[serde_json::Value],
    ) -> Result<Vec<Uuid>, JsValue> {
        match self {
            Table::Notes => {
                let mut order: Vec<Uuid> = Vec::with_capacity(rows.len());
                let mut by_id: std::collections::HashMap<Uuid, Note> =
                    std::collections::HashMap::with_capacity(rows.len());
                for v in rows {
                    let note: Note = serde_json::from_value(v.clone())
                        .map_err(|e| JsValue::from_str(&e.to_string()))?;
                    if !by_id.contains_key(&note.id) {
                        order.push(note.id);
                    }
                    // Same id twice in one batch: the later log entry wins.
                    by_id.insert(note.id, note);
                }
                let notes: Vec<Note> = order
                    .into_iter()
                    .map(|id| by_id.remove(&id).unwrap())
                    .collect();
                let ids: Vec<Uuid> = notes.iter().map(|n| n.id).collect();
                for chunk in notes.chunks(SNAPSHOT_CHUNK) {
                    upsert_notes(db, chunk).await?;
                }
                Ok(ids)
            }
        }
    }
}

/// Rows per bulk INSERT statement; 500 * 4 params stays well under PGlite's
/// host-parameter limit.
const SNAPSHOT_CHUNK: usize = 500;

async fn upsert_notes(db: &Pglite, notes: &[Note]) -> Result<(), JsValue> {
    if notes.is_empty() {
        return Ok(());
    }

    let mut sql = String::from("INSERT INTO notes (id, title, body, updated_at) VALUES ");
    let mut params: Vec<String> = Vec::with_capacity(notes.len() * 4);
    for (i, note) in notes.iter().enumerate() {
        let base = i * 4;
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!(
            "(${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4
        ));
        params.push(note.id.to_string());
        params.push(note.title.clone());
        params.push(note.body.clone());
        params.push(note.updated_at.clone());
    }
    sql.push_str(
        " ON CONFLICT (id) DO UPDATE
         SET title = EXCLUDED.title,
             body = EXCLUDED.body,
             updated_at = EXCLUDED.updated_at
       WHERE EXCLUDED.updated_at > notes.updated_at",
    );

    db.query(&sql, &params).await.map(|_| ())
}
