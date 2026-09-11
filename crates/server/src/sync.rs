use shared::delete::OpKind;
use shared::{Op, Table, TableData, Tombstone};
use sqlx::postgres::{PgConnection, PgPool};
use sync_lib::server::{self, OpApply, SnapshotSource};
// The lib owns the protocol machinery AND the WS transport; the app
// plugs in its tables via the two impls below and re-exports the entry
// points so main.rs and the integration tests keep one module to talk
// to.
pub use sync_lib::server::{SyncError, pull_since};
pub use sync_lib::ws::sync_router;

/// The app's apply arms — the checked SQL lives at these macro call
/// sites. Both matches are exhaustive, so a new `Table` variant (or a
/// new op kind) breaks the build here (same guarantee as the match in
/// the snapshot extraction).
#[derive(Clone, Copy)]
pub struct Apply;

impl OpApply for Apply {
    async fn apply(&self, op: &Op, tx: &mut PgConnection) -> Result<(), SyncError> {
        // Timestamps arrive as typed `Timestamp`s (validated at the serde
        // boundary when the WS message parsed) — malformed values cannot
        // reach here by type. The columns are `timestamptz` (migration
        // 0007), so chrono values bind directly.
        match op.kind() {
            OpKind::Upsert => match op.table {
                Table::Notes => {
                    let note: shared::Note = serde_json::from_value(op.data.clone())?;

                    // LWW upsert, tombstone-guarded: writes older than the
                    // row's delete are filtered before they can insert.
                    sqlx::query!(
                        "INSERT INTO notes (id, title, body, updated_at)
                         SELECT $1::uuid, $2, $3, $4
                         WHERE NOT EXISTS (
                           SELECT 1 FROM tombstones
                           WHERE table_name = 'notes'
                             AND id = $1 AND deleted_at >= $4
                         )
                         ON CONFLICT (id) DO UPDATE
                           SET title = EXCLUDED.title,
                               body = EXCLUDED.body,
                               updated_at = EXCLUDED.updated_at",
                        note.id,
                        note.title,
                        note.body,
                        note.updated_at.as_datetime(),
                    )
                    .execute(&mut *tx)
                    .await?;

                    // A strictly newer write resurrects: clear the tombstone.
                    sqlx::query!(
                        "DELETE FROM tombstones
                         WHERE table_name = 'notes' AND id = $1 AND deleted_at < $2",
                        note.id,
                        note.updated_at.as_datetime(),
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            },
            OpKind::Delete => match op.table {
                Table::Notes => {
                    // LWW-guarded delete, tombstoning only what it actually
                    // removed: a replayed or older delete loses to a newer
                    // resurrected row — no removal, no re-tombstone.
                    sqlx::query!(
                        r#"WITH gone AS (
                             DELETE FROM notes
                             WHERE id = $1 AND updated_at < $2
                             RETURNING id
                           )
                           INSERT INTO tombstones (table_name, id, deleted_at)
                           SELECT 'notes', $1, $2
                           WHERE EXISTS (SELECT 1 FROM gone)
                           ON CONFLICT (table_name, id) DO UPDATE
                             SET deleted_at = EXCLUDED.deleted_at
                           WHERE EXCLUDED.deleted_at > tombstones.deleted_at"#,
                        op.id,
                        op.updated_at.as_datetime(),
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            },
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

/// The app's snapshot extraction: the checked SQL lives at these macro
/// call sites. The match is exhaustive, so a new `Table` variant breaks
/// the build here (same guarantee as the match in [`Apply`]).
#[derive(Clone, Copy)]
pub struct Snapshots;

impl SnapshotSource<Table> for Snapshots {
    async fn snapshot(&self, table: Table, db: &PgPool) -> Result<serde_json::Value, sqlx::Error> {
        Ok(match table {
            Table::Notes => {
                // Single statement: the row set and the seq it is stamped
                // with come from one MVCC view, so a snapshot never misses
                // an op it claims.
                sqlx::query_scalar!(
                    r#"SELECT COALESCE(jsonb_agg(row_to_json(n)), '[]'::jsonb) as "notes!"
                       FROM (SELECT * FROM notes ORDER BY id) n"#,
                )
                .fetch_one(db)
                .await?
            }
        })
    }
}

/// Full state for a far-behind client — the lib's generic pipeline with
/// this app's tables and extraction plugged in. Tombstones ride the
/// snapshot: a client snapshotting past a delete never replays that
/// delete op, so without them it would resurrect the row.
pub async fn load_or_build_snapshot(
    db: &PgPool,
) -> Result<(i64, Vec<TableData>, Vec<Tombstone>), SyncError> {
    let (seq, tables, tombstones) = server::load_or_build_snapshot(db, &Snapshots).await?;
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
