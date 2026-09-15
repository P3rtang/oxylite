//! App-level note glue: op building and the row sink the engine
//! dispatches to. The row type is `shared::Note` — ONE declaration
//! (trait impl + decode) feeds both sides (#30); this file stays
//! client-only glue. All SQL comes from the [`SyncRow`] contract; the
//! engine stays generic: it never names a note.

use std::rc::Rc;

use oxylite::client::engine::RowSink;
use oxylite::client::pglite::Pglite;
use oxylite::client::query::apply_ops;
pub use shared::Note;
use shared::{Op, Table, Timestamp};
use uuid::Uuid;

/// The engine sinks `Table::Notes` ops here (Events and Snapshots,
/// deletes included). Registered in main at startup — ownership requires
/// the mapping to live app-side, since it names the row type.
pub fn notes_sink() -> RowSink<Table> {
    Rc::new(|db: &Pglite, ops: &[Op]| Box::pin(apply_ops::<Note>(db, ops)))
}

/// Row construction only — the op it rides and the write that lands it
/// are the engine's (`e.upsert(&note)`); the clock is injected (the
/// engine's `now()`), so no file here touches js_sys.
pub fn new_note(title: &str, now: Timestamp) -> Note {
    Note {
        // UUIDv7: time-ordered, so both the local and remote primary-key
        // indexes stay hot and rows sort by creation.
        id: Uuid::now_v7(),
        title: title.into(),
        body: String::new(),
        updated_at: now,
    }
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
