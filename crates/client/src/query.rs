//! Reactive query layer: firestore-shaped reads over the local DB.
//!
//! Components call [`use_query`] with a [`Query`] and get a signal of typed
//! rows that re-runs whenever the query's [`Dep`]s fire — a base-table change
//! or a specific row change (row-level listeners). No component ever touches
//! the websocket; the Engine owns that and bumps query revisions here.

use crate::engine::{EngineError, engine, log};
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

/// The contract a synced row type fulfills. `FromRow` covers reads; the
/// consts + default methods generate the SQL the engine needs to apply
/// batches — one `COLUMNS` list is the single source of truth for every
/// statement, so hand-written SQL can never drift from the row mapping
/// again (that drift shipped once: a SELECT missing a column FromRow read).
///
/// SQL is generated, not abstracted away: anything beyond these defaults
/// (joins, WHERE clauses) is still a plain hand-written `Query`.
pub trait SyncRow: FromRow + Sized {
    /// The synced table this row type belongs to.
    const TABLE: Table;
    /// Column names in bind order — feeds every generated statement.
    const COLUMNS: &'static [&'static str];
    /// Column compared for last-write-wins (`EXCLUDED.c > t.c`); `None`
    /// means batch order decides.
    const LWW: Option<&'static str> = None;
    /// Primary key column name.
    const PK: &'static str = "id";

    /// Bind values in `COLUMNS` order (schema is text-only, so strings).
    fn params(&self) -> Vec<String>;

    /// The row's primary key — sync row ids are UUIDs by protocol design.
    fn pk(&self) -> Uuid;

    /// INSERT for local writes.
    fn insert_sql() -> String {
        let cols: Vec<&str> = Self::COLUMNS.to_vec();
        let slots: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            Self::TABLE.as_str(),
            cols.join(", "),
            slots.join(", ")
        )
    }

    /// SELECT of all columns, LWW-ordered newest-first when applicable.
    fn select_all_sql() -> String {
        let mut sql = format!(
            "SELECT {} FROM {}",
            Self::COLUMNS.join(", "),
            Self::TABLE.as_str()
        );
        if let Some(c) = Self::LWW {
            sql.push_str(&format!(" ORDER BY {c} DESC"));
        }
        sql
    }

    /// Multi-row LWW upsert for `n` rows: one statement, `n * columns`
    /// placeholders, `ON CONFLICT (pk) DO UPDATE` with an optional
    /// `WHERE EXCLUDED.lww > t.lww` guard. Callers bind row-major.
    fn upsert_sql(n_rows: usize) -> String {
        let cols = Self::COLUMNS;
        let mut sql = format!(
            "INSERT INTO {} ({}) VALUES ",
            Self::TABLE.as_str(),
            cols.join(", ")
        );
        let n_cols = cols.len();
        for r in 0..n_rows {
            if r > 0 {
                sql.push_str(", ");
            }
            let slots: Vec<String> = (1..=n_cols)
                .map(|c| format!("${}", r * n_cols + c))
                .collect();
            sql.push_str(&format!("({})", slots.join(", ")));
        }
        sql.push_str(&format!(" ON CONFLICT ({}) DO UPDATE SET ", Self::PK));
        let sets: Vec<String> = cols
            .iter()
            .filter(|c| **c != Self::PK)
            .map(|c| format!("{c} = EXCLUDED.{c}"))
            .collect();
        sql.push_str(&sets.join(", "));
        if let Some(lww) = Self::LWW {
            sql.push_str(&format!(
                " WHERE EXCLUDED.{lww} > {}.{}",
                Self::TABLE.as_str(),
                lww
            ));
        }
        sql
    }
}

/// Apply a batch of payload rows (Events or a Snapshot): parse, dedup to
/// the last row per PK (log order is the LWW tiebreak), then one multi-row
/// upsert per chunk. A fresh IndexedDB must not pay one round-trip per row.
pub(crate) async fn bulk_upsert<T>(
    db: &crate::pglite::Pglite,
    rows: &[serde_json::Value],
) -> Result<Vec<Uuid>, String>
where
    T: SyncRow + serde::de::DeserializeOwned,
{
    let mut order: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut by_pk: std::collections::HashMap<Uuid, T> =
        std::collections::HashMap::with_capacity(rows.len());
    for v in rows {
        let row: T = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if !by_pk.contains_key(&row.pk()) {
            order.push(row.pk());
        }
        // Same PK twice in one batch: the later log entry wins.
        by_pk.insert(row.pk(), row);
    }

    let chunk_size = (4000 / T::COLUMNS.len()).max(1);
    let items: Vec<T> = order.iter().map(|pk| by_pk.remove(pk).unwrap()).collect();
    for chunk in items.chunks(chunk_size) {
        let params: Vec<String> = chunk.iter().flat_map(|r| r.params().into_iter()).collect();
        db.query(&T::upsert_sql(chunk.len()), &params)
            .await
            .map_err(|e| crate::engine::error_text(&e))?;
    }

    Ok(order)
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
pub fn use_query<T: FromRow + 'static>(query: Query) -> Signal<Result<Vec<T>, EngineError>> {
    let mut out = use_signal(|| Ok(Vec::<T>::new()));
    let rev = use_signal(|| 0u64);
    let run_query = query.clone();

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
                Ok(rows) => out.set(Ok(rows)),
                Err(e) => {
                    log("query", &format!("{e}"));
                    out.set(Err(e));
                }
            }
        });
    });

    out
}
