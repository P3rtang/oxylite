//! The sync engine: the ONE global structure (embedded-book singleton, like
//! `Pglite`). It owns the websocket connection lifecycle, the sync cursor,
//! the offline pending queue, and the registry of live queries the
//! connection keeps in sync. Components read data through `query.rs` hooks
//! and never see a socket (design: SYNC_API.md).
//!
//! One engine per browser, not per tab (docs/impl/multi-tab.md): the first
//! tab to boot claims the browser-wide Web Lock and IS the engine — it owns
//! the single PGlite instance and the websocket. Every other tab becomes a
//! subordinate: it never opens PGlite (two live instances over one
//! IndexedDB lose writes — see the audit in impl/multi-tab.md) and proxies
//! every query, write and push to the leader over BroadcastChannel. The
//! leader fans invalidations and status back out to all tabs, so a write
//! anywhere lights up queries everywhere (local echo) and every tab shows
//! the same connection state. When the leader's document dies, the browser
//! grants the lock to one of the queued tabs, which promotes in place.
//!
//! Concurrency model (wasm is single-threaded): JS callbacks (socket
//! onmessage, BroadcastChannel onmessage) run without a dioxus scope, so
//! they only enqueue or apply synchronously; the leader drives the socket
//! (`run`) and serves tab requests (`serve_tabs`) in two long-lived tasks,
//! each borrowing interior RefCells only for the duration of a synchronous
//! call — never across an `await` — so a callback firing mid-await can
//! never hit a borrowed RefCell.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use uuid::Uuid;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::WebSocket;

use crate::SCHEMA;
use crate::from_row::{FromRow, RowError};
use crate::pglite::{self, BridgeError, Pglite};
use crate::protocol::{ClientMsg, Op, ServerMsg};
use crate::query::{Query, Subscription, SubscriptionGuard, SubscriptionId};
use crate::table::SyncTableWire;
use crate::tabs;
use crate::timestamp::Timestamp;
use dioxus::prelude::{Global, Signal, WritableExt};
use std::any::Any;

/// Connection status for the UI status line. A GlobalSignal because it is
/// engine-owned (not per-component) and must initialize outside any dioxus
/// scope — plain `Signal::new` panics outside the runtime.
pub static STATUS: Global<Signal<String>, String> = Signal::global(|| "starting…".to_string());

/// The most recent batch the sinks could not write (Events payloads or a
/// Snapshot). The engine only logs; the app observes this to surface sync
/// failures centrally (overlay/notifications). PartialEq on EngineError
/// lets observers dedupe repeated failures. Leaders relay it to every
/// subordinate tab, so the notice overlay works everywhere.
pub static LAST_ERROR: Global<Signal<Option<EngineError>>, Option<EngineError>> =
    Signal::global(|| None);

// The singleton slot is type-erased (`Rc<dyn Any>`): the engine is
// generic over the app's table enum, and Rust has no generic statics.
// Each app instantiates exactly one `Engine<T>`; `init::<T>` stores it
// erased and `engine::<T>()` downcasts — the closures below (socket
// onmessage, BroadcastChannel) capture `T` through their enclosing
// generic fn, so callbacks resolve the same instantiation.
thread_local! {
    static ENGINE: RefCell<Option<Rc<dyn Any>>> = const { RefCell::new(None) };
}

/// Why a query call failed, surfaced to call sites via `Signal<Result<..>>`.
/// Serializable: apply failures relay to subordinate tabs over
/// BroadcastChannel (the notice overlay must work in every tab).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum EngineError {
    /// The local DB could not be opened or migrated (storage, wasm heap).
    #[error("db init failed: {0}")]
    DbInit(BridgeError),
    /// The statement itself failed at the PGlite/JS boundary.
    #[error("sql failed: {0}")]
    Sql(#[from] BridgeError),
    /// Rows came back but didn't map to the result type.
    #[error("row mapping failed: {0}")]
    Mapping(#[from] RowError),
    /// The app never registered a row sink for this table — a setup bug.
    /// Surfaced instead of faking an empty success: the cursor would
    /// otherwise advance past data the client silently dropped. The table
    /// is its wire name (`SyncTable::as_str`) — errors are T-free so the
    /// LAST_ERROR global signal stays possible (no generic statics).
    #[error("no row sink registered for {0:?}")]
    NoSink(String),
    /// A registered row sink failed to write its batch (payload didn't
    /// parse, or the upsert SQL failed).
    #[error("sink failed: {0}")]
    Sink(String),
}

/// This tab's role in the browser. Exactly one leader holds the Web Lock;
/// subordinates proxy every DB access to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Leader,
    Follower,
}

