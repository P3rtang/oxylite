//! The snapshot pipeline: full-state extraction for far-behind clients.
//! Every table's live rows are aggregated into `oxylite.snapshots` (ONE
//! transaction, one generation), tombstones ride along, and the stored
//! generation is served while fresh enough. The per-table extraction is
//! the app's [`SnapshotSource`] impl; the storage schema is the lib's
//! own migration. The op pipeline lives in [`super::ops`].

use enum_iterator::all;
use sqlx::postgres::{PgConnection, PgPool};
use std::future::Future;

use super::ops::current_cursor;
use crate::contract::table::SyncTable;
use crate::protocol::Tombstone;
use crate::protocol::timestamp::Timestamp;

/// Replay is fine for small backlogs, but a client that is more than this
/// many events behind gets a snapshot instead (fresh IndexedDB, or a long
/// offline stretch): one bulk load instead of N row-by-row upserts.
pub const SNAPSHOT_AFTER_OPS: i64 = 50;

/// A stored snapshot may be served while younger than this — it is allowed
/// to be stale, but not arbitrarily so.
const SNAPSHOT_MAX_AGE_SEC: i64 = 3600;

/// …or while the sync log has moved at most this far past its seq.
const SNAPSHOT_MAX_LAG: i64 = 5_000;

/// Per-table snapshot extraction, implemented app-side where the checked
/// SQL lives. The match on the table is the compile guarantee: exhaustive,
/// so a new table variant breaks the build until its arm exists.
///
/// The future is `Send` on purpose: the whole snapshot pipeline must stay
/// `tokio::spawn`-able for backend runtimes that need it (impls use plain
/// `async fn` — Send-ness is checked at the impl).
pub trait SnapshotSource<T: SyncTable> {
    /// The snapshot payload for `table`, aggregated from its live rows.
    /// Runs on the rebuild's transaction: every table is read from ONE
    /// point-in-time, so a concurrent push between two tables' reads
    /// cannot tear the snapshot into mixed generations.
    fn snapshot(
        &self,
        table: T,
        tx: &mut PgConnection,
    ) -> impl Future<Output = Result<serde_json::Value, sqlx::Error>> + Send;
}

/// One table's snapshot: every live row, payload-shaped. The app maps
/// this onto its wire DTOs.
pub struct TableRows<T: SyncTable> {
    pub table: T,
    pub rows: Vec<serde_json::Value>,
}

/// Rebuild every table's snapshot from the live tables (the source of
/// truth — cheaper and simpler than replaying sync_log). ONE transaction:
/// the log head and every table's rows are read at the same point-in-time
/// and the generation lands atomically — a snapshot can never mix table
/// generations (a mixed one would report a seq some tables never reached,
/// and clients would skip that stretch of log).
pub async fn rebuild_snapshots<T: SyncTable, S: SnapshotSource<T>>(
    db: &PgPool,
    source: &S,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let head: i64 =
        sqlx::query_scalar!(r#"SELECT COALESCE(MAX(seq), 0) as "head!" FROM oxylite.sync_log"#,)
            .fetch_one(&mut *tx)
            .await?;

    // `all()` is derived from the table enum, so a new variant joins this
    // loop automatically; `SnapshotSource` must then handle it, or the
    // build breaks.
    for table in all::<T>() {
        let data = source.snapshot(table, &mut tx).await?;

        sqlx::query!(
            "INSERT INTO oxylite.snapshots (table_name, seq, data)
             VALUES ($1, $2, $3)
             ON CONFLICT (table_name) DO UPDATE
               SET seq = EXCLUDED.seq, created_at = now(), data = EXCLUDED.data",
            table.as_str(),
            head,
            data,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
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
) -> Result<(i64, Vec<TableRows<T>>, Vec<Tombstone<T>>), sqlx::Error> {
    let head = current_cursor(db).await;

    let stored = sqlx::query!(
        r#"SELECT seq,
                  COALESCE(EXTRACT(EPOCH FROM (now() - created_at)), 0)::bigint as "age_sec!"
           FROM oxylite.snapshots ORDER BY seq DESC LIMIT 1"#,
    )
    .fetch_optional(db)
    .await?;

    let fresh = matches!(&stored, Some(snapshot)
        if head - snapshot.seq <= SNAPSHOT_MAX_LAG && snapshot.age_sec < SNAPSHOT_MAX_AGE_SEC);

    if !fresh {
        rebuild_snapshots(db, source).await?;
    }

    // All tables are rebuilt together, so any row's seq is the generation.
    let rows =
        sqlx::query!("SELECT table_name, seq, data FROM oxylite.snapshots ORDER BY table_name",)
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

    // Tombstones are the lib's own table, mapped onto the caller's table
    // type via `T::from_name`. Unknown table names (from a newer client)
    // are skipped: this side is the compat boundary.
    let tombstone_rows = sqlx::query!(
        "SELECT table_name, id, deleted_at FROM oxylite.tombstones ORDER BY table_name, id"
    )
    .fetch_all(db)
    .await?;
    let mut tombstones = Vec::with_capacity(tombstone_rows.len());
    for row in tombstone_rows {
        // Unknown table names (from a newer client) are skipped: this
        // side is the compat boundary.
        let Some(table) = T::from_name(&row.table_name) else {
            continue;
        };
        // Infallible: the column is timestamptz, so the decode is a real
        // DateTime — the storage layer did the validating.
        tombstones.push(Tombstone {
            table,
            id: row.id,
            deleted_at: Timestamp::from_datetime(row.deleted_at),
        });
    }

    Ok((seq, tables, tombstones))
}

/// Snapshot extraction for SyncRow tables: the SELECT is
/// table-name-generic (the name comes from the `SyncTable` enum, never
/// user input), so ONE impl serves every table forever — a new table
/// needs no snapshot arm at all. `ORDER BY id` is the protocol's pk
/// convention (`SyncRow::PK`'s default).
#[derive(Clone, Copy)]
pub struct SyncRowSnapshots;

impl<T: SyncTable> SnapshotSource<T> for SyncRowSnapshots {
    async fn snapshot(
        &self,
        table: T,
        tx: &mut PgConnection,
    ) -> Result<serde_json::Value, sqlx::Error> {
        sqlx::query_scalar::<_, serde_json::Value>(&format!(
            "SELECT COALESCE(jsonb_agg(row_to_json(n)), '[]'::jsonb) \
             FROM (SELECT * FROM {} ORDER BY id) n",
            table.as_str()
        ))
        .fetch_one(tx)
        .await
    }
}
