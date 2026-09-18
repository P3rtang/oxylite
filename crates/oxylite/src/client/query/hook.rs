//! Dioxus hooks: live, typed reads over the engine.

use crate::client::engine::{EngineError, engine};
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
/// The re-run driver is the prelude's `use_resource` — its future
/// cancels the in-flight run when deps re-fire, so the newest
/// invalidation's query is the newest answer (the spawn-based version
/// landed out of order: two close bumps raced two queries and the OLDER
/// result could overwrite the newer until the next bump). The engine
/// side is unchanged — `listen` bumps the `rev` signal and the
/// subscription rides the guard's Drop — the hook just reads `rev` in
/// the resource closure so the engine's invalidation IS the reactive
/// dependency.
///
/// The engine knows nothing about `R`; mapping is a generic `FromRow`
/// implementation. `K` is the app's table enum — the same instantiation
/// `init` was called with (use_select_all infers it from the row type;
/// bespoke queries name it explicitly).
///
/// Params are reactive for free: any signal read into the Query before
/// the async block re-runs it when written, so filtered/bespoke queries
/// no longer need re-creating subscriptions (the old hook froze params
/// for the component's lifetime — the noted future work, done here).
pub fn use_query<R: FromRow + 'static, K: SyncTableWire>(
    query: Query,
) -> Resource<Result<Vec<R>, EngineError>> {
    let rev = use_signal(|| 0u64);
    let run_query = query.clone();
    // Registered for this component's lifetime; the guard's Drop (last
    // handle) unsubscribes when the hook state is released.
    let _subscription = use_hook(|| engine::<K>().listen(query, rev));

    use_resource(move || {
        rev.read();
        let q = run_query.clone();
        async move { engine::<K>().query::<R>(&q).await }
    })
}

/// The zero-config read for a synced type: `use_all::<Note>()` — the
/// SyncRow-generated all-rows query, live on its whole table. Generic
/// methods like this are the point of the contract: any row type that
/// impls it gets the full read/write machinery, and anything bespoke
/// (joined views, filtered lists) overrides the defaults or passes its
/// own `Query` to `use_query`.
pub fn use_select_all<T: SyncRow + FromRow + 'static>() -> Resource<Result<Vec<T>, EngineError>> {
    use_query::<T, T::Table>(Query::new(&T::select_all_sql()).dep(Dep::Table(T::TABLE.as_str())))
}
