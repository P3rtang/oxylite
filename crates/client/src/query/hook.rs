//! Dioxus hooks: live, typed reads over the engine.

use crate::engine::{EngineError, engine, log};
use dioxus::prelude::*;

use super::dep::Dep;
use super::from_row::FromRow;
use super::statement::Query;
use super::sync_row::SyncRow;

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

/// The zero-config read for a synced type: `use_all::<Note>()` — the
/// SyncRow-generated all-rows query, live on its whole table. Generic
/// methods like this are the point of the contract: any row type that
/// impls it gets the full read/write machinery, and anything bespoke
/// (joined views, filtered lists) overrides the defaults or passes its
/// own `Query` to `use_query`.
pub fn use_all<T: SyncRow + 'static>() -> Signal<Result<Vec<T>, EngineError>> {
    use_query::<T>(Query::new(&T::select_all_sql()).dep(Dep::Table(T::TABLE)))
}
