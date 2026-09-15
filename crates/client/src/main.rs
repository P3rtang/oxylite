//! Offline-first notes UI. All data flows through the sync engine (the
//! `sync` crate): this file only wires components — one live query for the
//! list, one submit path, no sockets, no global state beyond the engine.

mod notes;
mod notify;

use dioxus::prelude::*;
use notes::{Note, display_time, new_note, notes_sink, op_for_delete, op_for_note};
use notify::{Notice, NoticeOverlay, notify};
use oxylite::client::engine::{self, EngineError, engine};
use oxylite::client::query::use_select_all;
use shared::SyncRow;
use shared::Table;

fn main() {
    console_error_panic_hook::set_once();
    // A malformed SCHEMA_VERSION is a build-time setup bug: init surfaces
    // it as EngineError::BadVersion. Without init there is no engine —
    // launching would only hit the documented missing-singleton panic —
    // so the console error IS the message and the app stays down.
    if let Err(e) = engine::init::<Table>(shared::APP_MIGRATIONS, shared::SCHEMA_VERSION) {
        engine::log("boot", &format!("engine init failed: {e}"));
        return;
    }
    engine::<Table>().register_sink(Table::Notes, notes_sink());
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // The entire data integration: one live query. Re-runs whenever the
    // notes table changes, locally or via server events. Errors surface
    // here instead of a silently empty list.
    let result = use_select_all::<Note>();
    let notes_state = result.read().clone();
    let notes = notes_state.clone().unwrap_or_default();
    let status = engine::STATUS.read().clone();
    let mut title_input = use_signal(String::new);

    // Sync-layer failures surface centrally: the engine publishes its last
    // apply failure, the app turns each NEW one into a notice.
    let mut shown_error = use_signal(|| None::<EngineError>);
    use_effect(move || {
        let err = engine::LAST_ERROR.read().clone();
        if let Some(e) = err
            && shown_error.read().as_ref() != Some(&e)
        {
            notify(Notice::new("Sync failed", e.to_string()));
            shown_error.set(Some(e));
        }
    });

    // The only long-lived task in the app: the engine's connect loop.
    use_effect(move || {
        spawn(async move {
            engine::<Table>().run().await;
        });
    });

    rsx! {
        NoticeOverlay {}
        div { style: "max-width: 640px; margin: 2rem auto; font-family: system-ui, sans-serif",
            h1 { "Offline Notes" }
            p { style: "color:#666",
                "{status} · local: PGlite (IndexedDB) · remote: Postgres via WS"
            }
            div { style: "display:flex; gap:8px",
                input {
                    style: "flex:1; padding:6px",
                    value: "{title_input}",
                    placeholder: "Note title…",
                    oninput: move |e| title_input.set(e.value().to_string()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            submit(title_input);
                        }
                    },
                }
                button {
                    style: "padding:6px 14px",
                    onclick: move |_| submit(title_input),
                    "Add"
                }
            }
            if let Err(e) = &notes_state {
                p { style: "color:#b00", "load failed: {e}" }
            }
            ul { style: "margin-top: 1rem; line-height: 1.8",
                for note in notes {
                    li { key: "{note.id}",
                        strong { "{note.title}" }
                        span { style: "color:#999", " · {display_time(&note.updated_at)}" }
                        button {
                            style: "margin-left: 8px; padding: 0 6px; cursor: pointer",
                            title: "Delete note",
                            onclick: move |_| delete_note(note.id),
                            "✕"
                        }
                    }
                }
            }
        }
    }
}

/// Store the note locally, then push it (or queue it while offline). The
/// engine's invalidation bump re-runs the live list query — no manual
/// refresh, and the exact same path a remote event takes.
///
/// Push FIRST (the engine persists the op write-ahead), then write
/// locally: a crash between the two leaves an op that resends on
/// reconnect, whose echo re-applies the row locally (#33). The local
/// write is therefore the same guarded upsert remote events use — an
/// echo racing this exec is an idempotent overwrite, never a
/// duplicate-key failure. A failed local write is surfaced, but the op
/// still syncs: the server is the source of truth.
fn submit(mut title_input: Signal<String>) {
    let t = title_input.read().clone();
    if t.is_empty() {
        return;
    }
    title_input.set(String::new());

    spawn(async move {
        let e = engine();
        let note = new_note(&t);
        e.push(op_for_note(&note)).await;
        if let Err(err) = e
            .exec(
                &Note::guarded_upsert_sql(1),
                &note.params(),
                &[(Note::TABLE, note.id)],
            )
            .await
        {
            notify(Notice::new("Write failed", err.to_string()));
        }
    });
}

/// Remove the note locally (real delete — the row is gone; the guarded
/// statement tombstones exactly what it removed), then push the delete op
/// (or queue it while offline). Same path a remote delete event takes on
/// the receiving side.
///
/// Push FIRST, like `submit`: the op is durable before the local delete
/// runs, and the delete statement is already idempotent (a replayed or
/// echoed delete against a gone row is a no-op).
fn delete_note(id: uuid::Uuid) {
    spawn(async move {
        let e = engine();
        let op = op_for_delete(id);
        e.push(op.clone()).await;
        if let Err(err) = e
            .exec(
                &Note::delete_sql(1),
                &[id.to_string(), op.updated_at.canonical_text()],
                &[(Note::TABLE, id)],
            )
            .await
        {
            notify(Notice::new("Delete failed", err.to_string()));
        }
    });
}
