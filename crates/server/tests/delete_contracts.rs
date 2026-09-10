//! Delete contracts (#27, ROADMAP 1.2), pinned server-side against real
//! Postgres: a delete is one op `{table, id, updated_at, data: null}`;
//! the live row goes away, a tombstone guards against stale writes, and
//! the op itself stays in sync_log (guards decide at apply time).
//! `#[sqlx::test]` makes a fresh database per test and applies the
//! shared migrations. Helpers use the runtime (non-macro) query API —
//! the checked macros live in the library, and keeping this file
//! macro-free means no `.sqlx` entries depend on a test target.

use server::sync::{load_or_build_snapshot, pull_since, push};
use shared::delete::OpKind;
use shared::timestamp::Timestamp;
use shared::{Op, Table};
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

/// Parse in the helper: fixtures validate like the wire does.
fn ts(s: &str) -> Timestamp {
    Timestamp::parse(s).unwrap()
}

// Canonical ISO, uniform precision — lexicographic == chronological.
const T1: &str = "2026-09-09T10:00:01.000Z";
const T2: &str = "2026-09-09T10:00:02.000Z";
const T3: &str = "2026-09-09T10:00:03.000Z";

fn upsert_op(id: Uuid, title: &str, at: &str) -> Op {
    Op {
        table: Table::Notes,
        id,
        data: serde_json::json!({ "id": id, "title": title, "body": "", "updated_at": at }),
        updated_at: ts(at),
    }
}

fn delete_op(id: Uuid, at: &str) -> Op {
    Op {
        table: Table::Notes,
        id,
        data: serde_json::Value::Null,
        updated_at: ts(at),
    }
}

async fn live_count(db: &sqlx::PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*)::bigint FROM notes WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn live_row(db: &sqlx::PgPool, id: Uuid) -> Option<(String, DateTime<Utc>)> {
    sqlx::query_as::<_, (String, DateTime<Utc>)>(
        "SELECT title, updated_at FROM notes WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .unwrap()
}

/// The row's tombstone, if any: deleted_at (timestamptz — a real
/// DateTime comes back; the storage layer did the validating).
async fn tombstone(db: &sqlx::PgPool, id: Uuid) -> Option<DateTime<Utc>> {
    sqlx::query_scalar("SELECT deleted_at FROM tombstones WHERE table_name = 'notes' AND id = $1")
        .bind(id)
        .fetch_optional(db)
        .await
        .unwrap()
}

async fn log_payloads(db: &sqlx::PgPool) -> Vec<serde_json::Value> {
    sqlx::query_scalar("SELECT payload FROM sync_log ORDER BY seq")
        .fetch_all(db)
        .await
        .unwrap()
}

async fn log_count(db: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*)::bigint FROM sync_log")
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../shared/migrations")]
async fn malformed_payload_is_rejected_and_logs_nothing(pool: sqlx::PgPool) {
    // The push door. A malformed ENVELOPE timestamp can no longer be
    // built in Rust at all — `Timestamp` forbids it, and serde validates
    // the WS message before `push` is ever called (pinned in
    // shared/tests/timestamp.rs). The reachable case is a malformed
    // PAYLOAD: Note's serde validation fails it, and nothing may be
    // logged or applied — a bad timestamp in sync_log would silently
    // mis-order every later LWW/tombstone comparison.
    let id = Uuid::now_v7();
    let mut op = upsert_op(id, "poison", T1);
    op.data = serde_json::json!({
        "id": id, "title": "poison", "body": "",
        "updated_at": "definitely not a time",
    });

    let err = push(&pool, &[op]).await.unwrap_err();
    assert!(
        matches!(err, server::sync::SyncError::Json(_)),
        "err: {err}"
    );
    assert!(
        err.to_string().contains("definitely not a time"),
        "err: {err}"
    );
    assert_eq!(log_count(&pool).await, 0);
    assert_eq!(live_count(&pool, id).await, 0);
}

#[sqlx::test(migrations = "../shared/migrations")]
async fn push_delete_removes_row_and_tombstones(pool: sqlx::PgPool) {
    let id = Uuid::now_v7();
    push(&pool, &[upsert_op(id, "alive", T1)]).await.unwrap();
    assert_eq!(live_count(&pool, id).await, 1);

    push(&pool, &[delete_op(id, T2)]).await.unwrap();
    assert_eq!(live_count(&pool, id).await, 0);
    assert_eq!(tombstone(&pool, id).await, Some(ts(T2).as_datetime()));
    // The op itself is logged (payload null), so other clients replay it.
    let payloads = log_payloads(&pool).await;
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[1], serde_json::Value::Null);
}

#[sqlx::test(migrations = "../shared/migrations")]
async fn stale_edit_dropped_live_table_keeps_delete(pool: sqlx::PgPool) {
    let id = Uuid::now_v7();
    push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
    push(&pool, &[delete_op(id, T2)]).await.unwrap();

    // A client offline before the delete pushes its mid edit: the
    // live table must not resurrect; the op is still logged and the
    // tombstone stands.
    push(&pool, &[upsert_op(id, "stale", T1)]).await.unwrap();
    assert_eq!(live_count(&pool, id).await, 0);
    assert_eq!(tombstone(&pool, id).await, Some(ts(T2).as_datetime()));
    assert_eq!(log_payloads(&pool).await.len(), 3);
}

#[sqlx::test(migrations = "../shared/migrations")]
async fn newer_edit_resurrects_and_clears_tombstone(pool: sqlx::PgPool) {
    let id = Uuid::now_v7();
    push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
    push(&pool, &[delete_op(id, T2)]).await.unwrap();

    push(&pool, &[upsert_op(id, "newer", T3)]).await.unwrap();
    let (title, updated_at) = live_row(&pool, id).await.unwrap();
    assert_eq!(title, "newer");
    assert_eq!(updated_at, ts(T3).as_datetime());
    assert_eq!(tombstone(&pool, id).await, None);
}

#[sqlx::test(migrations = "../shared/migrations")]
async fn duplicate_delete_is_idempotent(pool: sqlx::PgPool) {
    let id = Uuid::now_v7();
    push(&pool, &[upsert_op(id, "v1", T1)]).await.unwrap();
    push(&pool, &[delete_op(id, T2)]).await.unwrap();
    push(&pool, &[delete_op(id, T2)]).await.unwrap();

    assert_eq!(live_count(&pool, id).await, 0);
    assert_eq!(tombstone(&pool, id).await, Some(ts(T2).as_datetime()));
    assert_eq!(log_payloads(&pool).await.len(), 3);
}

#[sqlx::test(migrations = "../shared/migrations")]
async fn pull_streams_delete_with_null_payload(pool: sqlx::PgPool) {
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
async fn snapshot_excludes_deleted_row(pool: sqlx::PgPool) {
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
    assert_eq!(tombstones[0].deleted_at.canonical_text(), T2);
}
