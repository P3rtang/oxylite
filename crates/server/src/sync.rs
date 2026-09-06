use shared::{Op, NOTES_TABLE};
use sqlx::postgres::PgPool;

/// Upsert each note and append it to the sync log (server side).
pub async fn push(db: &PgPool, ops: &[Op]) -> Result<i64, String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;

    for op in ops {
        if op.table != NOTES_TABLE {
            return Err(format!("unsupported table {}", op.table));
        }
        let note: shared::Note =
            serde_json::from_value(op.data.clone()).map_err(|e| e.to_string())?;

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
        .await
        .map_err(|e| e.to_string())?;

        sqlx::query(
            "INSERT INTO sync_log (table_name, row_id, payload)
             VALUES ($1, $2::uuid, $3::jsonb)",
        )
        .bind(op.table.as_str())
        .bind(op.id.to_string())
        .bind(serde_json::to_string(&op.data).map_err(|e| e.to_string())?)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }

    let cursor: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0)::bigint FROM sync_log",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(cursor)
}

/// Events after `since`, batched, with the cursor they reach.
pub async fn pull_since(
    db: &PgPool,
    since: i64,
) -> Result<(Vec<Op>, i64), String> {
    let cursor: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0)::bigint FROM sync_log",
    )
    .fetch_one(db)
    .await
    .map_err(|e| e.to_string())?;

    if since >= cursor {
        return Ok((Vec::new(), cursor));
    }

    let rows: Vec<(String, sqlx::types::Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT table_name, row_id, payload
         FROM sync_log WHERE seq > $1 ORDER BY seq LIMIT 1000",
    )
    .bind(since)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;

    let events = rows
        .into_iter()
        .map(|(table, id, data)| Op {
            table,
            id,
            data,
            updated_at: String::new(), // kept in payload; unused by the client upsert
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
