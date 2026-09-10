//! Server-side sync machinery (feature `server`): the snapshot pipeline
//! every backend of this protocol needs, generic over the app's table
//! type. Tables are defined by the implementing repo — they derive
//! `enum_iterator::Sequence`, implement [`SyncTable`] for the wire names,
//! and implement [`SnapshotSource`] for the per-table extraction (which
//! stays app-side: that's where the sqlx macros can see the SQL literals,
//! and where the exhaustive match forces new tables to be handled).
//! The `sync_log`/`snapshots` schema comes from the shared migrations.

use enum_iterator::{Sequence, all};
use shared::Table;
use sqlx::postgres::PgPool;
use std::future::Future;

/// A synced table, as the machinery sees it. The implementing repo's
/// table enum derives `Sequence` (so a new variant joins every loop
/// automatically) and implements this for its wire names.
pub trait SyncTable: Copy + Sequence {
    /// The wire name stored in `sync_log.table_name` and
    /// `snapshots.table_name`; must stay stable across releases.
    fn as_str(self) -> &'static str;

    /// Inverse of [`SyncTable::as_str`]. Returning `None` means "unknown
    /// here" (a table from a newer client): replaying sites skip it —
    /// this side is the compat boundary and can't apply what it doesn't
    /// know.
    fn from_name(name: &str) -> Option<Self>;
}

/// This workspace's table type (the lib is coupled to `shared` for the
/// wire DTOs anyway). Other projects implement [`SyncTable`] for their
/// own enum instead.
impl SyncTable for Table {
    fn as_str(self) -> &'static str {
        Table::as_str(self)
    }

    fn from_name(name: &str) -> Option<Self> {
        Table::from_name(name)
    }
}

/// Per-table snapshot extraction, implemented app-side where the checked
/// SQL lives. The match on the table is the compile guarantee: exhaustive,
/// so a new `Table` variant breaks the build until its arm exists.
///
/// The future is `Send` on purpose: the whole snapshot pipeline must stay
/// `tokio::spawn`-able for backend runtimes that need it (impls use plain
/// `async fn` — Send-ness is checked at the impl).
pub trait SnapshotSource<T: SyncTable> {
    /// The snapshot payload for `table`, aggregated from its live rows.
    fn snapshot(
        &self,
        table: T,
        db: &PgPool,
    ) -> impl Future<Output = Result<serde_json::Value, sqlx::Error>> + Send;
}

/// One table's snapshot: every live row, payload-shaped. The app maps
/// this onto its wire DTOs.
pub struct TableRows<T: SyncTable> {
    pub table: T,
    pub rows: Vec<serde_json::Value>,
}

/// Replay is fine for small backlogs, but a client that is more than this
/// many events behind gets a snapshot instead (fresh IndexedDB, or a long
/// offline stretch): one bulk load instead of N row-by-row upserts.
pub const SNAPSHOT_AFTER_OPS: i64 = 50;

/// A stored snapshot may be served while younger than this — it is allowed
/// to be stale, but not arbitrarily so.
const SNAPSHOT_MAX_AGE_SEC: i64 = 3600;

/// …or while the sync log has moved at most this far past its seq.
const SNAPSHOT_MAX_LAG: i64 = 5_000;

/// The server's cursor: the newest seq in the sync log.
pub async fn current_cursor(db: &PgPool) -> i64 {
    sqlx::query_scalar!(r#"SELECT COALESCE(MAX(seq), 0) as "cursor!" FROM sync_log"#)
        .fetch_one(db)
        .await
        .unwrap_or(0)
}

/// Rebuild every table's snapshot from the live tables (the source of
/// truth — cheaper and simpler than replaying sync_log).
pub async fn rebuild_snapshots<T: SyncTable, S: SnapshotSource<T>>(
    db: &PgPool,
    source: &S,
) -> Result<(), sqlx::Error> {
    let head = current_cursor(db).await;

    // `all()` is derived from the table enum, so a new variant joins this
    // loop automatically; `SnapshotSource` must then handle it, or the
    // build breaks.
    for table in all::<T>() {
        let data = source.snapshot(table, db).await?;

        sqlx::query!(
            "INSERT INTO snapshots (table_name, seq, data)
             VALUES ($1, $2, $3)
             ON CONFLICT (table_name) DO UPDATE
               SET seq = EXCLUDED.seq, created_at = now(), data = EXCLUDED.data",
            table.as_str(),
            head,
            data,
        )
        .execute(db)
        .await?;
    }

    Ok(())
}

/// Full state for a far-behind client. Serves the stored snapshot when it
/// is fresh enough (young enough, close enough to the log head), else
/// rebuilds it first. Tombstones ride along: a client that snapshots past
/// a delete never replays that delete op, so without them it would
/// resurrect the row on the next stale replay. The tombstones query is
/// lib-owned (the table is generic), read after the snapshot rows — a
/// delete committing in between is still safe: the row is simply absent
/// from the snapshot, and the tombstone only makes that fact explicit.
pub async fn load_or_build_snapshot<T: SyncTable, S: SnapshotSource<T>>(
    db: &PgPool,
    source: &S,
) -> Result<(i64, Vec<TableRows<T>>, Vec<shared::Tombstone>), sqlx::Error> {
    let head = current_cursor(db).await;

    let stored = sqlx::query!(
        r#"SELECT seq,
                  COALESCE(EXTRACT(EPOCH FROM (now() - created_at)), 0)::bigint as "age_sec!"
           FROM snapshots ORDER BY seq DESC LIMIT 1"#,
    )
    .fetch_optional(db)
    .await?;

    let fresh = matches!(&stored, Some(snapshot)
        if head - snapshot.seq <= SNAPSHOT_MAX_LAG && snapshot.age_sec < SNAPSHOT_MAX_AGE_SEC);

    if !fresh {
        rebuild_snapshots(db, source).await?;
    }

    // All tables are rebuilt together, so any row's seq is the generation.
    let rows = sqlx::query!("SELECT table_name, seq, data FROM snapshots ORDER BY table_name",)
        .fetch_all(db)
        .await?;

    let seq = rows.first().map(|row| row.seq).unwrap_or(head);
    let tables = rows
        .into_iter()
        .filter_map(|row| {
            let table = T::from_name(&row.table_name)?;
            Some(TableRows {
                table,
                rows: match row.data {
                    serde_json::Value::Array(rows) => rows,
                    _ => Vec::new(),
                },
            })
        })
        .collect();

    // Tombstones are the lib's own table, mapped onto the workspace's wire
    // table type (the lib is coupled to `shared` for the wire DTOs).
    // Unknown table names (from a newer client) are skipped: this side is
    // the compat boundary.
    let tombstone_rows =
        sqlx::query!("SELECT table_name, id, deleted_at FROM tombstones ORDER BY table_name, id")
            .fetch_all(db)
            .await?;
    let tombstones = tombstone_rows
        .into_iter()
        .filter_map(|row| {
            let table = shared::Table::from_name(&row.table_name)?;
            Some(shared::Tombstone {
                table,
                id: row.id,
                deleted_at: row.deleted_at,
            })
        })
        .collect();

    Ok((seq, tables, tombstones))
}
