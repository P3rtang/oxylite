use shared::delete::OpKind;
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
                        note.updated_at,
                    )
                    .execute(&mut *tx)
                    .await?;

                    // A strictly newer write resurrects: clear the tombstone.
                    sqlx::query!(
                        "DELETE FROM tombstones
                         WHERE table_name = 'notes' AND id = $1 AND deleted_at < $2",
                        note.id,
                        note.updated_at,
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
                        op.updated_at,
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
            op.updated_at,
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
                // The op's own timestamp: a delete's payload is null (that
                // is the marker), so `updated_at` must ride the log row.
                updated_at: row.updated_at,
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::delete::OpKind;
    use uuid::Uuid;

    // Canonical ISO, uniform precision — lexicographic == chronological.
    const T1: &str = "2026-09-09T10:00:01.000Z";
    const T2: &str = "2026-09-09T10:00:02.000Z";
    const T3: &str = "2026-09-09T10:00:03.000Z";

    // Delete contracts (#27, ROADMAP 1.2), pinned server-side:
    // a delete is one op `{table, id, updated_at, data: null}`; the live
    // row goes away, a tombstone guards against stale writes, and the op
    // itself stays in sync_log (guards decide at apply time). Run against
    // real Postgres — `#[sqlx::test]` makes a fresh database per test and
    // applies the shared migrations.

    fn upsert_op(id: Uuid, title: &str, at: &str) -> Op {
        Op {
            table: Table::Notes,
            id,
            data: serde_json::json!({ "id": id, "title": title, "body": "", "updated_at": at }),
            updated_at: at.to_string(),
        }
    }

    fn delete_op(id: Uuid, at: &str) -> Op {
        Op {
            table: Table::Notes,
            id,
            data: serde_json::Value::Null,
            updated_at: at.to_string(),
        }
    }

    async fn live_count(db: &PgPool, id: Uuid) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*)::bigint as "count!" FROM notes WHERE id = $1"#,
            id
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn live_row(db: &PgPool, id: Uuid) -> Option<(String, String)> {
        sqlx::query!("SELECT title, updated_at FROM notes WHERE id = $1", id)
            .fetch_optional(db)
            .await
            .unwrap()
            .map(|r| (r.title, r.updated_at))
    }

    /// The row's tombstone, if any: deleted_at.
    async fn tombstone(db: &PgPool, id: Uuid) -> Option<String> {
        sqlx::query_scalar!(
            "SELECT deleted_at FROM tombstones WHERE table_name = 'notes' AND id = $1",
            id
        )
        .fetch_optional(db)
        .await
        .unwrap()
    }

    async fn log_payloads(db: &PgPool) -> Vec<serde_json::Value> {
        sqlx::query_scalar!("SELECT payload FROM sync_log ORDER BY seq")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../shared/migrations")]
    async fn push_delete_removes_row_and_tombstones(pool: PgPool) {
        let id = Uuid::now_v7();
        push(&pool, &[upsert_op(id, "alive", T1)]).await.unwrap();
        assert_eq!(live_count(&pool, id).await, 1);

        push(&pool, &[delete_op(id, T2)]).await.unwrap();
        assert_eq!(live_count(&pool, id).await, 0);
        assert_eq!(tombstone(&pool, id).await.as_deref(), Some(T2));
        // The op itself is logged (payload null), so other clients replay it.
        let payloads = log_payloads(&pool).await;
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[1], serde_json::Value::Null);
    }

    #[sqlx::test(migrations = "../shared/migrations")]
    async fn stale_edit_dropped_live_table_keeps_delete(pool: PgPool) {
        let id = Uuid::now_v7();
        push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
        push(&pool, &[delete_op(id, T2)]).await.unwrap();

        // A client offline before the delete pushes its mid edit: the
        // live table must not resurrect; the op is still logged and the
        // tombstone stands.
        push(&pool, &[upsert_op(id, "stale", T1)]).await.unwrap();
        assert_eq!(live_count(&pool, id).await, 0);
        assert_eq!(tombstone(&pool, id).await.as_deref(), Some(T2));
        assert_eq!(log_payloads(&pool).await.len(), 3);
    }

    #[sqlx::test(migrations = "../shared/migrations")]
    async fn newer_edit_resurrects_and_clears_tombstone(pool: PgPool) {
        let id = Uuid::now_v7();
        push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
        push(&pool, &[delete_op(id, T2)]).await.unwrap();

        push(&pool, &[upsert_op(id, "newer", T3)]).await.unwrap();
        let (title, updated_at) = live_row(&pool, id).await.unwrap();
        assert_eq!(title, "newer");
        assert_eq!(updated_at, T3);
        assert_eq!(tombstone(&pool, id).await, None);
    }

    #[sqlx::test(migrations = "../shared/migrations")]
    async fn duplicate_delete_is_idempotent(pool: PgPool) {
        let id = Uuid::now_v7();
        push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
        push(&pool, &[delete_op(id, T2)]).await.unwrap();
        push(&pool, &[delete_op(id, T2)]).await.unwrap();

        assert_eq!(live_count(&pool, id).await, 0);
        assert_eq!(tombstone(&pool, id).await.as_deref(), Some(T2));
        assert_eq!(log_payloads(&pool).await.len(), 3);
    }

    #[sqlx::test(migrations = "../shared/migrations")]
    async fn pull_streams_delete_with_null_payload(pool: PgPool) {
        let id = Uuid::now_v7();
        push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
        push(&pool, &[delete_op(id, T2)]).await.unwrap();

        let (events, cursor) = pull_since(&pool, 0).await.unwrap();
        assert_eq!(cursor, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind(), OpKind::Upsert);
        assert_eq!(events[1].kind(), OpKind::Delete);
        assert_eq!(events[1].id, id);
        assert_eq!(events[1].data, serde_json::Value::Null);
    }

    #[sqlx::test(migrations = "../shared/migrations")]
    async fn snapshot_excludes_deleted_row(pool: PgPool) {
        let gone = Uuid::now_v7();
        let alive = Uuid::now_v7();
        push(
            &pool,
            &[upsert_op(gone, "gone", T1), upsert_op(alive, "alive", T1)],
        )
        .await
        .unwrap();
        push(&pool, &[delete_op(gone, T2)]).await.unwrap();

        let (_, tables, tombstones) = load_or_build_snapshot(&pool).await.unwrap();
        let notes = tables
            .iter()
            .find(|t| t.table == Table::Notes)
            .expect("notes table in snapshot");
        assert_eq!(notes.rows.len(), 1);
        assert_eq!(notes.rows[0]["title"], "alive");
        // Tombstones ride the snapshot: a far-behind client must learn
        // deletes it never replays.
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].table, Table::Notes);
        assert_eq!(tombstones[0].id, gone);
        assert_eq!(tombstones[0].deleted_at, T2);
    }
}
