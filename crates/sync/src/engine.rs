//! The sync engine: the ONE global structure (embedded-book singleton, like
//! `Pglite`). It owns the websocket connection lifecycle, the sync cursor,
//! the offline pending queue, and the registry of live queries the
//! connection keeps in sync. Components read data through `query.rs` hooks
//! and never see a socket (design: SYNC_API.md).
//!
//! Concurrency model (wasm is single-threaded): JS callbacks (socket
//! onmessage) run without a dioxus scope, so they only enqueue
//! (`recv_text`); one master task (`run`) drives connect/reconnect and
//! drains the inbox. Methods borrow their interior RefCells only for the
//! duration of a synchronous call — never across an `await` — so a callback
//! firing mid-await can never hit a borrowed RefCell.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use uuid::Uuid;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::WebSocket;

use crate::pglite::{self, Pglite};
use crate::query::{Query, Sub, SubId};
use dioxus::prelude::{Global, Signal, WritableExt};
use shared::{ClientMsg, Op, ServerMsg, Table};

/// Connection status for the UI status line. A GlobalSignal because it is
/// engine-owned (not per-component) and must initialize outside any dioxus
/// scope — plain `Signal::new` panics outside the runtime.
pub static STATUS: Global<Signal<String>, String> = Signal::global(|| "starting…".to_string());

thread_local! {
    static ENGINE: RefCell<Option<Rc<Engine>>> = const { RefCell::new(None) };
}

/// Why a query call failed, surfaced to call sites via `Signal<Result<..>>`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The local DB could not be opened or migrated (storage, wasm heap).
    #[error("db init failed: {0}")]
    DbInit(String),
    /// The SQL failed or a row didn't map to the result type (FromRow).
    #[error("query failed: {0}")]
    Query(String),
}

pub struct Engine {
    /// The app's schema migrations, applied to every fresh local DB.
    migrations: &'static [(&'static str, &'static str)],
    /// Live queries: what the connection keeps in sync.
    subs: RefCell<Vec<Sub>>,
    /// The socket while a session is open.
    sock: RefCell<Option<web_sys::WebSocket>>,
    /// Server messages parsed by the onmessage callback, awaiting the
    /// master task.
    inbox: RefCell<Vec<ServerMsg>>,
    /// Local writes that could not be delivered while offline.
    pending: RefCell<Vec<Op>>,
    /// Where we've streamed sync_log to; persisted in the client's meta
    /// table so it survives reloads.
    cursor: Cell<i64>,
    /// Per-table row sinks registered by the app.
    sinks: RefCell<HashMap<Table, RowSink>>,
}

/// Create the engine singleton. Called once from `main`, before launch, so
/// hooks and callbacks can always reach it.
pub fn init(migrations: &'static [(&'static str, &'static str)]) {
    ENGINE.with_borrow_mut(|slot| {
        *slot = Some(Rc::new(Engine {
            migrations,
            subs: RefCell::new(Vec::new()),
            sock: RefCell::new(None),
            inbox: RefCell::new(Vec::new()),
            pending: RefCell::new(Vec::new()),
            cursor: Cell::new(-1),
            sinks: RefCell::new(HashMap::new()),
        }));
    });
}

/// App-provided sink for one table's rows: writes that table's payload
/// rows (Events payloads or a Snapshot) into the local DB. The app owns
/// the mapping (it names its row types); the engine just sinks rows into
/// it — the dynamic-dispatch boundary of this library.
pub type RowSink = Rc<
    dyn for<'a> Fn(
        &'a Pglite,
        &'a [serde_json::Value],
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Uuid>, String>> + 'a>>,
>;

/// Handle to the engine singleton.
pub fn engine() -> Rc<Engine> {
    ENGINE.with_borrow(|e| e.clone().expect("engine initialized in main"))
}

impl Engine {
    /// Register the app's sink for one table: what the engine calls to
    /// write that table's payload rows into the local DB. The app owns the
    /// mapping (it names its row types); the engine just sinks rows into it.
    pub fn register_sink(&self, table: Table, sink: RowSink) {
        self.sinks.borrow_mut().insert(table, sink);
    }