/// Tab messages over BroadcastChannel (JSON, like the websocket wire).
/// Requests flow subordinate → leader; replies and broadcasts flow back.
///
/// Deliberately T-FREE (#31): the relay rides the tables' WIRE NAMES —
/// the same identity the server logs — and the pushed op as raw JSON.
/// The leader re-attaches the typed table via `T::from_name` when it
/// acts. This keeps the derive's serde bounds out of generic territory
/// (a `T`-generic derive here fights the `DeserializeOwned` bound —
/// E0283) and makes the cross-tab plumbing reusable as-is.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum TabMsg {
    /// Run a read in the leader's PGlite and ship the raw result back.
    Query {
        id: u64,
        sql: String,
        params: Vec<String>,
    },
    /// Run a local write in the leader's PGlite (the only live instance).
    Exec {
        id: u64,
        sql: String,
        params: Vec<String>,
        touched: Vec<(String, Uuid)>,
    },
    /// Deliver an op through the leader's socket/pending queue: the
    /// op's wire JSON, re-parsed into `Op<T>` by the leader.
    Push { op: serde_json::Value },
    /// A newly-subordinate tab asks for the leader's current status.
    Hello,
    /// Query reply: the PGlite result JSON-stringified (the same shape a
    /// direct call would return), or the boundary error that failed it.
    Rows {
        id: u64,
        result: Result<String, BridgeError>,
    },
    /// Exec reply: the write committed (and the bump went out), or failed.
    ExecDone {
        id: u64,
        result: Result<(), BridgeError>,
    },
    /// Invalidation fan-out: local echo. A write anywhere re-runs the
    /// matching live queries in every tab.
    Bump { touched: Vec<(String, Uuid)> },
    /// The leader's connection state, so every tab shows the same thing.
    Status { text: String },
    /// Relay of an apply failure so the notice overlay works everywhere.
    ApplyError { error: EngineError },
}

pub struct Engine<T: SyncTableWire> {
    /// The app's schema migrations, applied to every fresh local DB.
    migrations: &'static [(&'static str, &'static str)],
    /// The socket while a session is open.
    sock: RefCell<Option<web_sys::WebSocket>>,
    /// Server messages parsed by the onmessage callback, awaiting the
    /// master task.
    inbox: RefCell<Vec<ServerMsg<T>>>,
    /// Where we've streamed sync_log to; persisted in the client's meta
    /// table so it survives reloads.
    cursor: Cell<i64>,
    /// Per-table row sinks registered by the app.
    sinks: RefCell<HashMap<T, RowSink<T>>>,
    /// This tab's role; `None` until the election in `run` resolves. All
    /// DB access awaits it — subordinates must never open PGlite.
    role: Cell<Option<Role>>,
    /// The cross-tab transport; `None` only when BroadcastChannel is
    /// unavailable (then every tab self-leads: today's per-tab engines).
    tabs_channel: RefCell<Option<tabs::TabsChannel>>,
    /// Tab requests parsed by the BroadcastChannel callback, awaiting the
    /// leader's servicer task.
    tab_inbox: RefCell<Vec<TabMsg>>,
    /// Replies to this tab's own subordinate requests, keyed by request id.
    tab_replies: RefCell<HashMap<u64, TabMsg>>,
    next_req: Cell<u64>,
    /// Scope-free mirror of STATUS (the Hello reply needs it).
    status_text: RefCell<String>,
}

// Live queries are T-FREE reactive plumbing (`Dep` carries wire names
// since #31, not the table enum), so the registry is a separate global:
// the subscription guard's Drop cannot know `T`, and the guard must
// always reach it.
thread_local! {
    static SUBS: RefCell<Vec<Subscription>> = const { RefCell::new(Vec::new()) };
}

/// Create the engine singleton. Called once from `main`, before launch, so
/// hooks and callbacks can always reach it. Also joins the cross-tab
/// channel: from here on every tab participates in the leader election.
/// `T` is the app's table enum — the ONE instantiation of this library
/// in the app.
pub fn init<T: SyncTableWire>(migrations: &'static [(&'static str, &'static str)]) {
    let singleton: Rc<Engine<T>> = Rc::new(Engine {
        migrations,
        sock: RefCell::new(None),
        inbox: RefCell::new(Vec::new()),
        cursor: Cell::new(-1),
        sinks: RefCell::new(HashMap::new()),
        role: Cell::new(None),
        tabs_channel: RefCell::new(None),
        tab_inbox: RefCell::new(Vec::new()),
        tab_replies: RefCell::new(HashMap::new()),
        next_req: Cell::new(0),
        status_text: RefCell::new("starting…".into()),
    });

    match tabs::TabsChannel::new() {
        Ok(channel) => {
            let on_tab = wasm_bindgen::closure::Closure::new(|e: web_sys::MessageEvent| {
                if let Some(text) = e.data().as_string() {
                    engine::<T>().recv_tab(&text);
                }
            });
            channel.set_on_message(&on_tab);
            on_tab.forget();
            *singleton.tabs_channel.borrow_mut() = Some(channel);
        }
        Err(e) => {
            log(
                "tabs",
                &format!("BroadcastChannel unavailable ({e:?}) — per-tab engines"),
            );
        }
    }

    ENGINE.with_borrow_mut(|slot| *slot = Some(singleton as Rc<dyn Any>));
}

