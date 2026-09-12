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
//!
//! Layout: this module is the singleton and the app-facing API;
//! [`error`] the error type; [`relay`] the cross-tab layer (election
//! roles, BroadcastChannel protocol, serving and proxying); [`sync`]
//! the connection lifecycle (run/session/handle_msg, the durable op
//! log, the socket itself); [`tabs`] the browser transport the relay
//! rides (Web Locks election, BroadcastChannel).

mod error;
mod relay;
mod sync;
mod tabs;

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;

use dioxus::prelude::{Global, Signal, WritableExt};
use uuid::Uuid;
use wasm_bindgen::{JsCast, JsValue};

use crate::contract::from_row::FromRow;
use crate::contract::table::SyncTableWire;
use crate::pglite::{self, Pglite};
use crate::protocol::{Op, SchemaVersion, ServerMsg};
use crate::query::{Query, Subscription, SubscriptionGuard, SubscriptionId};

use relay::{Role, TabMsg};

pub use error::EngineError;

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

// Live queries are T-FREE reactive plumbing (`Dep` carries wire names
// since #31, not the table enum), so the registry is a separate global:
// the subscription guard's Drop cannot know `T`, and the guard must
// always reach it.
thread_local! {
    static SUBS: RefCell<Vec<Subscription>> = const { RefCell::new(Vec::new()) };
}

pub struct Engine<T: SyncTableWire> {
    /// The app's schema migrations, applied to every fresh local DB.
    migrations: &'static [(&'static str, &'static str)],
    /// The app's schema version (the app bakes it; the lib compares).
    version: SchemaVersion,
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

/// Create the engine singleton. Called once from `main`, before launch, so
/// hooks and callbacks can always reach it. Also joins the cross-tab
/// channel: from here on every tab participates in the leader election.
/// `T` is the app's table enum — the ONE instantiation of this library
/// in the app. `version` is the app's schema version (`SCHEMA_VERSION`
/// in shared): a malformed const fails with [`EngineError::BadVersion`]
/// — the app's own setup bug, surfaced where the app can parse it
/// instead of a hidden panic. On `Err` nothing is registered: the
/// singleton exists only after `Ok`.
pub fn init<T: SyncTableWire>(
    migrations: &'static [(&'static str, &'static str)],
    version: &str,
) -> Result<(), EngineError> {
    // Parse BEFORE any registration: a failure must not half-initialize
    // the engine (callers bailing on Err then meet the documented
    // `engine()` panic only if they call it anyway).
    let version = SchemaVersion::parse(version)?;
    let singleton: Rc<Engine<T>> = Rc::new(Engine {
        migrations,
        version,
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
    Ok(())
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
    pub(super) async fn apply_batch(
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

    /// Fire the rev signal of every registered query whose deps match one
    /// of the touched (table, row) pairs. Owners re-run their queries. As
    /// the leader, this is also the local-echo fan-out: subordinates get
    /// the same bump over BroadcastChannel and re-run their queries there.
    pub(super) fn bump(&self, touched: &[(T, Uuid)]) {
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

/// Remove a subscription without going through the engine — the RAII
/// guard's Drop cannot know the engine's table type (`T`).
pub(crate) fn unsubscribe(id: SubscriptionId) {
    SUBS.with_borrow_mut(|s| s.retain(|s| s.id != id));
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
