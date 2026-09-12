//! The op pipeline: everything that reads and writes `oxylite.sync_log`
//! (the lib's own table — protocol machinery, not app code), the
//! app-side apply hook ([`OpApply`]), the runtime `apply_one` for
//! `SyncRow` tables, and the retention operation. The snapshot pipeline
//! lives in [`super::snapshot`], the per-connection decisions in
//! [`super::session`].

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnection, PgPool};
use std::future::Future;

use super::SyncError;
use crate::contract::sync_row::SyncRow;
use crate::contract::table::SyncTable;
use crate::delete::{OpExt, OpKind};
use crate::protocol::Op;
use crate::timestamp::Timestamp;

/// The server's cursor: the newest seq in the sync log.
pub async fn current_cursor(db: &PgPool) -> i64 {
    sqlx::query_scalar!(r#"SELECT COALESCE(MAX(seq), 0) as "cursor!" FROM oxylite.sync_log"#)
        .fetch_one(db)
        .await
        .unwrap_or(0)
}

/// The log's surviving floor: the earliest seq a replay could start
/// from (`None` on an empty log). The pruning gate reads it (#35).
pub async fn log_floor(db: &PgPool) -> Result<Option<i64>, SyncError> {
    let floor: Option<i64> =
        sqlx::query_scalar!(r#"SELECT MIN(seq) as "min" FROM oxylite.sync_log"#)
            .fetch_one(db)
            .await?;
    Ok(floor)
}

/// Events after `since`, windowed, with the seq the batch reaches.
/// Protocol machinery, not app code: this touches only `sync_log` —
/// the log's schema is the lib's own (lib migrations), so the whole
/// read path is generic. Only the apply path is per-table.
pub async fn pull_since<T: SyncTable>(
    db: &PgPool,
    since: i64,
) -> Result<(Vec<Op<T>>, i64), SyncError> {
    let head: i64 =
        sqlx::query_scalar!(r#"SELECT COALESCE(MAX(seq), 0) as "head!" FROM oxylite.sync_log"#,)
            .fetch_one(db)
            .await?;

    if since >= head {
        return Ok((Vec::new(), head));
    }

    let rows = sqlx::query!(
        "SELECT seq, table_name, row_id, payload, updated_at
         FROM oxylite.sync_log WHERE seq > $1 ORDER BY seq LIMIT 1000",
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
        let Some(table) = T::from_name(&row.table_name) else {
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

/// Per-table op application, implemented app-side where the checked SQL
/// lives — the [`SnapshotSource`](super::snapshot::SnapshotSource)
/// pattern for the push path. The impl's match on kind + table is the
/// compile guarantee: exhaustive, so a new variant breaks the build
/// until its arm exists.
///
/// The future is `Send` on purpose (same as
/// [`SnapshotSource`](super::snapshot::SnapshotSource)): the push path
/// must stay `tokio::spawn`-able; impls use plain `async fn` and are
/// checked at the impl.
pub trait OpApply<T: SyncTable> {
    /// Apply one op to the app's live tables, inside `push`'s
    /// transaction. `push` logs the op AFTER this returns, whatever the
    /// apply decided (dropped guards included): the log is the wire
    /// history, guards decide at apply time.
    fn apply(
        &self,
        op: &Op<T>,
        tx: &mut PgConnection,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;
}

/// Apply each op to the live tables — via the app's [`OpApply`] impl for
/// the per-table arms, then append it to the sync log (lib-owned: the
/// log is generic). A delete is an op like any other — `{table, id,
/// updated_at, data: null}` — and is logged whatever the apply decided:
/// the log is the wire history, guards decide at apply time.
pub async fn push<T: SyncTable, A: OpApply<T>>(
    db: &PgPool,
    ops: &[Op<T>],
    applier: &A,
) -> Result<i64, SyncError> {
    let mut tx = db.begin().await?;

    for op in ops {
        // An op the CURRENT schema cannot apply is quarantined (roadmap
        // 3.3): the batch commits, the sender is never wedged (a rollback
        // would leave the op unacked, resending forever, client blind),
        // and the error surfaces through the sender's own echo. The op
        // does NOT enter sync_log — a payload no compatible client can
        // apply is not replayable history; #34's floor narrows to "the
        // log is the APPLYABLE history".
        if let Err(e) = applier.apply(op, &mut tx).await {
            eprintln!("quarantined op ({} {}): {e}", op.table.as_str(), op.id);
            sqlx::query!(
                "INSERT INTO oxylite.quarantine
                 (table_name, row_id, payload, updated_at, error)
                 VALUES ($1, $2, $3, $4, $5)",
                op.table.as_str(),
                op.id,
                op.data,
                op.updated_at.as_datetime(),
                e.to_string(),
            )
            .execute(&mut *tx)
            .await?;
            continue;
        }

        sqlx::query!(
            "INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)
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
        r#"SELECT COALESCE(MAX(seq), 0) as "cursor!" FROM oxylite.sync_log"#,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(cursor)
}

/// Apply one op to a SyncRow table on the host. The payload parses into
/// the caller's row type and the statements are the SAME SyncRow
/// defaults the client's engine executes — one declaration (`impl
/// SyncRow for ...` in the app) feeds both sides, so a hand-written
/// server copy can never drift (reviewer goal, #30: a new table is one
/// row impl + one match arm). Runtime API on purpose: the SQL is
/// generated per table (a checked macro could never be generic over the
/// mapping), and the shapes are PREPARE-checked against the real schema
/// in CI (`server/tests/generated_sql_prepares.rs`) — the same guard the
/// generated client statements already had. The checked macros remain
/// for the generic protocol queries (sync_log, snapshots storage).
///
/// Binds are the row's own string params; the generated casts
/// (`$N::uuid`, `$N::timestamptz`) type them against the real columns —
/// identical to how PGlite binds them.
pub async fn apply_one<T>(op: &Op<T::Table>, tx: &mut PgConnection) -> Result<(), SyncError>
where
    T: SyncRow + serde::de::DeserializeOwned,
{
    match op.kind() {
        OpKind::Delete => {
            // The delete needs only the table name and the LWW axis —
            // both live in the row mapping; the payload is null.
            let params = vec![op.id.to_string(), op.updated_at.canonical_text()];
            exec_binds(&T::delete_sql(1), params, tx).await?;
        }
        OpKind::Upsert => {
            let row: T = serde_json::from_value(op.data.clone())?;
            exec_binds(&T::guarded_upsert_sql(1), row.params(), tx).await?;
            if T::LWW.is_some() {
                // A strictly newer write resurrects: clear the tombstone.
                let pairs = vec![row.pk().to_string(), row.lww_value()];
                exec_binds(&T::tombstone_clear_sql(1), pairs, tx).await?;
            }
        }
    }
    Ok(())
}

async fn exec_binds(
    sql: &str,
    params: Vec<String>,
    tx: &mut PgConnection,
) -> Result<(), SyncError> {
    let mut q = sqlx::query(sql);
    for p in params {
        q = q.bind(p);
    }
    q.execute(tx).await?;
    Ok(())
}

/// Delete log rows older than `cutoff` (server-arrival `logged_at`).
/// The pure retention operation — mechanism-free by design: NO scheduler
/// lives in this codebase. A cron task, an event-bus consumer, an ops
/// script, or pg_cron wires it in with one call (reviewer, #35:
/// age-based pruning only "when we have an event bus or a cron task
/// system"). Safe against any client age: `pull_since` clamps to the
/// surviving floor, so a pruned-behind client replays overlap instead
/// of silently skipping the pruned stretch.
pub async fn prune_sync_log(db: &PgPool, cutoff: DateTime<Utc>) -> Result<u64, SyncError> {
    let result = sqlx::query!("DELETE FROM oxylite.sync_log WHERE logged_at < $1", cutoff)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}
