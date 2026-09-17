//! examples/counter — the consumer ladder's third rung: the ENGINE.
//! Named counters with +/− over a SYNCED table: engine::init boots the
//! merged migration list, `use_select_all` is the entire read
//! integration (one live query, re-run on every local or remote
//! change), and every write is one engine call (`e.upsert(&row)` /
//! `e.delete::<Counter>(id)` — the op, the guarded SQL and the
//! invalidation bump are the lib's, not this file's). Server-side, the
//! example's axum server grew the real sync route — the miniature of
//! app #2.
//!
//! Gap ledger (reviewer: "we will find the gaps still open"):
//! - reactive re-render = one re-SELECT per bump — instant UI (the
//!   previous rung's optimistic pattern) is ROADMAP 4.7; this rung is
//!   deliberately demo-shaped to surface that.
//! - the atomic increment (count + delta) has no engine verb: the bump
//!   goes read-modify-write through upsert, so racing tabs can lose an
//!   increment under LWW (the op carries the row's new state, not a
//!   delta).
//! - everything else this file needed was already in the lib.
//!
//! Run: `scripts/oxylite.sh serve` from this directory (builds client +
//! server; the server needs Postgres — the repo's compose provides it,
//! the example's server creates its own `counter_demo` database) or
//! `watch` for hot reload (Dioxus.toml proxies /pglite AND /sync).
//! Server :3002 (hello-world keeps :3001).

use counter_shared::{Counter, CounterTable, SCHEMA_VERSION};
use dioxus::prelude::*;
use oxylite::client::engine::{self, engine};
use oxylite::client::query::use_select_all;
use uuid::Uuid;

fn main() {
    console_error_panic_hook::set_once();
    // A malformed SCHEMA_VERSION is a build-time setup bug: init
    // surfaces it as EngineError::BadVersion — the console error IS the
    // message (the demo's shape, verbatim).
    if let Err(e) = engine::init::<CounterTable>(counter_shared::APP_MIGRATIONS, SCHEMA_VERSION) {
        engine::log("boot", &format!("engine init failed: {e}"));
        return;
    }
    engine::<CounterTable>().register_sink(CounterTable::Counters, counters_sink());
    dioxus::launch(App);
}

/// The engine sinks `CounterTable::Counters` ops here (Events and
/// Snapshots, deletes included) — ownership requires the mapping to
/// live app-side, since it names the row type.
fn counters_sink() -> oxylite::client::engine::RowSink<CounterTable> {
    use oxylite::client::pglite::Pglite;
    use oxylite::client::query::apply_ops;
    std::rc::Rc::new(|db: &Pglite, ops: &[counter_shared::Op]| {
        Box::pin(apply_ops::<Counter>(db, ops))
    })
}

#[component]
fn App() -> Element {
    // The entire data integration: one live query. Re-runs whenever the
    // counters table changes, locally or via server events — the
    // hand-rolled load/refresh of the previous rung is gone.
    let result = use_select_all::<Counter>();
    let counters_state = result.read().clone();
    let counters = counters_state.clone().unwrap_or_default();
    let status = engine::STATUS.read().clone();
    let mut name_input = use_signal(String::new);
    // Write failures surface inline (the demo's notice overlay is
    // demo-owned glue — a gap noted for the template story).
    let error = use_signal(|| None::<String>);
    let error_state = error.read().clone();
    // Per-row move-captures, prepared OUTSIDE the rsx: the closures own
    // their row clone (an rsx for-body is an expression scope — no
    // statements inside).
    let rows: Vec<(Uuid, String, i64, Counter, Counter)> = counters
        .iter()
        .map(|c| (c.id, c.name.clone(), c.count, c.clone(), c.clone()))
        .collect();
    // The only long-lived task: the engine's connect loop.
    use_effect(move || {
        spawn(async move {
            engine::<CounterTable>().run().await;
        });
    });

    rsx! {
        div { style: "max-width: 640px; margin: 2rem auto; font-family: system-ui, sans-serif",
            h1 { "Counters" }
            p { style: "color:#666",
                "{status} · local: PGlite (IndexedDB) · remote: Postgres via WS"
            }
            div { style: "display:flex; gap:8px",
                input {
                    style: "flex:1; padding:6px",
                    value: "{name_input}",
                    placeholder: "Counter name…",
                    oninput: move |e| name_input.set(e.value().to_string()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            submit(name_input, error);
                        }
                    },
                }
                button {
                    style: "padding:6px 14px",
                    onclick: move |_| submit(name_input, error),
                    "Add"
                }
            }
            if let Err(e) = &counters_state {
                p { style: "color:#b00", "load failed: {e}" }
            }
            if let Some(e) = &error_state {
                p { style: "color:#b00", "write failed: {e}" }
            }
            if counters.is_empty() && counters_state.is_ok() {
                p { style: "color:#999; margin-top: 1rem", "no counters yet — add one" }
            }
            ul { style: "margin-top: 1rem; line-height: 2.4",
                for (id, name, count, minus, plus) in rows {
                    li { key: "{id}",
                        strong { "{name}" }
                        span { style: "color:#666; margin: 0 12px", "{count}" }
                        button {
                            style: "padding: 0 9px; cursor: pointer",
                            onclick: move |_| bump(minus.clone(), -1, error),
                            "−"
                        }
                        button {
                            style: "padding: 0 9px; margin-left: 4px; cursor: pointer",
                            onclick: move |_| bump(plus.clone(), 1, error),
                            "+"
                        }
                        button {
                            style: "padding: 0 6px; margin-left: 12px; cursor: pointer",
                            title: "Remove counter",
                            onclick: move |_| remove(id, error),
                            "✕"
                        }
                    }
                }
            }
        }
    }
}

/// One engine call: the op, the write-ahead, the guarded upsert and the
/// invalidation bump are the lib's. LWW ordering comes from the row's
/// own `updated_at` (stamped here, the engine's clock).
fn submit(mut name_input: Signal<String>, mut error: Signal<Option<String>>) {
    let name = name_input.read().trim().to_string();
    if name.is_empty() {
        return;
    }
    name_input.set(String::new());

    spawn(async move {
        let e = engine();
        let row = Counter {
            id: Uuid::now_v7(),
            name,
            count: 0,
            updated_at: e.now(),
        };
        if let Err(err) = e.upsert(&row).await {
            error.set(Some(err.to_string()));
        }
    });
}

/// GAP (see the header): an increment is a read-modify-write through the
/// row-shaped op — the current count comes from the live query, the new
/// count rides the op. Racing tabs can lose an increment; the op
/// protocol is row-shaped (data = the row's resulting state), so an
/// atomic delta needs an op-verb layer (future).
fn bump(c: Counter, delta: i64, mut error: Signal<Option<String>>) {
    spawn(async move {
        let e = engine();
        let row = Counter {
            count: c.count + delta,
            updated_at: e.now(),
            ..c
        };
        if let Err(err) = e.upsert(&row).await {
            error.set(Some(err.to_string()));
        }
    });
}

/// One engine call: the null-payload op, the write-ahead, the guarded
/// delete (it tombstones exactly what it removed — a stale edit cannot
/// resurrect the row).
fn remove(id: Uuid, mut error: Signal<Option<String>>) {
    spawn(async move {
        let e = engine();
        if let Err(err) = e.delete::<Counter>(id).await {
            error.set(Some(err.to_string()));
        }
    });
}
