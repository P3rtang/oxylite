//! The client's generic batch sink: ops → the SyncRow-generated SQL,
//! executed against PGlite. The merge contract here is the decision
//! sequence of the reference model (`shared::delete`), batched; the
//! server's per-op applier (`sync::server::apply_one`) runs the SAME
//! generated statements — one declaration feeds both.

use crate::SCHEMA;
use crate::contract::sync_row::SyncRow;
use crate::contract::table::SyncTable;
use crate::delete::{OpExt, OpKind};
use crate::engine::EngineError;
use crate::pglite::Pglite;
use crate::protocol::{Op, Tombstone};
use crate::timestamp::Timestamp;
use uuid::Uuid;

/// The final state of one pk after a batch: its newest op, as row data
/// (upsert) or a deletion timestamp.
enum FinalRow<T> {
    Upsert(T),
    Delete(Timestamp),
}

/// Apply a batch of ops (Events, or Snapshot rows synthesized as upserts).
/// Ops collapse per pk by REPLAYING the merge contract (row LWW +
/// tombstone guard) in log order — not by "last op in log order": that
/// would discard an intermediate delete's tombstone, and a later-in-log
/// but older upsert would resurrect the row (found by e2e: a cold client
/// replaying create → delete → stale edit in one pull window brought the
/// row back with the stale timestamp). The reference model
/// (`shared::delete`) is the spec; this is its decision sequence, batched.
///
/// Unparseable upserts are skipped individually — they have no valid
/// content to apply, and failing the whole chunk would drop every good
/// row around them (one poison row in a backlog used to erase a thousand
/// events). Skips are surfaced on `LAST_ERROR`, not swallowed.
pub async fn apply_ops<T>(db: &Pglite, ops: &[Op<T::Table>]) -> Result<Vec<Uuid>, EngineError>
where
    T: SyncRow + serde::de::DeserializeOwned,
{
    let mut order: Vec<Uuid> = Vec::with_capacity(ops.len());
    let mut last: std::collections::HashMap<Uuid, FinalRow<T>> =
        std::collections::HashMap::with_capacity(ops.len());
    let mut skipped: Vec<String> = Vec::new();
    for op in ops {
        match op.kind() {
            OpKind::Delete => {
                if !last.contains_key(&op.id) {
                    order.push(op.id);
                }
                let next = match last.remove(&op.id) {
                    None => FinalRow::Delete(op.updated_at),
                    Some(FinalRow::Upsert(row)) => {
                        // Deletes are writes: they must be strictly newer
                        // than the row to remove it; older-or-equal loses
                        // to the row entirely.
                        if op.updated_at.canonical_text() > row.lww_value() {
                            FinalRow::Delete(op.updated_at)
                        } else {
                            FinalRow::Upsert(row)
                        }
                    }
                    Some(FinalRow::Delete(at)) => {
                        // Concurrent deletes: only a newer timestamp moves
                        // the tombstone.
                        if op.updated_at > at {
                            FinalRow::Delete(op.updated_at)
                        } else {
                            FinalRow::Delete(at)
                        }
                    }
                };
                last.insert(op.id, next);
            }
            OpKind::Upsert => {
                match serde_json::from_value::<T>(op.data.clone()) {
                    Ok(row) => {
                        let pk = row.pk();
                        if !last.contains_key(&pk) {
                            order.push(pk);
                        }
                        let next = match last.remove(&pk) {
                            None => FinalRow::Upsert(row),
                            Some(FinalRow::Upsert(cur)) => {
                                // Row-level LWW: older row state never
                                // overwrites newer.
                                if row.lww_value() > cur.lww_value() {
                                    FinalRow::Upsert(row)
                                } else {
                                    FinalRow::Upsert(cur)
                                }
                            }
                            Some(FinalRow::Delete(at)) => {
                                // Resurrection needs a strictly newer write;
                                // equal-or-older meets the tombstone.
                                if row.lww_value() > at.canonical_text() {
                                    FinalRow::Upsert(row)
                                } else {
                                    FinalRow::Delete(at)
                                }
                            }
                        };
                        last.insert(pk, next);
                    }
                    Err(e) => skipped.push(e.to_string()),
                }
            }
        }
    }

    let mut deletes: Vec<(Uuid, Timestamp)> = Vec::new();
    let mut upserts: Vec<T> = Vec::new();
    for id in &order {
        match last.remove(id) {
            Some(FinalRow::Upsert(row)) => upserts.push(row),
            Some(FinalRow::Delete(at)) => deletes.push((*id, at)),
            None => unreachable!("order only holds pks present in `last`"),
        }
    }

    // Stay well under PGlite's host-parameter limit (the densest statement
    // here binds 2 params per row).
    let chunk_size = 4000 / 2;
    for chunk in deletes.chunks(chunk_size) {
        let params: Vec<String> = chunk
            .iter()
            .flat_map(|(id, at)| [id.to_string(), at.canonical_text()])
            .collect();
        db.query(&T::delete_sql(chunk.len()), &params)
            .await
            .map_err(EngineError::from)?;
    }

    let n_cols = T::COLUMNS.len();
    for chunk in upserts.chunks((4000 / n_cols).max(1)) {
        let params: Vec<String> = chunk.iter().flat_map(|r| r.params().into_iter()).collect();
        db.query(&T::guarded_upsert_sql(chunk.len()), &params)
            .await
            .map_err(EngineError::from)?;
        if T::LWW.is_some() {
            let pairs: Vec<String> = chunk
                .iter()
                .flat_map(|r| vec![r.pk().to_string(), r.lww_value()])
                .collect();
            db.query(&T::tombstone_clear_sql(chunk.len()), &pairs)
                .await
                .map_err(EngineError::from)?;
        }
    }

    if !skipped.is_empty() {
        let first = skipped.first().map(String::as_str).unwrap_or("?");
        crate::engine::publish_last_error::<T::Table>(EngineError::Sink(format!(
            "skipped {} unparseable payload row(s); first: {first}",
            skipped.len()
        )));
    }

    Ok(order)
}

/// Apply snapshot tombstones: upsert each one, never regressing a newer
/// local tombstone (a pending offline delete must outlive the snapshot
/// that predates it).
pub async fn apply_tombstones<T: SyncTable>(
    db: &Pglite,
    tombstones: &[Tombstone<T>],
) -> Result<(), EngineError> {
    // Group by table so the SQL can quote the name (same statement shape
    // as SyncRow::tombstone_upsert_sql, but the table comes from the row).
    let mut by_table: std::collections::HashMap<T, Vec<&Tombstone<T>>> =
        std::collections::HashMap::new();
    for t in tombstones {
        by_table.entry(t.table).or_default().push(t);
    }
    for (table, rows) in by_table {
        for chunk in rows.chunks(2000) {
            let mut sql = String::from(&format!(
                "INSERT INTO {SCHEMA}.tombstones (table_name, id, deleted_at) VALUES "
            ));
            sql.push_str(
                &chunk
                    .iter()
                    .enumerate()
                    .map(|(r, _)| format!("('{}', ${}, ${})", table.as_str(), r * 2 + 1, r * 2 + 2))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            sql.push_str(&format!(
                " ON CONFLICT (table_name, id) DO UPDATE SET deleted_at = EXCLUDED.deleted_at \
                 WHERE EXCLUDED.deleted_at > {SCHEMA}.tombstones.deleted_at",
            ));
            let params: Vec<String> = chunk
                .iter()
                .flat_map(|t| [t.id.to_string(), t.deleted_at.canonical_text()])
                .collect();
            db.query(&sql, &params).await.map_err(EngineError::from)?;
        }
    }
    Ok(())
}
