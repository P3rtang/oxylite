//! Server-side sync machinery (feature `server`): the snapshot pipeline
//! every backend of this protocol needs, generic over the app's table
//! type. Tables are defined by the implementing repo — they derive
//! `enum_iterator::Sequence`, implement [`SyncTable`] for the wire names,
//! and implement [`SnapshotSource`] for the per-table extraction (which
//! stays app-side: that's where the sqlx macros can see the SQL literals,
//! and where the exhaustive match forces new tables to be handled).
//! The `sync_log`/`snapshots` schema comes from the lib's own migrations.

use enum_iterator::all;
use sqlx::postgres::{PgConnection, PgPool};
use std::future::Future;
use std::marker::PhantomData;
use thiserror::Error;

use crate::delete::{OpExt, OpKind};
use crate::protocol::{ClientMsg, Op, ServerMsg, TableData, Tombstone};
use crate::sync_row::SyncRow;
use crate::table::{SyncTable, SyncTableWire};
use crate::timestamp::Timestamp;

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
    sqlx::query_scalar!(r#"SELECT COALESCE(MAX(seq), 0) as "cursor!" FROM oxylite.sync_log"#)
        .fetch_one(db)
        .await
        .unwrap_or(0)
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

/// Why a server sync operation failed. Typed so callers match on the shape
/// of the failure; sources convert with `#[from]` (errors-spec shape —
/// moved here with `pull_since`/`push`, which own these failure kinds).
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
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
/// lives — the [`SnapshotSource`] pattern for the push path. The impl's
/// match on kind + table is the compile guarantee: exhaustive, so a new
/// variant breaks the build until its arm exists.
///
/// The future is `Send` on purpose (same as [`SnapshotSource`]): the
/// push path must stay `tokio::spawn`-able; impls use plain `async fn`
/// and are checked at the impl.
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
        applier.apply(op, &mut tx).await?;

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

/// Full state for a far-behind client, wire-shaped: the generic
/// [`load_or_build_snapshot`] pipeline mapped onto the protocol's own
/// DTOs — now generic (#31), so this maps directly with no per-app glue.
async fn snapshot_msg<T: SyncTable, S: SnapshotSource<T>>(
    db: &PgPool,
    source: &S,
) -> Result<ServerMsg<T>, SyncError> {
    let (seq, tables, tombstones) = load_or_build_snapshot::<T, S>(db, source).await?;
    Ok(ServerMsg::Snapshot {
        seq,
        tables: tables
            .into_iter()
            .map(|rows| TableData {
                table: rows.table,
                rows: rows.rows,
            })
            .collect(),
        tombstones,
    })
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

/// Per-connection protocol decisions, transport-free: everything a
/// backend's WS loop needs EXCEPT the sockets. Holds the app's plug-ins
/// (apply arms + snapshot extraction) at construction, so the transport
/// loop stays a thin `select!` over [`Session::on_text`] and its ticker.
pub struct Session<T: SyncTableWire, A: OpApply<T>, S: SnapshotSource<T>> {
    applier: A,
    source: S,
    // One snapshot per connection, max: a follow-up Pull replays events
    // instead, otherwise a stale snapshot and the backlog would ping-pong.
    snapshotted: bool,
    // `T` appears only in the trait bounds — the session speaks
    // `ServerMsg<T>` — so the type parameter is pinned with a marker.
    _table: PhantomData<T>,
}

impl<T: SyncTableWire, A: OpApply<T>, S: SnapshotSource<T>> Session<T, A, S> {
    pub fn new(applier: A, source: S) -> Self {
        Self {
            applier,
            source,
            snapshotted: false,
            _table: PhantomData,
        }
    }

    /// The whole client-message dispatch — parse, Push→Ack, Pull→
    /// snapshot-or-replay. `Ok(None)` means nothing to send; the
    /// transport loop logs `Err` and keeps the connection (one poisoned
    /// message must not kill the stream — at-least-once makes skipping
    /// harmless).
    pub async fn on_text(
        &mut self,
        db: &PgPool,
        text: &str,
    ) -> Result<Option<ServerMsg<T>>, SyncError> {
        match serde_json::from_str::<ClientMsg<T>>(text)? {
            ClientMsg::Push { ops, batch } => {
                let cursor = push(db, &ops, &self.applier).await?;
                Ok(Some(ServerMsg::Ack { cursor, batch }))
            }
            ClientMsg::Pull { since } => {
                // Far behind? Replay would be one upsert per logged op —
                // hand over a snapshot instead (once per connection; a
                // follow-up Pull replays events, so snapshot and backlog
                // can't ping-pong).
                let head = current_cursor(db).await;
                if head - since > SNAPSHOT_AFTER_OPS && !self.snapshotted {
                    self.snapshotted = true;
                    Ok(Some(snapshot_msg(db, &self.source).await?))
                } else {
                    let (events, cursor) = pull_since(db, since).await?;
                    Ok(Some(ServerMsg::Events { events, cursor }))
                }
            }
        }
    }
}
