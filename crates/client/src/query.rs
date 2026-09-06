//! Reactive query layer: firestore-shaped reads over the local DB.
//!
//! Components call [`use_query`] with a [`Query`] and get a signal of typed
//! rows that re-runs whenever the query's [`Dep`]s fire — a base-table change
//! or a specific row change (row-level listeners). No component ever touches
//! the websocket; the Engine owns that and bumps query revisions here.

use crate::engine::{engine, log};
use dioxus::prelude::*;
use shared::Table;
use std::rc::Rc;
use uuid::Uuid;

/// A raw row from the local DB: a JS object keyed by column name, with
/// string values (our schema is text-only; see pglite.rs).
pub type Row = js_sys::Object;

/// What invalidates a query. `Table` fires on any change to that table;
/// `Row` only when that specific row changes. Both may be listed — the
/// engine matches either against the (table, row_id) of every applied
/// change, local or remote.
///
/// `Row` is defined now (approved design) and used once a detail view
/// exists; `param` likewise.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    Table(Table),
    Row(Uuid),
}

impl Dep {
    pub(crate) fn matches(self, table: Table, row_id: Uuid) -> bool {
        match self {
            Dep::Table(t) => t == table,
            Dep::Row(id) => id == row_id,
        }
    }
}

/// A read to run against the local DB. `id` is generated at construction
/// and is the registry key for `listen`/`unsubscribe` (a subscription rides
/// the component that built the query; see `use_query`).
#[derive(Clone)]
pub struct Query {
    pub(crate) id: Uuid,
    pub(crate) sql: String,
    pub(crate) params: Vec<String>,
    pub(crate) deps: Vec<Dep>,
}

impl Query {
    pub fn new(sql: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            sql: sql.into(),
            params: Vec::new(),
            deps: Vec::new(),
        }
    }

    #[allow(dead_code)] // planned API (SYNC_API.md); first use is a detail view
    pub fn param(mut self, value: impl Into<String>) -> Self {
        self.params.push(value.into());
        self
    }

    pub fn dep(mut self, dep: Dep) -> Self {
        self.deps.push(dep);
        self
    }
}

/// Row -> typed result, mirroring sqlx's `FromRow` so a shared type maps
/// with `query_as` on the server and with this trait on the client. The
/// engine stays row-generic; result types own their conversion.
pub trait FromRow: Sized {
    fn from_row(row: &Row) -> Result<Self, String>;
}

/// Subscription id: identifies a registered query for unregistration.
pub type SubId = Uuid;

/// A live query registered with the engine.
#[derive(Clone)]
pub(crate) struct Sub {
    pub(crate) id: SubId,
    pub(crate) deps: Vec<Dep>,
    pub(crate) rev: Signal<u64>,
}

/// RAII subscription guard: the "unsub callback" is its Drop impl, so a
/// component literally cannot leak a registration — when the LAST handle
/// drops (component unmounted, hook state released), the engine forgets
/// the query. Cloning shares the same subscription; the id stays
/// engine-internal (callers never need it).
#[derive(Clone)]
pub struct Subscription {
    /// Only purpose is Drop timing: releasing the last Rc unsubscribes.
    _inner: Rc<SubGuardInner>,
}

struct SubGuardInner {
    id: SubId,
}

impl Drop for SubGuardInner {
    fn drop(&mut self) {
        engine().unsubscribe(self.id);
    }
}

impl Subscription {
    pub(crate) fn new(id: SubId) -> Self {
        Self {
            _inner: Rc::new(SubGuardInner { id }),
        }
    }
}

/// Dioxus hook: run a query once now, then re-run whenever its deps fire
/// (locally or via server events). Unregisters on unmount via the
/// subscription guard's Drop.
///
/// The engine knows nothing about `T`; mapping is a generic `FromRow`
/// implementation. Params are fixed for the component's lifetime (a param
/// change re-creating the subscription is future work).
pub fn use_query<T: FromRow + 'static>(query: Query) -> Signal<Vec<T>> {
    let mut out = use_signal(Vec::<T>::new);
    let rev = use_signal(|| 0u64);
    let run_query = query.clone();

    // Register once per component lifetime; the returned guard
    // unsubscribes when this component's hook state is dropped.
    // Registered for this component's lifetime; the guard's Drop (last
    // handle) unsubscribes when the hook state is released.
    let _subscription = use_hook(|| engine().listen(query, rev));

    // Initial load + re-run on every invalidation of our deps. Reading
    // `rev` inside the effect subscribes us to those bumps.
    use_effect(move || {
        rev.read();
        let q = run_query.clone();
        spawn(async move {
            match engine().query::<T>(&q).await {
                Ok(rows) => out.set(rows),
                Err(e) => log("query", &format!("query failed: {e}")),
            }
        });
    });

    out
}
