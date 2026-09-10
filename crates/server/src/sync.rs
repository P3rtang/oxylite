use shared::delete::OpKind;
use shared::timestamp::Timestamp;
use shared::{Op, Table, TableData, Tombstone};
use sqlx::postgres::PgPool;
use sync_lib::server::{self, SnapshotSource};
use thiserror::Error;

/// Why a server sync operation failed. Typed so callers match on the shape
/// of the failure; sources convert with `#[from]`.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub use sync_lib::server::{SNAPSHOT_AFTER_OPS, current_cursor};

/// Apply each op to the live tables and append it to the sync log
/// (server side). A delete is an op like any other — `{table, id,
/// updated_at, data: null}` — applied as a real DELETE plus a tombstone;
/// upserts pass the tombstone guard so a stale edit cannot resurrect a
/// deleted row (the ON CONFLICT LWW guard cannot catch an absent row).
/// Every op is logged, dropped ones included: the log is the wire
/// history, guards decide at apply time.
pub async fn push(db: &PgPool, ops: &[Op]) -> Result<i64, SyncError> {
    let mut tx = db.begin().await?;

    for op in ops {
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

        sqlx::query!(
            "INSERT INTO sync_log (table_name, row_id, payload, updated_at)
             VALUES ($1, $2, $3, $4)",
            op.table.as_str(),
            op.id,
            op.data,
            op.updated_at.as_datetime(),
        )
        .execute(&mut *tx)
        .await?;
    }

    let cursor: i64 = sqlx::query_scalar!(
        // COALESCE is an expression, so nullability can't be inferred:
        // force it — this query can only return 0 or a real seq.
        r#"SELECT COALESCE(MAX(seq), 0) as "cursor!" FROM sync_log"#,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(cursor)
}

/// Events after `since`, windowed, with the seq the batch reaches.
pub async fn pull_since(db: &PgPool, since: i64) -> Result<(Vec<Op>, i64), SyncError> {
    let head: i64 =
        sqlx::query_scalar!(r#"SELECT COALESCE(MAX(seq), 0) as "head!" FROM sync_log"#,)
            .fetch_one(db)
            .await?;

    if since >= head {
        return Ok((Vec::new(), head));
    }

    let rows = sqlx::query!(
        "SELECT seq, table_name, row_id, payload, updated_at
         FROM sync_log WHERE seq > $1 ORDER BY seq LIMIT 1000",
        since,
    )
    .fetch_all(db)
    .await?;

    // The cursor is the LAST STREAMED seq, never the head: the batch is a
    // window into the backlog, and returning the head here let clients
    // skip every event past the window (their cursor jumped ahead while
    // the data stayed on the server).
    let cursor = rows.last().map(|row| row.seq).unwrap_or(head);

    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        // Unknown table names (newer client) are skipped: this server is
        // the compat boundary and can't apply what it doesn't know.
        let Some(table) = Table::from_name(&row.table_name) else {
            continue;
        };
        events.push(Op {
            table,
            id: row.row_id,
            data: row.payload,
            // The op's own timestamp: a delete's payload is null (that
            // is the marker), so `updated_at` must ride the log row.
            // Infallible: the column is timestamptz, so the decode is a
            // real DateTime — the storage layer did the validating.
            updated_at: Timestamp::from_datetime(row.updated_at),
        });
    }

    Ok((events, cursor))
}

/// The app's snapshot extraction: the checked SQL lives at these macro
/// call sites. The match is exhaustive, so a new `Table` variant breaks
/// the build here (same guarantee as the match in `push`).
struct Snapshots;

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
