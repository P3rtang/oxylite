//! Dioxus hooks: live, typed reads over the engine.

use crate::client::engine::{EngineError, engine, log};
use crate::contract::table::{SyncTable, SyncTableWire};
use dioxus::prelude::*;

use super::dep::Dep;
use super::statement::Query;
use crate::contract::from_row::FromRow;
use crate::contract::sync_row::SyncRow;

/// Dioxus hook: run a query once now, then re-run whenever its deps fire
/// (locally or via server events). Unregisters on unmount via the
/// subscription guard's Drop.
///
/// The engine knows nothing about `R`; mapping is a generic `FromRow`
/// implementation. `K` is the app's table enum — the same instantiation
/// `init` was called with (use_select_all infers it from the row type;
/// bespoke queries name it explicitly). Params are fixed for the
/// component's lifetime (a param change re-creating the subscription is
/// future work).
pub fn use_query<R: FromRow + 'static, K: SyncTableWire>(
    query: Query,
) -> Signal<Result<Vec<R>, EngineError>> {
    let mut out = use_signal(|| Ok(Vec::<R>::new()));
    let rev = use_signal(|| 0u64);
    let run_query = query.clone();

    // Registered for this component's lifetime; the guard's Drop (last
    // handle) unsubscribes when the hook state is released.
    let _subscription = use_hook(|| engine::<K>().listen(query, rev));

    // Initial load + re-run on every invalidation of our deps. Reading
    // `rev` inside the effect subscribes us to those bumps.
    use_effect(move || {
        rev.read();
        let q = run_query.clone();

        spawn(async move {
            match engine::<K>().query::<R>(&q).await {
                Ok(rows) => out.set(Ok(rows)),
                Err(e) => out.set(Err(e)),
            }
        });
    });

    out
}

/// The zero-config read for a synced type: `use_all::<Note>()` — the
/// SyncRow-generated all-rows query, live on its whole table. Generic
/// methods like this are the point of the contract: any row type that
/// impls it gets the full read/write machinery, and anything bespoke
/// (joined views, filtered lists) overrides the defaults or passes its
/// own `Query` to `use_query`.
pub fn use_select_all<T: SyncRow + FromRow + 'static>() -> Signal<Result<Vec<T>, EngineError>> {
    use_query::<T, T::Table>(Query::new(&T::select_all_sql()).dep(Dep::Table(T::TABLE.as_str())))
}
