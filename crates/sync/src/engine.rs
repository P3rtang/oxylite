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

use crate::pglite::{self, BridgeError, Pglite};
use crate::query::{FromRow, Query, RowError, Subscription, SubscriptionGuard, SubscriptionId};
use crate::tabs;
use dioxus::prelude::{Global, Signal, WritableExt};
use shared::{ClientMsg, Op, ServerMsg, Table, timestamp::Timestamp};

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

thread_local! {
    static ENGINE: RefCell<Option<Rc<Engine>>> = const { RefCell::new(None) };
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
    /// otherwise advance past data the client silently dropped.
    #[error("no row sink registered for {0:?}")]
    NoSink(Table),
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
        touched: Vec<(Table, Uuid)>,
    },
    /// Deliver an op through the leader's socket/pending queue.
    Push { op: Op },
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
    Bump { touched: Vec<(Table, Uuid)> },
    /// The leader's connection state, so every tab shows the same thing.
    Status { text: String },
    /// Relay of an apply failure so the notice overlay works everywhere.
    ApplyError { error: EngineError },
}

pub struct Engine {
    /// The app's schema migrations, applied to every fresh local DB.
    migrations: &'static [(&'static str, &'static str)],
    /// Live queries: what the connection keeps in sync.
    subs: RefCell<Vec<Subscription>>,
    /// The socket while a session is open.
    sock: RefCell<Option<web_sys::WebSocket>>,
    /// Server messages parsed by the onmessage callback, awaiting the
    /// master task.
    inbox: RefCell<Vec<ServerMsg>>,
    /// Where we've streamed sync_log to; persisted in the client's meta
    /// table so it survives reloads.
    cursor: Cell<i64>,
    /// Per-table row sinks registered by the app.
    sinks: RefCell<HashMap<Table, RowSink>>,
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

/// Create the engine singleton. Called once from `main`, before launch, so
/// hooks and callbacks can always reach it. Also joins the cross-tab
/// channel: from here on every tab participates in the leader election.
pub fn init(migrations: &'static [(&'static str, &'static str)]) {
    let singleton = Rc::new(Engine {
        migrations,
        subs: RefCell::new(Vec::new()),
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
                    engine().recv_tab(&text);
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

    ENGINE.with_borrow_mut(|slot| *slot = Some(singleton));
}

/// App-provided sink for one table's ops: applies that table's events (or
/// a Snapshot's rows, synthesized as upsert ops) into the local DB —
/// including deletes (`data: null`). The app owns the mapping (it names
/// its row types); the engine just sinks ops into it — the
/// dynamic-dispatch boundary of this library.
pub type RowSink = Rc<
    dyn for<'a> Fn(
        &'a Pglite,
        &'a [Op],
    )
        -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Uuid>, EngineError>> + 'a>>,
>;

/// Handle to the engine singleton.
pub fn engine() -> Rc<Engine> {
    ENGINE.with_borrow(|e| e.clone().expect("engine initialized in main"))
}

impl Engine {
    /// Register the app's sink for one table: what the engine calls to
    /// write that table's payload rows into the local DB. The app owns
    /// the mapping (it names its row types); the engine just sinks rows into it.
    pub fn register_sink(&self, table: Table, sink: RowSink) {
        self.sinks.borrow_mut().insert(table, sink);
    }

    /// Sink one table's batch through its registered row sink.
    async fn apply_batch(
        &self,
        db: &Pglite,
        table: Table,
        ops: &[Op],
    ) -> Result<Vec<Uuid>, EngineError> {
        let sink = self.sinks.borrow().get(&table).cloned();
        match sink {
            Some(f) => (f)(db, ops).await,
            None => Err(EngineError::NoSink(table)),
        }
    }
    /// Register a live query; returns an RAII guard whose Drop unsubscribes
    /// (shared-ownership, so any clone of the guard keeps it alive).
    /// Re-registering the same query id refreshes its deps instead of
    /// duplicating.
    pub fn listen(&self, q: Query, rev: Signal<u64>) -> SubscriptionGuard {
        let mut subs = self.subs.borrow_mut();

        match subs.iter_mut().find(|s| s.id == q.id) {
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
        }

        SubscriptionGuard::new(q.id)
    }

    /// Remove a subscription (component unmounted).
    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.subs.borrow_mut().retain(|s| s.id != id);
    }

    /// One-shot local read, mapped to `T` via its FromRow impl. Works
    /// regardless of connection state — reads never wait on the network.
    /// Failures are typed so call sites can render them (see use_query).
    pub async fn query<T: FromRow>(&self, q: &Query) -> Result<Vec<T>, EngineError> {
        match self.wait_role().await {
            Role::Leader => self.query_local(q).await,
            Role::Follower => self.query_remote(q).await,
        }
    }

    async fn query_local<T: FromRow>(&self, q: &Query) -> Result<Vec<T>, EngineError> {
        let db = Pglite::init(self.migrations)
            .await
            .map_err(EngineError::DbInit)?;

        let result = db
            .query(&q.sql, &q.params)
            .await
            .map_err(EngineError::from)?;

        pglite::rows_of(&result)
            .iter()
            .map(|row| T::from_row(row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(EngineError::from)
    }

    /// A subordinate's read: the leader runs it in THE PGlite instance and
    /// ships the raw result back as JSON. The leader can die mid-request
    /// (tab close → re-election in flight); requests are read-only, so
    /// re-asking is safe and the fresh leader answers.
    async fn query_remote<T: FromRow>(&self, q: &Query) -> Result<Vec<T>, EngineError> {
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
                            .map(|row| T::from_row(row))
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
        touched: &[(Table, Uuid)],
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
        touched: &[(Table, Uuid)],
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
        touched: &[(Table, Uuid)],
    ) -> Result<(), EngineError> {
        loop {
            let id = self.next_req.get();
            self.next_req.set(id + 1);
            self.post(&TabMsg::Exec {
                id,
                sql: sql.into(),
                params: params.to_vec(),
                touched: touched.to_vec(),
            });
            if let Some(TabMsg::ExecDone { result, .. }) = self.await_tab(id).await {
                return result.map_err(EngineError::Sql);
            }
            log("tabs", "write timed out — leader died? asking again");
        }
    }

    /// Deliver an operation: send it if the socket is OPEN, else persist
    /// it in the durable op log (pending_ops) for the connect flush.
    /// At-least-once; LWW makes retries harmless. Subordinates hand the
    /// op to the leader, whose log it becomes.
    pub async fn push(&self, op: Op) {
        match self.wait_role().await {
            Role::Leader => self.push_local(op).await,
            Role::Follower => self.post(&TabMsg::Push { op }),
        }
    }

    async fn push_local(&self, op: Op) {
        let open = self
            .sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
        if open {
            self.send(&ClientMsg::Push { ops: vec![op] });
            return;
        }
        // Offline: the op must survive this tab (and this engine) dying.
        // Persist before anything else can forget it; a failed write is
        // surfaced — an op that vanishes here is lost cross-client.
        let db = match Pglite::init(self.migrations).await {
            Ok(db) => db,
            Err(e) => {
                log("sync", &format!("op log unavailable: {e}"));
                self.set_last_error(EngineError::Sink(format!(
                    "offline write could not be persisted: {e}"
                )));
                return;
            }
        };
        let json = match serde_json::to_string(&op) {
            Ok(json) => json,
            Err(e) => {
                log("sync", &format!("op serialization failed: {e}"));
                return;
            }
        };
        if let Err(e) = db
            .query("INSERT INTO pending_ops (op) VALUES ($1)", &[json])
            .await
        {
            log("sync", &format!("op log write failed: {e}"));
            self.set_last_error(EngineError::Sql(e));
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
            TabMsg::Bump { touched } => self.bump(touched),
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
            self.subs.borrow().len()
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
    fn send(&self, msg: &ClientMsg) {
        let _ = self.send_ok(msg);
    }

    /// Serialize and hand to the socket buffer. `Ok` means the frame was
    /// ACCEPTED (buffered), not delivered — see `flush_pending` for the
    /// durability consequence.
    fn send_ok(&self, msg: &ClientMsg) -> bool {
        let Ok(text) = serde_json::to_string(msg) else {
            return false;
        };
        self.sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.send_with_str(&text).is_ok())
    }

    /// Drain the durable op log on connect: send what's queued, then
    /// delete the rows whose send the socket accepted. A refused send
    /// (socket died mid-flush) keeps its row — the next connect retries
    /// it. At-least-once: the server may see a row twice across attempts,
    /// which LWW resolves. The window between an accepted send and the
    /// server writing it is the same exposure the old in-memory queue
    /// had (mem::take then fire-and-forget); ack-based deletion is the
    /// stronger follow-up.
    async fn flush_pending(&self, db: &Pglite) {
        let rows = match db
            .query("SELECT seq::text, op FROM pending_ops ORDER BY seq", &[])
            .await
        {
            Ok(result) => pglite::rows_of(&result),
            Err(e) => {
                log("sync", &format!("op log read failed: {e}"));
                return;
            }
        };
        let mut accepted: Vec<String> = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(seq) = pglite::str_field(row, "seq") else {
                continue;
            };
            let op: Op = match pglite::str_field(row, "op")
                .and_then(|json| serde_json::from_str(&json).ok())
            {
                Some(op) => op,
                None => {
                    log("sync", "op log row unparsable — keeping it");
                    continue;
                }
            };
            if self.send_ok(&ClientMsg::Push { ops: vec![op] }) {
                accepted.push(seq);
            }
        }
        if accepted.is_empty() {
            return;
        }
        let slots: Vec<String> = (1..=accepted.len()).map(|i| format!("${i}")).collect();
        if let Err(e) = db
            .query(
                &format!(
                    "DELETE FROM pending_ops WHERE seq IN ({})",
                    slots.join(", ")
                ),
                &accepted,
            )
            .await
        {
            // The sends were accepted but the delete failed: the next
            // connect replays them — harmless under LWW.
            log("sync", &format!("op log cleanup failed: {e}"));
        }
    }

    /// Fire the rev signal of every registered query whose deps match one
    /// of the touched (table, row) pairs. Owners re-run their queries. As
    /// the leader, this is also the local-echo fan-out: subordinates get
    /// the same bump over BroadcastChannel and re-run their queries there.
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
        if self.role.get() == Some(Role::Leader) && !touched.is_empty() {
            self.post(&TabMsg::Bump {
                touched: touched.to_vec(),
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
        let servicer = engine();
        wasm_bindgen_futures::spawn_local(async move {
            servicer.serve_tabs().await;
        });

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
                let result = match db.query(&sql, &params).await {
                    Ok(_) => {
                        self.bump(&touched);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                self.post(&TabMsg::ExecDone { id, result });
            }
            TabMsg::Push { op } => self.push_local(op).await,
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
                    let ops: Vec<Op> = events
                        .iter()
                        .filter(|o| o.table == table)
                        .cloned()
                        .collect();
                    match self.apply_batch(db, table, &ops).await {
                        Ok(ids) => touched.extend(ids.into_iter().map(|id| (table, id))),
                        Err(e) => {
                            log("sync", &format!("apply failed: {e}"));
                            self.set_last_error(e);
                        }
                    }
                }
                // One bump per batch, not per op: replaying a backlog must
                // re-run each query once, not once per row (the UI would
                // visibly re-render row by row).
                self.bump(&touched);
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
                    let ops: Vec<Op> = table_data
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
pub(crate) fn publish_last_error(error: EngineError) {
    if ENGINE.with_borrow(|e| e.is_some()) {
        engine().set_last_error(error);
    } else {
        *LAST_ERROR.write_unchecked() = Some(error);
    }
}

/// Console logging for debugging in Chrome devtools.
pub fn log(kind: &str, msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("[{kind}] {msg}")));
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