/// App-provided sink for one table's ops: applies that table's events (or
/// a Snapshot's rows, synthesized as upsert ops) into the local DB —
/// including deletes (`data: null`). The app owns the mapping (it names
/// its row types); the engine just sinks ops into it — the
/// dynamic-dispatch boundary of this library.
pub type RowSink<T> = Rc<
    dyn for<'a> Fn(
        &'a Pglite,
        &'a [Op<T>],
    )
        -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Uuid>, EngineError>> + 'a>>,
>;

/// Handle to the engine singleton. `T` must be the SAME type `init` was
/// called with — one `Engine<T>` per app (panics otherwise, which is the
/// desired setup-bug behavior).
pub fn engine<T: SyncTableWire>() -> Rc<Engine<T>> {
    ENGINE.with_borrow(|e| {
        e.clone()
            .and_then(|any| any.downcast::<Engine<T>>().ok())
            .expect("engine initialized in main")
    })
}

impl<T: SyncTableWire> Engine<T> {
    /// Register the app's sink for one table: what the engine calls to
    /// write that table's payload rows into the local DB. The app owns
    /// the mapping (it names its row types); the engine just sinks rows into it.
    pub fn register_sink(&self, table: T, sink: RowSink<T>) {
        self.sinks.borrow_mut().insert(table, sink);
    }

    /// Sink one table's batch through its registered row sink.
    async fn apply_batch(
        &self,
        db: &Pglite,
        table: T,
        ops: &[Op<T>],
    ) -> Result<Vec<Uuid>, EngineError> {
        let sink = self.sinks.borrow().get(&table).cloned();
        match sink {
            Some(f) => (f)(db, ops).await,
            None => Err(EngineError::NoSink(table.as_str().to_string())),
        }
    }
    /// Register a live query; returns an RAII guard whose Drop unsubscribes
    /// (shared-ownership, so any clone of the guard keeps it alive).
    /// Re-registering the same query id refreshes its deps instead of
    /// duplicating. The registry itself is T-free (see `SUBS`).
    pub fn listen(&self, q: Query, rev: Signal<u64>) -> SubscriptionGuard {
        SUBS.with_borrow_mut(|subs| match subs.iter_mut().find(|s| s.id == q.id) {
            Some(existing) => {
                existing.deps = q.deps;
                existing.rev = rev;
            }
            None => {
                subs.push(Subscription {
                    id: q.id,
                    deps: q.deps,
                    rev,
                });
            }
        });

        SubscriptionGuard::new(q.id)
    }

    /// Remove a subscription (component unmounted).
    pub fn unsubscribe(&self, id: SubscriptionId) {
        unsubscribe(id);
    }

    /// One-shot local read, mapped to `T` via its FromRow impl. Works
    /// regardless of connection state — reads never wait on the network.
    /// Failures are typed so call sites can render them (see use_query).
    pub async fn query<R: FromRow>(&self, q: &Query) -> Result<Vec<R>, EngineError> {
        match self.wait_role().await {
            Role::Leader => self.query_local::<R>(q).await,
            Role::Follower => self.query_remote::<R>(q).await,
        }
    }

