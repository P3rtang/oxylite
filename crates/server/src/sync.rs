use shared::{Op, Table, TableData};
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

/// Upsert each note and append it to the sync log (server side).
pub async fn push(db: &PgPool, ops: &[Op]) -> Result<i64, SyncError> {
    let mut tx = db.begin().await?;

    for op in ops {
        match op.table {
            Table::Notes => {
                let note: shared::Note = serde_json::from_value(op.data.clone())?;

                // LWW: the server is the source of truth for concurrent edits.
                sqlx::query!(
                    "INSERT INTO notes (id, title, body, updated_at)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (id) DO UPDATE
                       SET title = EXCLUDED.title,
                           body = EXCLUDED.body,
                           updated_at = EXCLUDED.updated_at",
                    note.id,
                    note.title,
                    note.body,
                    note.updated_at,
                )
                .execute(&mut *tx)
                .await?;
            }
        }

        sqlx::query!(
            "INSERT INTO sync_log (table_name, row_id, payload)
             VALUES ($1, $2, $3)",
            op.table.as_str(),
            op.id,
            op.data,
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
        "SELECT seq, table_name, row_id, payload
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

    let events = rows
        .into_iter()
        // Unknown table names (newer client) are skipped: this server is
        // the compat boundary and can't apply what it doesn't know.
        .filter_map(|row| {
            let table = Table::from_name(&row.table_name)?;
            Some(Op {
                table,
                id: row.row_id,
                data: row.payload,
                updated_at: String::new(), // kept in payload; unused by the client upsert
            })
        })
        .collect();

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
/// this app's tables and extraction plugged in.
pub async fn load_or_build_snapshot(db: &PgPool) -> Result<(i64, Vec<TableData>), SyncError> {
    let (seq, tables) = server::load_or_build_snapshot(db, &Snapshots).await?;
    Ok((
        seq,
        tables
            .into_iter()
            .map(|rows| TableData {
                table: rows.table,
                rows: rows.rows,
            })
            .collect(),
    ))
}
