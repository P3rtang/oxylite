pub use sync_lib::server::SyncRowSnapshots;
use sync_lib::server::{self, OpApply};
use shared::{Note, Op, Table, TableData, Tombstone};
use sqlx::postgres::PgPool;
// The lib owns the protocol machinery AND the WS transport; the app
// plugs in its tables via the impls below and re-exports the entry
// points so main.rs and the integration tests keep one module to talk
// to.
pub use sync_lib::server::{SyncError, pull_since};
pub use sync_lib::ws::sync_router;

/// One migrator over the UNION list (the lib's protocol tables + this
/// app's tables — `shared::MIGRATIONS` is the merged list shared's
/// build.rs generates). sqlx validates every applied row against its
/// list, so lib and app migrations must apply as ONE Migrator sharing
/// one `_sqlx_migrations` table — two separate migrators fail with
/// VersionMissing on each other's rows. This IS the app #2 recipe:
/// concatenate `sync::MIGRATIONS` with your own list, build, run.
pub fn migrator() -> sqlx::migrate::Migrator {
    let migrations = shared::MIGRATIONS
        .iter()
        .map(|(name, sql)| {
            // `NNNN_name` — the four-digit stem is the version; build.rs
            // enforces the convention (and global uniqueness) at compile
            // time, so this parse cannot fail.
            let version: i64 = name[..4].parse().expect("NNNN version stem");
            sqlx::migrate::Migration::new(
                version,
                (*name).into(),
                sqlx::migrate::MigrationType::Simple,
                (*sql).into(),
                false,
            )
        })
        .collect();

    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
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
