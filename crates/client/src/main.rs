//! Offline-first notes UI. All data flows through the sync engine (the
//! `sync` crate): this file only wires components — one live query for the
//! list, one submit path, no sockets, no global state beyond the engine.

mod notes;

use dioxus::prelude::*;
use notes::{Note, new_note, notes_applier, op_for_note};
use shared::Table;
use sync::engine::{self, engine};
use sync::query::{SyncRow, use_all};

fn main() {
    console_error_panic_hook::set_once();
    engine::init(shared::MIGRATIONS);
    engine().register_applier(Table::Notes, notes_applier());
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // The entire data integration: one live query. Re-runs whenever the
    // notes table changes, locally or via server events. Errors surface
    // here instead of a silently empty list.
    let result = use_all::<Note>();
    let notes_state = result.read().clone();
    let status = engine::STATUS.read().clone();
    let mut title_input = use_signal(String::new);

    // The only long-lived task in the app: the engine's connect loop.
    use_effect(move || {
        spawn(async move {
            engine().run().await;
        });
    });

    rsx! {
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
                for note in notes_state.as_ref().unwrap_or(&Vec::new()).iter() {
                    li { key: "{note.id}",
                        strong { "{note.title}" }
                        span { style: "color:#999", " · {note.updated_at}" }
                    }
                }
            }
        }
    }
}

/// Store the note locally, then push it (or queue it while offline). The
/// engine's invalidation bump re-runs the live list query — no manual
/// refresh, and the exact same path a remote event takes.
fn submit(mut title_input: Signal<String>) {
    let t = title_input.read().clone();
    if t.is_empty() {
        return;
    }
    title_input.set(String::new());

    spawn(async move {
        let e = engine();
        let note = new_note(&t);
        e.exec(
            &Note::insert_sql(),
            &note.params(),
            &[(Note::TABLE, note.id)],
        )
        .await;
        e.push(op_for_note(&note));
    });
}
