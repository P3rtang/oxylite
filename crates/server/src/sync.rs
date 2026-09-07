use shared::{Op, Table, TableData};
use sqlx::postgres::PgPool;
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

/// Replay is fine for small backlogs, but a client that is more than this
/// many events behind gets a snapshot instead (fresh IndexedDB, or a long
/// offline stretch): one bulk load instead of N row-by-row upserts.
pub const SNAPSHOT_AFTER_OPS: i64 = 50;

/// A stored snapshot may be served while younger than this — it is allowed
/// to be stale, but not arbitrarily so.
const SNAPSHOT_MAX_AGE_SEC: i64 = 3600;

/// …or while the sync log has moved at most this far past its seq.
const SNAPSHOT_MAX_LAG: i64 = 5_000;

/// Upsert each note and append it to the sync log (server side).
pub async fn push(db: &PgPool, ops: &[Op]) -> Result<i64, SyncError> {
    let mut tx = db.begin().await?;

    for op in ops {
        match op.table {
            Table::Notes => {
                let note: shared::Note = serde_json::from_value(op.data.clone())?;

                // LWW: the server is the source of truth for concurrent edits.
                sqlx::query(
                    "INSERT INTO notes (id, title, body, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE
               SET title = EXCLUDED.title,
                   body = EXCLUDED.body,
                   updated_at = EXCLUDED.updated_at",
                )
                .bind(note.id)
                .bind(note.title)
                .bind(note.body)
                .bind(&note.updated_at)
                .execute(&mut *tx)
                .await?;
            }
        }

        sqlx::query(
            "INSERT INTO sync_log (table_name, row_id, payload)
             VALUES ($1, $2::uuid, $3::jsonb)",
        )
        .bind(op.table.as_str())
        .bind(op.id.to_string())
        .bind(serde_json::to_string(&op.data)?)
        .execute(&mut *tx)
        .await?;
    }

    let cursor: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0)::bigint FROM sync_log")
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(cursor)
}

/// Events after `since`, batched, with the cursor they reach.
pub async fn pull_since(db: &PgPool, since: i64) -> Result<(Vec<Op>, i64), SyncError> {
    let cursor: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0)::bigint FROM sync_log")
        .fetch_one(db)
        .await?;

    if since >= cursor {
        return Ok((Vec::new(), cursor));
    }

    let rows: Vec<(String, sqlx::types::Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT table_name, row_id, payload
         FROM sync_log WHERE seq > $1 ORDER BY seq LIMIT 1000",
    )
    .bind(since)
    .fetch_all(db)
    .await?;

    let events = rows
        .into_iter()
        // Unknown table names (newer client) are skipped: this server is
        // the compat boundary and can't apply what it doesn't know.
        .filter_map(|(table, id, data)| {
            let table = Table::from_name(&table)?;
            Some(Op {
                table,
                id,
                data,
                updated_at: String::new(), // kept in payload; unused by the client upsert
            })
        })
        .collect();

    Ok((events, cursor))
}

pub async fn current_cursor(db: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0)::bigint FROM sync_log")
        .fetch_one(db)
        .await
        .unwrap_or(0)
}

/// Full state for a far-behind client. Serves the stored snapshot when it
/// is fresh enough (young enough, close enough to the log head), else
/// rebuilds it first.
pub async fn load_or_build_snapshot(db: &PgPool) -> Result<(i64, Vec<TableData>), SyncError> {
    let head = current_cursor(db).await;

    let stored: Option<(i64, i64)> = sqlx::query_as(
        "SELECT seq, COALESCE(EXTRACT(EPOCH FROM (now() - created_at)), 0)::bigint
         FROM snapshots ORDER BY seq DESC LIMIT 1",
    )
    .fetch_optional(db)
    .await?;

    let fresh = matches!(&stored, Some((seq, age_sec))
        if head - seq <= SNAPSHOT_MAX_LAG && *age_sec < SNAPSHOT_MAX_AGE_SEC);

    if !fresh {
        rebuild_snapshots(db).await?;
    }

    // All tables are rebuilt together, so any row's seq is the generation.
    let rows: Vec<(String, i64, serde_json::Value)> =
        sqlx::query_as("SELECT table_name, seq, data FROM snapshots ORDER BY table_name")
            .fetch_all(db)
            .await?;

    let seq = rows.first().map(|(_, s, _)| *s).unwrap_or(head);
    let tables = rows
        .into_iter()
        .filter_map(|(name, _, data)| {
            // Unknown names (newer client) are skipped: compat boundary.
            let table = Table::from_name(&name)?;
            Some(TableData {
                table,
                rows: match data {
                    serde_json::Value::Array(rows) => rows,
                    _ => Vec::new(),
                },
            })
        })
        .collect();

    Ok((seq, tables))
}

/// Rebuild every table's snapshot from the live tables (the source of
/// truth — cheaper and simpler than replaying sync_log). New `Table`
/// variants must add an arm here.
async fn rebuild_snapshots(db: &PgPool) -> Result<(), SyncError> {
    let head = current_cursor(db).await;

    // Single statement: the row set and the seq it is stamped with come
    // from one MVCC view, so a snapshot never misses an op it claims.
    let notes: serde_json::Value = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(row_to_json(n)), '[]'::jsonb)
         FROM (SELECT * FROM notes ORDER BY id) n",
    )
    .fetch_one(db)
    .await?;

    sqlx::query(
        "INSERT INTO snapshots (table_name, seq, data)
         VALUES ($1, $2, $3::jsonb)
         ON CONFLICT (table_name) DO UPDATE
           SET seq = EXCLUDED.seq, created_at = now(), data = EXCLUDED.data",
    )
    .bind(Table::Notes.as_str())
    .bind(head)
    .bind(serde_json::to_string(&notes)?)
    .execute(db)
    .await?;

    Ok(())
}