    /// Sink one table's batch through its registered row sink.
    async fn apply_batch(
        &self,
        db: &Pglite,
        table: Table,
        rows: &[serde_json::Value],
    ) -> Result<Vec<Uuid>, String> {
        // Clone the Rc out so no borrow is held across the await.
        let sink = self.sinks.borrow().get(&table).cloned();
        match sink {
            Some(f) => (f)(db, rows).await,
            None => {
                log("sync", &format!("no sink registered for {:?}", table));
                Ok(Vec::new())
            }
        }
    }
    /// Register a live query; returns an RAII guard whose Drop unsubscribes
    /// (shared-ownership, so any clone of the guard keeps it alive).
    /// Re-registering the same query id refreshes its deps instead of
    /// duplicating.
    pub fn listen(&self, q: Query, rev: Signal<u64>) -> crate::query::Subscription {
        let mut subs = self.subs.borrow_mut();
        if let Some(existing) = subs.iter_mut().find(|s| s.id == q.id) {
            existing.deps = q.deps;
            existing.rev = rev;
        } else {
            subs.push(Sub {
                id: q.id,
                deps: q.deps,
                rev,
            });
        }
        crate::query::Subscription::new(q.id)
    }

    /// Remove a subscription (component unmounted).
    pub fn unsubscribe(&self, id: SubId) {
        self.subs.borrow_mut().retain(|s| s.id != id);
    }

