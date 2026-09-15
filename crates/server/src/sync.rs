pub use oxylite::server::SyncRowSnapshots;
use oxylite::server::{self, OpApply};
use shared::{Note, Op, Table, TableData, Tombstone};
use sqlx::postgres::PgPool;
// The lib owns the protocol machinery AND the WS transport; the app
// plugs in its tables via the impls below and re-exports the entry
// points so main.rs and the integration tests keep one module to talk
// to.
pub use oxylite::server::{OPS_CHANNEL, spawn_wake_adapter};
pub use oxylite::server::{SyncError, pull_since};
pub use oxylite::ws::{DEFAULT_FALLBACK_TICK, sync_router};

/// One migrator over the UNION list — now one line: the lib's
/// `server::migrator` merges the lib's protocol migrations with the
/// app's `APP_MIGRATIONS` (shared's build.rs scans `migrations/`), the
/// same merge the client's `Pglite::init` does for PGlite. sqlx
/// validates every applied row against ONE list over ONE
/// `_sqlx_migrations` table — lib and app must ride together (two
/// separate migrators fail VersionMissing on each other's rows). This IS
/// the app recipe: `oxylite::server::migrator(oxylite::migrations!(
/// "migrations"))`, build, run.
pub fn migrator() -> sqlx::migrate::Migrator {
    oxylite::server::migrator(shared::APP_MIGRATIONS)
}

/// The app's table hookup: ONE arm per table — the row type IS the
/// hookup. The statements are SyncRow-generated (shared's `impl SyncRow
/// for Note`), so a new table is a row impl + this arm, and the match
/// stays the compile guarantee (a new `Table` variant breaks the build
/// until its arm exists).
#[derive(Clone, Copy)]
pub struct Apply;

impl OpApply<Table> for Apply {
    async fn apply(&self, op: &Op, tx: &mut sqlx::postgres::PgConnection) -> Result<(), SyncError> {
        match op.table {
            Table::Notes => server::apply_one::<Note>(op, tx).await?,
        }
        Ok(())
    }
}

/// Two-arg shape kept for the binary + the integration tests; the
/// generic machinery (transaction, per-op logging, cursor) lives in the
/// lib behind [`OpApply`].
pub async fn push(db: &PgPool, ops: &[Op]) -> Result<i64, SyncError> {
    server::push(db, ops, &Apply).await
}

/// Full state for a far-behind client — the lib's generic pipeline with
/// the table-generic extraction plugged in. Tombstones ride the
/// snapshot: a client snapshotting past a delete never replays that
/// delete op, so without them it would resurrect the row.
pub async fn load_or_build_snapshot(
    db: &PgPool,
) -> Result<(i64, Vec<TableData>, Vec<Tombstone>), SyncError> {
    let (seq, tables, tombstones) = server::load_or_build_snapshot(db, &SyncRowSnapshots).await?;
    Ok((
        seq,
        tables
            .into_iter()
            .map(|rows| TableData {
                table: rows.table,
                rows: rows.rows,
            })
            .collect(),
        tombstones,
    ))
}