    async fn query_local<R: FromRow>(&self, q: &Query) -> Result<Vec<R>, EngineError> {
        let db = Pglite::init(self.migrations)
            .await
            .map_err(EngineError::DbInit)?;

        let result = db
            .query(&q.sql, &q.params)
            .await
            .map_err(EngineError::from)?;

        pglite::rows_of(&result)
            .iter()
            .map(|row| R::from_row(row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(EngineError::from)
    }

    /// A subordinate's read: the leader runs it in THE PGlite instance and
    /// ships the raw result back as JSON. The leader can die mid-request
    /// (tab close → re-election in flight); requests are read-only, so
    /// re-asking is safe and the fresh leader answers.
    async fn query_remote<R: FromRow>(&self, q: &Query) -> Result<Vec<R>, EngineError> {
        loop {
            let id = self.next_req.get();
            self.next_req.set(id + 1);
            self.post(&TabMsg::Query {
                id,
                sql: q.sql.clone(),
                params: q.params.clone(),
            });
            if let Some(TabMsg::Rows { result, .. }) = self.await_tab(id).await {
                return match result {
                    Ok(json) => {
                        let parsed: JsValue = js_sys::JSON::parse(&json)
                            .map_err(|e| EngineError::Sql(BridgeError::from_rejection(&e)))?;
                        pglite::rows_of(&parsed)
                            .iter()
                            .map(|row| R::from_row(row))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(EngineError::from)
                    }
                    Err(e) => Err(EngineError::Sql(e)),
                };
            }
            log("tabs", "query timed out — leader died? asking again");
        }
    }

    /// Run a local write, then invalidate queries depending on `touched` —
    /// the same path remote events take, so offline writes light up the UI
    /// identically. The write is the caller's to check: a failed local
    /// write is returned, not swallowed (the op can still be pushed — the
    /// server is the source of truth and will sync it back).
    pub async fn exec(
        &self,
        sql: &str,
        params: &[String],
        touched: &[(T, Uuid)],
    ) -> Result<(), EngineError> {
        match self.wait_role().await {
            Role::Leader => self.exec_local(sql, params, touched).await,
            Role::Follower => self.exec_remote(sql, params, touched).await,
        }
    }

    async fn exec_local(
        &self,
        sql: &str,
        params: &[String],
        touched: &[(T, Uuid)],
    ) -> Result<(), EngineError> {
        let db = Pglite::init(self.migrations)
            .await
            .map_err(EngineError::DbInit)?;
        db.query(sql, params).await.map_err(EngineError::from)?;
        self.bump(touched);
        Ok(())
    }

    /// A subordinate's write: forwarded to the leader, which owns the only
    /// live PGlite instance (two instances over one IndexedDB lose writes
    /// — impl/multi-tab.md). Retried on leader death: a write that lost
    /// its leader either never ran (retry is required) or committed to
    /// IndexedDB before the tab died (the retry surfaces a duplicate-key
    /// error — harmless, LWW syncs the row back from the server).
    async fn exec_remote(
        &self,
        sql: &str,
        params: &[String],
        touched: &[(T, Uuid)],
    ) -> Result<(), EngineError> {
        loop {
            let id = self.next_req.get();
            self.next_req.set(id + 1);
            self.post(&TabMsg::Exec {
                id,
                sql: sql.into(),
                params: params.to_vec(),
                // The relay rides wire names; the leader re-attaches the
                // typed table via `from_name`.
                touched: touched
                    .iter()
                    .map(|(t, r)| (t.as_str().to_string(), *r))
                    .collect(),
            });
            if let Some(TabMsg::ExecDone { result, .. }) = self.await_tab(id).await {
                return result.map_err(EngineError::Sql);
            }
            log("tabs", "write timed out — leader died? asking again");
        }
    }

    /// Deliver an operation: persist it in the durable op log FIRST
    /// (write-ahead, with a client-generated batch id), then send it if
    /// the socket is open. The pending row is retired ONLY by the
    /// server's Ack (handle_msg) — never by an accepted send: a socket
    /// can accept a frame and die before the server reads it, and an op
    /// deleted on send-acceptance is lost cross-client forever (#33).
    /// The resend on reconnect is a no-op under LWW. Subordinates hand
    /// the op to the leader, whose log it becomes.
    pub async fn push(&self, op: Op<T>) {
        match self.wait_role().await {
            Role::Leader => self.push_local(op).await,
            Role::Follower => {
                // The op crosses as wire JSON; the leader re-parses it
                // into its own typed shape. A serialization failure here
                // would lose the write, so it is surfaced, not dropped.
                match serde_json::to_value(&op) {
                    Ok(op) => self.post(&TabMsg::Push { op }),
                    Err(e) => {
                        log("sync", &format!("op serialization failed: {e}"));
                        self.set_last_error(EngineError::Sink(format!(
                            "offline write could not be relayed: {e}"
                        )));
                    }
                }
            }
        }
    }

    async fn push_local(&self, op: Op<T>) {
        // The op log must be reachable for EVERY push now (write-ahead):
        // its row is what makes the send survivable.
        let db = match Pglite::init(self.migrations).await {
            Ok(db) => db,
            Err(e) => {
                log("sync", &format!("op log unavailable: {e}"));
                self.set_last_error(EngineError::Sink(format!(
                    "write could not be persisted: {e}"
                )));
                return;
            }
        };
        let json = match serde_json::to_string(&op) {
            Ok(json) => json,
            Err(e) => {
                log("sync", &format!("op serialization failed: {e}"));
                self.set_last_error(EngineError::Sink(format!(
                    "write could not be serialized: {e}"
                )));
                return;
            }
        };
        // One batch id per push: the invariant that lets an Ack retire
        // exactly the row(s) it confirms (flush sends one row per
        // message under its own batch id). v7: time-ordered, so a
        // batch's log position roughly tracks its creation.
        let batch = Uuid::now_v7();
        if let Err(e) = db
            .query(
                &format!("INSERT INTO {SCHEMA}.pending_ops (op, batch_id) VALUES ($1, $2)"),
                &[json, batch.to_string()],
            )
            .await
        {
            log("sync", &format!("op log write failed: {e}"));
            self.set_last_error(EngineError::Sql(e));
            return;
        }
        let open = self
            .sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
        if open {
            self.send(&ClientMsg::Push {
                ops: vec![op],
                batch,
            });
        }
        // Not open: the row waits for the connect flush. Either way the
        // Ack — not this send — decides when the row is retired.
    }

    /// Entry point for the socket's onmessage callback. Runs outside any
    /// dioxus scope, so it must never spawn: it parses and enqueues only.
    pub fn recv_text(&self, text: &str) {
        match serde_json::from_str::<ServerMsg<T>>(text) {
            Ok(msg) => self.inbox.borrow_mut().push(msg),
            Err(e) => log("sync", &format!("bad message: {e}")),
        }
    }

    /// Entry point for the BroadcastChannel callback. Same discipline as
    /// `recv_text`: parse and either enqueue (requests for the leader's
    /// servicer) or apply synchronously (replies and broadcasts — bump,
    /// status, errors are all synchronous work).
    fn recv_tab(&self, text: &str) {
        let msg = match serde_json::from_str::<TabMsg>(text) {
            Ok(msg) => msg,
            Err(e) => {
                log("tabs", &format!("bad tab message: {e}"));
                return;
            }
        };
        match &msg {
            // Requests: the leader's servicer drains these.
            TabMsg::Query { .. } | TabMsg::Exec { .. } | TabMsg::Push { .. } | TabMsg::Hello => {
                if self.role.get() == Some(Role::Leader) {
                    self.tab_inbox.borrow_mut().push(msg);
                } else {
                    log("tabs", "request arrived while not leading — dropped");
                }
            }
            // Replies to this tab's own subordinate requests.
            TabMsg::Rows { id, .. } | TabMsg::ExecDone { id, .. } => {
                self.tab_replies.borrow_mut().insert(*id, msg);
            }
            // Broadcasts, applied inline.
            TabMsg::Bump { touched } => {
                let touched = touched_from_names::<T>(touched.clone());
                self.bump(&touched);
            }
            TabMsg::Status { text } => self.set_status_relayed(text),
            TabMsg::ApplyError { error } => {
                *LAST_ERROR.write_unchecked() = Some(error.clone());
            }
        }
    }

    // ---- transparency (open questions #3: debug handle over privacy) ----

    /// One-line snapshot of engine state for debugging in the console.
    pub fn debug(&self) -> String {
        format!(
            "role={:?} cursor={} live_queries={}",
            self.role.get(),
            self.cursor.get(),
            SUBS.with_borrow(|s| s.len())
        )
    }

    // ---- internals ----

    /// Block until the election in `run` has decided this tab's role.
    async fn wait_role(&self) -> Role {
        loop {
            if let Some(role) = self.role.get() {
                return role;
            }
            timer_pause(25).await;
        }
    }

    /// Post a message to the browser's other tabs (no-op without a
    /// channel — the degraded per-tab-engine mode).
    fn post(&self, msg: &TabMsg) {
        if let Ok(text) = serde_json::to_string(msg)
            && let Some(channel) = self.tabs_channel.borrow().as_ref()
        {
            channel.post(&text);
        }
    }

    /// Await the reply to one of this tab's subordinate requests. `None`
    /// means the leader died (or stalled) — callers retry.
    async fn await_tab(&self, id: u64) -> Option<TabMsg> {
        let mut waited = 0u32;
        loop {
            if let Some(reply) = self.tab_replies.borrow_mut().remove(&id) {
                return Some(reply);
            }
            if waited >= 5000 {
                return None;
            }
            timer_pause(20).await;
            waited += 20;
        }
    }

    fn set_status(&self, text: &str) {
        *STATUS.write_unchecked() = text.into();
        *self.status_text.borrow_mut() = text.to_string();
        // The leader is the authority: fan the state out to every tab.
        self.post(&TabMsg::Status {
            text: text.to_string(),
        });
    }

    /// Subordinates render the leader's state behind a role marker: sync
    /// is delegated to another tab, and tests can tell the roles apart.
    fn set_status_relayed(&self, text: &str) {
        let shown = format!("subordinate — {text}");
        *STATUS.write_unchecked() = shown.clone();
        *self.status_text.borrow_mut() = shown;
    }

    fn set_last_error(&self, error: EngineError) {
        *LAST_ERROR.write_unchecked() = Some(error.clone());
        if self.role.get() == Some(Role::Leader) {
            self.post(&TabMsg::ApplyError { error });
        }
    }
    fn send(&self, msg: &ClientMsg<T>) {
        let _ = self.send_ok(msg);
    }

    /// Serialize and hand to the socket buffer. `Ok` means the frame was
    /// ACCEPTED (buffered), not delivered — see `flush_pending` for the
    /// durability consequence.
    fn send_ok(&self, msg: &ClientMsg<T>) -> bool {
        let Ok(text) = serde_json::to_string(msg) else {
            return false;
        };
        self.sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.send_with_str(&text).is_ok())
    }

    /// Drain the durable op log on connect: resend every pending batch.
    /// Rows are NOT deleted here — retirement is the Ack's job
    /// (`handle_msg`): a send the socket accepted tells us nothing about
    /// whether the server read it. An unacked batch simply resends on
    /// every connect until acknowledged; LWW makes the replay a no-op.
    /// One row per message under its own batch id (the persist-first
    /// invariant: a batch is exactly one push_local call).
    async fn flush_pending(&self, db: &Pglite) {
        let rows = match db
            .query(
                &format!(
                    "SELECT seq::text, op, COALESCE(batch_id::text, '') AS batch_id \
                     FROM {SCHEMA}.pending_ops ORDER BY seq"
                ),
                &[],
            )
            .await
        {
            Ok(result) => pglite::rows_of(&result),
            Err(e) => {
                log("sync", &format!("op log read failed: {e}"));
                return;
            }
        };
        for row in &rows {
            // A row the mapper can't read must not wedge the flush: log
            // the reason (now typed — missing vs null vs wrong shape) and
            // keep it for the next connect.
            let seq = match row.str_field_req("seq") {
                Ok(seq) => seq,
                Err(e) => {
                    log("sync", &format!("op log row unreadable — keeping it: {e}"));
                    continue;
                }
            };
            let op: Op<T> = match row
                .str_field_req("op")
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
            {
                Some(op) => op,
                None => {
                    log("sync", &format!("op log row {seq} unparsable — keeping it"));
                    continue;
                }
            };
            // batch_id is nullable in the schema (ALTER-added), so the
            // null-aware read: None lands in the keep-it branch too.
            let batch = match row
                .str_field("batch_id")
                .ok()
                .flatten()
                .and_then(|b| b.parse().ok())
            {
                Some(batch) => batch,
                None => {
                    // Unreachable since migration 0009 backfills ids —
                    // a row without one must not fly under a fabricated
                    // id that could collide with a live batch's ack.
                    log(
                        "sync",
                        &format!("op log row {seq} has no batch id — keeping it"),
                    );
                    continue;
                }
            };
            // A refused send keeps its row; the next connect retries.
            self.send(&ClientMsg::Push {
                ops: vec![op],
                batch,
            });
        }
    }

    /// Fire the rev signal of every registered query whose deps match one
    /// of the touched (table, row) pairs. Owners re-run their queries. As
    /// the leader, this is also the local-echo fan-out: subordinates get
    /// the same bump over BroadcastChannel and re-run their queries there.
    fn bump(&self, touched: &[(T, Uuid)]) {
        SUBS.with_borrow(|subs| {
            for sub in subs.iter() {
                if sub
                    .deps
                    .iter()
                    .any(|d| touched.iter().any(|(t, r)| d.matches(t.as_str(), *r)))
                {
                    *sub.rev.write_unchecked() += 1;
                }
            }
        });
        if self.role.get() == Some(Role::Leader) && !touched.is_empty() {
            self.post(&TabMsg::Bump {
                touched: touched
                    .iter()
                    .map(|(t, r)| (t.as_str().to_string(), *r))
                    .collect(),
            });
        }
    }

    /// The single long-lived task: claim the browser's engine slot, then
    /// either lead (socket + DB) or follow until the leader dies.
    pub async fn run(&self) {
        if self.tabs_channel.borrow().is_none() {
            // No BroadcastChannel: tab coordination is impossible. Every
            // tab self-leads — today's per-tab engines, clobbers and all.
            self.run_leader().await;
            return;
        }
        match tabs::try_acquire().await {
            Some(_hold) => self.run_leader().await,
            None => {
                self.role.set(Some(Role::Follower));
                self.set_status_relayed("the leader tab owns sync…");
                self.post(&TabMsg::Hello);
                log("tabs", "subordinate tab — following the leader");
                // Queued with the browser from this moment; resolves when
                // the leader's document dies and the lock comes to us.
                tabs::wait_acquire().await;
                log("tabs", "promoted to leader — the previous one died");
                self.run_leader().await
            }
        }
    }

    /// The leader's lifetime: open THE PGlite instance, serve subordinate
    /// tab requests, and run the connect/pull/push loop until the tab dies.
    async fn run_leader(&self) {
        self.role.set(Some(Role::Leader));
        let db = match Pglite::init(self.migrations).await {
            Ok(p) => p,
            Err(e) => {
                self.set_status(&format!("pglite failed: {e}"));
                return;
            }
        };
        log("boot", "pglite ready — this tab is the engine's leader");
        self.cursor.set(load_cursor(&db).await);
        self.set_status("offline — local data loaded");
        log("boot", &self.debug());

        // One dedicated task serves every subordinate's query/write/push
        // for the lifetime of the leadership (it re-joins the PGlite
        // singleton, so it never races `run` for initialization).
        let servicer = engine::<T>();
        wasm_bindgen_futures::spawn_local(async move {
            servicer.serve_tabs().await;
        });

        loop {
            self.set_status("connecting…");
            match open_socket::<T>(&sync_url()) {
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

    /// Serve subordinate tab requests: every DB access in the browser
    /// funnels through the leader's PGlite instance.
    async fn serve_tabs(&self) {
        loop {
            let msgs = std::mem::take(&mut *self.tab_inbox.borrow_mut());
            for msg in msgs {
                self.serve_tab(msg).await;
            }
            timer_pause(50).await;
        }
    }

    async fn serve_tab(&self, msg: TabMsg) {
        let db = match Pglite::init(self.migrations).await {
            Ok(db) => db,
            Err(e) => {
                log("tabs", &format!("db unavailable for tab request: {e}"));
                return;
            }
        };
        match msg {
            // A subordinate just joined: tell it where sync stands (it has
            // no other way to learn a status the leader set before it
            // existed).
            TabMsg::Hello => self.post(&TabMsg::Status {
                text: self.status_text.borrow().clone(),
            }),
            TabMsg::Query { id, sql, params } => {
                let result = match db.query(&sql, &params).await {
                    Ok(value) => match js_sys::JSON::stringify(&value) {
                        Ok(s) => Ok(JsValue::from(s).as_string().unwrap_or_default()),
                        Err(e) => Err(BridgeError::from_rejection(&e)),
                    },
                    Err(e) => Err(e),
                };
                self.post(&TabMsg::Rows { id, result });
            }
            TabMsg::Exec {
                id,
                sql,
                params,
                touched,
            } => {
                let touched = touched_from_names::<T>(touched);
                let result = match db.query(&sql, &params).await {
                    Ok(_) => {
                        self.bump(&touched);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                self.post(&TabMsg::ExecDone { id, result });
            }
            TabMsg::Push { op } => match serde_json::from_value::<Op<T>>(op) {
                Ok(op) => self.push_local(op).await,
                Err(e) => log("tabs", &format!("relayed op unparsable: {e}")),
            },
            // Replies and broadcasts are addressed to subordinates.
            _ => log("tabs", "reply/broadcast arrived at the leader — dropped"),
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
                        self.flush_pending(db).await;
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

    async fn handle_msg(&self, db: &Pglite, msg: ServerMsg<T>) {
        match msg {
            ServerMsg::Ack { batch, .. } => {
                // The server confirmed this batch: retire its pending
                // rows. A failed delete is logged — the next flush
                // resends the batch (LWW no-op) and acks again.
                if let Err(e) = db
                    .query(
                        &format!("DELETE FROM {SCHEMA}.pending_ops WHERE batch_id = $1"),
                        &[batch.to_string()],
                    )
                    .await
                {
                    log("sync", &format!("ack retirement failed: {e}"));
                }
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
                // Every event already IS the invalidation notice — its
                // (table, row_id) rides along with the payload, so phase 1
                // needs no extra protocol messages (phase 2 can send a
                // payload-less Invalidate instead).
                let mut unique: Vec<T> = Vec::new();
                for op in &events {
                    if !unique.contains(&op.table) {
                        unique.push(op.table);
                    }
                }
                let mut touched = Vec::with_capacity(events.len());
                let mut failed: Option<EngineError> = None;
                for table in unique {
                    let ops: Vec<Op<T>> = events
                        .iter()
                        .filter(|o| o.table == table)
                        .cloned()
                        .collect();
                    match self.apply_batch(db, table, &ops).await {
                        Ok(ids) => touched.extend(ids.into_iter().map(|id| (table, id))),
                        Err(e) => {
                            log("sync", &format!("apply failed: {e}"));
                            failed = Some(e);
                        }
                    }
                }
                // One bump per batch, not per op: replaying a backlog must
                // re-run each query once, not once per row (the UI would
                // visibly re-render row by row).
                self.bump(&touched);
                if let Some(e) = failed {
                    // Apply failed: the cursor must NOT advance past data
                    // the client never wrote (the old order — save, then
                    // apply — permanently skipped anything a crash or a
                    // failing chunk interrupted). The batch stays pending:
                    // the next ack or reconnect re-pulls it (at-least-once,
                    // LWW-idempotent), and LAST_ERROR keeps the stuck batch
                    // visible instead of silently skipped.
                    self.set_last_error(e);
                    return;
                }
                self.cursor.set(c);
                save_cursor(db, c).await;
                // The batch is a window into the backlog: keep pulling
                // until the server returns an empty one. (At-least-once;
                // LWW makes replays idempotent.)
                if !events.is_empty() {
                    self.send(&ClientMsg::Pull { since: c });
                }
            }
            ServerMsg::Snapshot {
                seq,
                tables,
                tombstones,
            } => {
                let rows_count: usize = tables.iter().map(|t| t.rows.len()).sum();
                log(
                    "sync",
                    &format!(
                        "snapshot at {seq}: {rows_count} rows + {} tombstones",
                        tombstones.len()
                    ),
                );
                // Tombstones go first: the guarded upserts below consult
                // them, and a pending offline delete must outlive a
                // snapshot that predates it (upserts never regress).
                if let Err(e) = crate::query::apply_tombstones(db, &tombstones).await {
                    log("sync", &format!("snapshot tombstones apply failed: {e}"));
                }
                let mut touched = Vec::with_capacity(rows_count);
                for table_data in &tables {
                    // Snapshot rows are payload-shaped live rows: upserts
                    // by construction (deleted rows are excluded
                    // server-side). `id`/`updated_at` are read from the
                    // payload by the sink's row parsing.
                    let ops: Vec<Op<T>> = table_data
                        .rows
                        .iter()
                        .map(|row| Op {
                            table: table_data.table,
                            id: Uuid::nil(),
                            data: row.clone(),
                            // Snapshot rows re-enter as upserts. For
                            // upserts the applier never reads the
                            // envelope timestamp — the payload's own
                            // updated_at binds (and serde-validated on
                            // the way in) — so this epoch placeholder is
                            // inert; a parse here just keeps the type
                            // honest instead of smuggling a raw String.
                            updated_at: Timestamp::parse("1970-01-01T00:00:00.000Z").unwrap(),
                        })
                        .collect();
                    match self.apply_batch(db, table_data.table, &ops).await {
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

/// Publish an apply failure from outside the `Engine` impl (the generic
/// batch sink skips poison rows and surfaces them here). Relayed to
/// subordinate tabs exactly like engine-internal failures.
pub(crate) fn publish_last_error<T: SyncTableWire>(error: EngineError) {
    if ENGINE.with_borrow(|e| e.is_some()) {
        engine::<T>().set_last_error(error);
    } else {
        *LAST_ERROR.write_unchecked() = Some(error);
    }
}

/// Console logging for debugging in Chrome devtools.
pub fn log(kind: &str, msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("[{kind}] {msg}")));
}

/// Re-attach typed tables to relayed touched pairs: the tab wire rides
/// wire names (`SyncTable::as_str`), the engine's internals are typed.
/// Unknown names (from a newer app version) are skipped — the compat
/// boundary, same rule as server replay.
fn touched_from_names<T: SyncTableWire>(touched: Vec<(String, Uuid)>) -> Vec<(T, Uuid)> {
    touched
        .into_iter()
        .filter_map(|(name, id)| Some((T::from_name(&name)?, id)))
        .collect()
}

/// Remove a subscription without going through the engine — the RAII
/// guard's Drop cannot know the engine's table type (`T`).
pub(crate) fn unsubscribe(id: SubscriptionId) {
    SUBS.with_borrow_mut(|s| s.retain(|s| s.id != id));
}

/// Open the sync websocket; parsed messages are enqueued via
/// [`Engine::recv_text`] and everything else is driven by `run`.
fn open_socket<T: SyncTableWire>(url: &str) -> Result<WebSocket, JsValue> {
    let ws = WebSocket::new(url)?;

    let onmessage =
        wasm_bindgen::closure::Closure::wrap(Box::new(move |e: web_sys::MessageEvent| {
            if let Some(text) = e.data().as_string() {
                engine::<T>().recv_text(&text);
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
        .query(
            &format!("SELECT value FROM {SCHEMA}.meta WHERE key = 'cursor'"),
            &[],
        )
        .await
        .ok()
        .and_then(|r| {
            pglite::rows_of(&r)
                .first()
                .and_then(|row| row.str_field_req("value").ok()?.parse().ok())
        })
        .unwrap_or(-1)
}

async fn save_cursor(pglite: &Pglite, cursor: i64) {
    let _ = pglite
        .query(
            &format!(
                "INSERT INTO {SCHEMA}.meta (key, value) VALUES ('cursor', $1) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
            ),
            &[cursor.to_string()],
        )
        .await;
}

/// Sleep in the browser without tokio (wasm has no time driver).
pub async fn timer_pause(ms: u32) {
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