    /// One-shot local read, mapped to `T` via its FromRow impl. Works
    /// regardless of connection state — reads never wait on the network.
    /// Failures are typed so call sites can render them (see use_query).
    pub async fn query<T: crate::query::FromRow>(
        &self,
        q: &crate::query::Query,
    ) -> Result<Vec<T>, EngineError> {
        let db = Pglite::init(self.migrations)
            .await
            .map_err(|e| EngineError::DbInit(error_text(&e)))?;
        let result = db
            .query(&q.sql, &q.params)
            .await
            .map_err(|e| EngineError::Query(error_text(&e)))?;
        pglite::rows_of(&result)
            .iter()
            .map(|row| T::from_row(row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(EngineError::Query)
    }

    /// Run a local write, then invalidate queries depending on `touched` —
    /// the same path remote events take, so offline writes light up the UI
    /// identically.
    pub async fn exec(&self, sql: &str, params: &[String], touched: &[(Table, Uuid)]) {
        let db = match Pglite::init(self.migrations).await {
            Ok(p) => p,
            Err(e) => {
                log("exec", &format!("db not ready: {}", error_text(&e)));
                return;
            }
        };
        match db.query(sql, params).await {
            Ok(_) => self.bump(touched),
            Err(e) => log("exec", &format!("write failed: {}", error_text(&e))),
        }
    }

    /// Deliver an operation: send it if the socket is OPEN, else queue it
    /// for the next connect (at-least-once; LWW makes retries harmless).
    pub fn push(&self, op: Op) {
        let open = self
            .sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
        if open {
            self.send(&ClientMsg::Push { ops: vec![op] });
        } else {
            self.pending.borrow_mut().push(op);
        }
    }

    /// Entry point for the socket's onmessage callback. Runs outside any
    /// dioxus scope, so it must never spawn: it parses and enqueues only.
    pub fn recv_text(&self, text: &str) {
        match serde_json::from_str::<ServerMsg>(text) {
            Ok(msg) => self.inbox.borrow_mut().push(msg),
            Err(e) => log("sync", &format!("bad message: {e}")),
        }
    }

    // ---- transparency (open questions #3: debug handle over privacy) ----

    /// One-line snapshot of engine state for debugging in the console.
    pub fn debug(&self) -> String {
        format!(
            "cursor={} live_queries={} pending={}",
            self.cursor.get(),
            self.subs.borrow().len(),
            self.pending.borrow().len()
        )
    }

    // ---- internals ----

    fn set_status(&self, text: &str) {
        *STATUS.write_unchecked() = text.into();
    }

    fn send(&self, msg: &ClientMsg) {
        if let Ok(text) = serde_json::to_string(msg)
            && let Some(ws) = self.sock.borrow().as_ref()
        {
            let _ = ws.send_with_str(&text);
        }
    }

    /// Fire the rev signal of every registered query whose deps match one
    /// of the touched (table, row) pairs. Owners re-run their queries.
    fn bump(&self, touched: &[(Table, Uuid)]) {
        for sub in self.subs.borrow().iter() {
            if sub
                .deps
                .iter()
                .any(|d| touched.iter().any(|(t, r)| d.matches(*t, *r)))
            {
                *sub.rev.write_unchecked() += 1;
            }
        }
    }

    /// The single long-lived task: connect, pull since cursor, flush
    /// pending, drain inbox, and reconnect with backoff on close.
    pub async fn run(&self) {
        let db = match Pglite::init(self.migrations).await {
            Ok(p) => p,
            Err(e) => {
                self.set_status(&format!("pglite failed: {}", error_text(&e)));
                return;
            }
        };
        log("boot", "pglite ready");
        self.cursor.set(load_cursor(&db).await);
        self.set_status("offline — local data loaded");
        log("boot", &self.debug());

        loop {
            self.set_status("connecting…");
            match open_socket(&sync_url()) {
                Ok(sock) => {
                    *self.sock.borrow_mut() = Some(sock.clone());
                    self.set_status("connected");
                    log("sync", "connected");
                    self.session(&db, &sock).await;
                    *self.sock.borrow_mut() = None;
                    log("sync", "disconnected — retrying in 3s");
                    self.set_status("offline — will retry…");
                    timer_pause(3000).await;
                }
                Err(e) => {
                    log("sync", &format!("connect failed: {e:?}"));
                    self.set_status("offline — will retry…");
                    timer_pause(3000).await;
                }
            }
        }
    }

    /// One connected session: flush the initial pull + pending pushes, then
    /// drain the inbox until the socket closes (or the handshake times out).
    async fn session(&self, db: &Pglite, sock: &WebSocket) {
        let mut flushed = false;
        let mut connecting_ms = 0u32;
        loop {
            match sock.ready_state() {
                WebSocket::CONNECTING => {
                    // A handshake can hang (firewall, proxy, mock); give up
                    // and let `run` reconnect rather than waiting forever.
                    connecting_ms += 100;
                    if connecting_ms > 5000 {
                        log("sync", "handshake timeout — treating as offline");
                        return;
                    }
                    timer_pause(100).await;
                }
                WebSocket::OPEN => {
                    if !flushed {
                        flushed = true;
                        self.send(&ClientMsg::Pull {
                            since: self.cursor.get(),
                        });
                        let outgoing = std::mem::take(&mut *self.pending.borrow_mut());
                        for op in outgoing {
                            self.send(&ClientMsg::Push { ops: vec![op] });
                        }
                    }
                    let messages = std::mem::take(&mut *self.inbox.borrow_mut());
                    for msg in messages {
                        self.handle_msg(db, msg).await;
                    }
                    timer_pause(150).await;
                }
                _ => return, // closed or closing: reconnect in `run`
            }
        }
    }

    async fn handle_msg(&self, db: &Pglite, msg: ServerMsg) {
        match msg {
            ServerMsg::Ack { .. } => {
                // The server may hold events we haven't seen; pull again.
                self.send(&ClientMsg::Pull {
                    since: self.cursor.get(),
                });
            }
            ServerMsg::Events { events, cursor: c } => {
                log(
                    "sync",
                    &format!("received {} events, cursor -> {}", events.len(), c),
                );
                self.cursor.set(c);
                save_cursor(db, c).await;
                // Every event already IS the invalidation notice — its
                // (table, row_id) rides along with the payload, so phase 1
                // needs no extra protocol messages (phase 2 can send a
                // payload-less Invalidate instead).
                let mut unique: Vec<Table> = Vec::new();
                for op in &events {
                    if !unique.contains(&op.table) {
                        unique.push(op.table);
                    }
                }
                let mut touched = Vec::with_capacity(events.len());
                for table in unique {
                    let rows: Vec<serde_json::Value> = events
                        .iter()
                        .filter(|o| o.table == table)
                        .map(|o| o.data.clone())
                        .collect();
                    match self.apply_batch(db, table, &rows).await {
                        Ok(ids) => touched.extend(ids.into_iter().map(|id| (table, id))),
                        Err(e) => log("sync", &format!("apply failed: {e}")),
                    }
                }
                // One bump per batch, not per op: replaying a backlog must
                // re-run each query once, not once per row (the UI would
                // visibly re-render row by row).
                self.bump(&touched);
            }
            ServerMsg::Snapshot { seq, tables } => {
                let rows_count: usize = tables.iter().map(|t| t.rows.len()).sum();
                log("sync", &format!("snapshot at {seq}: {rows_count} rows"));
                let mut touched = Vec::with_capacity(rows_count);
                for table_data in &tables {
                    match self
                        .apply_batch(db, table_data.table, &table_data.rows)
                        .await
                    {
                        Ok(ids) => {
                            touched.extend(ids.into_iter().map(|id| (table_data.table, id)));
                        }
                        Err(e) => log("sync", &format!("snapshot apply failed: {e}")),
                    }
                }
                self.cursor.set(seq);
                save_cursor(db, seq).await;
                self.bump(&touched);
                // The snapshot may already be behind the live log head —
                // pull the remainder right away (same re-pull as Ack).
                self.send(&ClientMsg::Pull { since: seq });
            }
        }
    }
}

/// Console logging for debugging in Chrome devtools.
pub fn log(kind: &str, msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("[{kind}] {msg}")));
}

pub(crate) fn error_text(e: &JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}

/// Open the sync websocket; parsed messages are enqueued via
/// [`Engine::recv_text`] and everything else is driven by `run`.
fn open_socket(url: &str) -> Result<WebSocket, JsValue> {
    let ws = WebSocket::new(url)?;

    let onmessage =
        wasm_bindgen::closure::Closure::wrap(Box::new(move |e: web_sys::MessageEvent| {
            if let Some(text) = e.data().as_string() {
                engine().recv_text(&text);
            }
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    Ok(ws)
}

/// Sync URL derived from the current page origin (single-origin deployment:
/// the client, the vendored PGlite bundle and the WS all live on the server).
fn sync_url() -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_else(|| "http://localhost:3000".into());
    format!("{}/sync", origin.replacen("http", "ws", 1))
}

/// Persisted sync cursor helpers (stored in PGlite's meta table so they
/// survive reloads).
async fn load_cursor(pglite: &Pglite) -> i64 {
    pglite
        .query("SELECT value FROM meta WHERE key = 'cursor'", &[])
        .await
        .ok()
        .and_then(|r| {
            pglite::rows_of(&r)
                .first()
                .and_then(|row| pglite::str_field(row, "value")?.parse().ok())
        })
        .unwrap_or(-1)
}

async fn save_cursor(pglite: &Pglite, cursor: i64) {
    let _ = pglite
        .query(
            "INSERT INTO meta (key, value) VALUES ('cursor', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            &[cursor.to_string()],
        )
        .await;
}

/// Sleep in the browser without tokio (wasm has no time driver).
pub(crate) async fn timer_pause(ms: u32) {
    let p = js_sys::Promise::new(&mut |resolve, _reject| {
        let cb = wasm_bindgen::closure::Closure::wrap(Box::new(move || {
            let _ = resolve.call0(&wasm_bindgen::JsValue::NULL);
        }) as Box<dyn FnMut()>);
        let _ = web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                ms as i32,
            );
        cb.forget();
    });
    wasm_bindgen_futures::JsFuture::from(p).await.ok();
}
