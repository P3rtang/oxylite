//! The synced-row contract: FromRow + the SQL shape the engine needs, plus
//! the generic batch sink built on it.

use crate::engine::EngineError;
use crate::pglite::Pglite;
use crate::delete::OpExt;

use shared::timestamp::Timestamp;
use shared::{Op, Table, Tombstone};
use uuid::Uuid;

use super::from_row::FromRow;

/// The contract a synced row type fulfills. `FromRow` covers reads; the
/// consts + default methods generate the SQL the engine needs to apply
/// batches — one `COLUMNS` list is the single source of truth for every
/// statement, so hand-written SQL can never drift from the row mapping
/// again (that drift shipped once: a SELECT missing a column FromRow read).
///
/// SQL is generated, not abstracted away: defaults are plain Rust defaults,
/// so anything bespoke (joined views, filtered lists) overrides them or
/// hands its own hand-written `Query` to `use_query`. Dynamic dispatch
/// stays at the `Table` enum boundary (see `ApplyOp`); this trait is the
/// generic side.
pub trait SyncRow: FromRow + Sized {
    /// The synced table this row type belongs to.
    const TABLE: Table;
    /// Column names in bind order — feeds every generated statement.
    const COLUMNS: &'static [&'static str];
    /// Column compared for last-write-wins (`EXCLUDED.c > t.c`); `None`
    /// means batch order decides. Deletion needs this axis too: the
    /// tombstone guard compares a write's timestamp against the row's
    /// `deleted_at`, so LWW-less tables get plain upserts and deletes
    /// without a tombstone. Protocol convention: the LWW column is a
    /// `timestamptz` column (the `Timestamp` wire type's storage form) —
    /// the generated SQL binds it with an explicit `::timestamptz` cast
    /// wherever it flows through VALUES aliases.
    const LWW: Option<&'static str> = None;
    /// Primary key column name.
    const PK: &'static str = "id";

    /// Bind values in `COLUMNS` order (schema is text-only, so strings).
    fn params(&self) -> Vec<String>;

    /// The row's primary key — sync row ids are UUIDs by protocol design.
    fn pk(&self) -> Uuid;

    /// INSERT for local writes.
    fn insert_sql() -> String {
        let cols: Vec<&str> = Self::COLUMNS.to_vec();
        let slots: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            Self::TABLE.as_str(),
            cols.join(", "),
            slots.join(", ")
        )
    }

    /// SELECT of all columns, LWW-ordered newest-first when applicable.
    fn select_all_sql() -> String {
        let mut sql = format!(
            "SELECT {} FROM {}",
            Self::COLUMNS.join(", "),
            Self::TABLE.as_str()
        );
        if let Some(c) = Self::LWW {
            sql.push_str(&format!(" ORDER BY {c} DESC"));
        }
        sql
    }

    /// Multi-row LWW upsert for `n` rows: one statement, `n * columns`
    /// placeholders, `ON CONFLICT (pk) DO UPDATE` with an optional
    /// `WHERE EXCLUDED.lww > t.lww` guard. Callers bind row-major.
    fn upsert_sql(n_rows: usize) -> String {
        let cols = Self::COLUMNS;
        let mut sql = format!(
            "INSERT INTO {} ({}) VALUES ",
            Self::TABLE.as_str(),
            cols.join(", ")
        );
        let n_cols = cols.len();
        for r in 0..n_rows {
            if r > 0 {
                sql.push_str(", ");
            }
            let slots: Vec<String> = (1..=n_cols)
                .map(|c| format!("${}", r * n_cols + c))
                .collect();
            sql.push_str(&format!("({})", slots.join(", ")));
        }
        sql.push_str(&format!(" ON CONFLICT ({}) DO UPDATE SET ", Self::PK));
        let sets: Vec<String> = cols
            .iter()
            .filter(|c| **c != Self::PK)
            .map(|c| format!("{c} = EXCLUDED.{c}"))
            .collect();
        sql.push_str(&sets.join(", "));
        if let Some(lww) = Self::LWW {
            sql.push_str(&format!(
                " WHERE EXCLUDED.{lww} > {}.{}",
                Self::TABLE.as_str(),
                lww
            ));
        }
        sql
    }

    /// The guarded form of [`Self::upsert_sql`]: rows whose tombstone is
    /// newer-or-equal are filtered out before they can insert — the
    /// `ON CONFLICT` LWW guard cannot catch an absent row (no conflict =
    /// plain INSERT), so without this a stale edit pushed by a client that
    /// was offline before the delete would resurrect the row. Requires
    /// `LWW` (a delete needs a timestamp axis to lose against); falls
    /// back to the plain upsert otherwise.
    fn guarded_upsert_sql(n_rows: usize) -> String {
        let Some(lww) = Self::LWW else {
            return Self::upsert_sql(n_rows);
        };
        let cols = Self::COLUMNS;
        let n_cols = cols.len();
        let mut sql = format!(
            "INSERT INTO {} ({}) SELECT {} FROM (VALUES ",
            Self::TABLE.as_str(),
            cols.join(", "),
            cols.iter()
                .map(|c| format!("r.{c}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        for r in 0..n_rows {
            if r > 0 {
                sql.push_str(", ");
            }
            // Consumed through the `r` alias (no target-column context to
            // pin types), so cast explicitly: pk is uuid, the LWW column
            // is timestamptz (its comparisons must type against the
            // tombstone/LWW columns), everything else is text.
            let slots: Vec<String> = (1..=n_cols)
                .map(|c| {
                    let i = r * n_cols + c;
                    let col = cols[c - 1];
                    if col == Self::PK {
                        format!("${i}::uuid")
                    } else if Some(col) == Self::LWW {
                        format!("${i}::timestamptz")
                    } else {
                        format!("${i}::text")
                    }
                })
                .collect();
            sql.push_str(&format!("({})", slots.join(", ")));
        }
        sql.push_str(&format!(") AS r({})", cols.join(", ")));
        sql.push_str(&format!(
            " WHERE NOT EXISTS (SELECT 1 FROM tombstones tb \
             WHERE tb.table_name = '{}' AND tb.id = r.{} AND tb.deleted_at >= r.{lww})",
            Self::TABLE.as_str(),
            Self::PK
        ));
        sql.push_str(&format!(" ON CONFLICT ({}) DO UPDATE SET ", Self::PK));
        let sets: Vec<String> = cols
            .iter()
            .filter(|c| **c != Self::PK)
            .map(|c| format!("{c} = EXCLUDED.{c}"))
            .collect();
        sql.push_str(&sets.join(", "));
        sql.push_str(&format!(
            " WHERE EXCLUDED.{lww} > {}.{}",
            Self::TABLE.as_str(),
            lww
        ));
        sql
    }

    /// Clear tombstones that a strictly newer write resurrects: one
    /// `(pk, lww)` pair per row, bound after the upsert's own params.
    /// Rows the upsert's LWW guard rejected keep backstop semantics either
    /// way (the row is newer than any tombstone such a write could clear).
    fn tombstone_clear_sql(n_rows: usize) -> String {
        format!(
            "DELETE FROM tombstones t USING (VALUES {}) AS w(id, at) \
             WHERE t.table_name = '{}' AND t.id = w.id AND t.deleted_at < w.at",
            (0..n_rows)
                .map(|r| format!("(${}::uuid, ${}::timestamptz)", r * 2 + 1, r * 2 + 2))
                .collect::<Vec<_>>()
                .join(", "),
            Self::TABLE.as_str(),
        )
    }

    /// Delete `n` rows and tombstone only the ones actually removed, in
    /// one statement. The LWW guard (`t.{lww} < w.at`) makes a replayed
    /// or older delete lose to a newer resurrected row — no removal, and
    /// the `gone` CTE gates the tombstone so a resurrection is never
    /// re-tombstoned by a stale replay. Params row-major `(pk,
    /// deleted_at)`. LWW-less tables fall back to a plain batch delete
    /// (batch order decides, no tombstone axis).
    fn delete_sql(n_rows: usize) -> String {
        let pk = Self::PK;
        let values = |casts: bool| {
            (0..n_rows)
                .map(|r| {
                    let (i, a) = (r * 2 + 1, r * 2 + 2);
                    if casts {
                        // The delete timestamp is the LWW axis: timestamptz,
                        // so both comparisons (`t.{lww} < w.at`, the
                        // tombstone upsert's guard) type against the real
                        // columns.
                        format!("(${i}::uuid, ${a}::timestamptz)")
                    } else {
                        format!("(${i}, ${a})")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        match Self::LWW {
            Some(lww) => format!(
                "WITH gone AS ( \
                   DELETE FROM {table} AS t USING (VALUES {vals}) AS w(id, at) \
                   WHERE t.{pk} = w.id AND t.{lww} < w.at RETURNING t.{pk} \
                 ) \
                 INSERT INTO tombstones (table_name, id, deleted_at) \
                 SELECT '{table}', w.id, w.at FROM (VALUES {vals}) AS w(id, at) \
                 WHERE w.{pk} IN (SELECT {pk} FROM gone) \
                 ON CONFLICT (table_name, id) DO UPDATE \
                   SET deleted_at = EXCLUDED.deleted_at \
                 WHERE EXCLUDED.deleted_at > tombstones.deleted_at",
                table = Self::TABLE.as_str(),
                pk = pk,
                lww = lww,
                vals = values(true),
            ),
            None => format!(
                "DELETE FROM {} WHERE {} IN ({})",
                Self::TABLE.as_str(),
                pk,
                (1..=n_rows)
                    .map(|i| format!("${i}::uuid"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

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
pub async fn apply_ops<T>(db: &Pglite, ops: &[Op]) -> Result<Vec<Uuid>, EngineError>
where
    T: SyncRow + serde::de::DeserializeOwned,
{
    let mut order: Vec<Uuid> = Vec::with_capacity(ops.len());
    let mut last: std::collections::HashMap<Uuid, FinalRow<T>> =
        std::collections::HashMap::with_capacity(ops.len());
    let mut skipped: Vec<String> = Vec::new();
    for op in ops {
        match op.kind() {
            crate::delete::OpKind::Delete => {
                if !last.contains_key(&op.id) {
                    order.push(op.id);
                }
                let next = match last.remove(&op.id) {
                    None => FinalRow::Delete(op.updated_at),
                    Some(FinalRow::Upsert(row)) => {
                        // Deletes are writes: they must be strictly newer
                        // than the row to remove it; older-or-equal loses
                        // to the row entirely.
                        if op.updated_at.canonical_text() > lww_value(&row) {
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
            crate::delete::OpKind::Upsert => {
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
                                if lww_value(&row) > lww_value(&cur) {
                                    FinalRow::Upsert(row)
                                } else {
                                    FinalRow::Upsert(cur)
                                }
                            }
                            Some(FinalRow::Delete(at)) => {
                                // Resurrection needs a strictly newer write;
                                // equal-or-older meets the tombstone.
                                if lww_value(&row) > at.canonical_text() {
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
                .flat_map(|r| vec![r.pk().to_string(), lww_value(r)])
                .collect();
            db.query(&T::tombstone_clear_sql(chunk.len()), &pairs)
                .await
                .map_err(EngineError::from)?;
        }
    }

    if !skipped.is_empty() {
        let first = skipped.first().map(String::as_str).unwrap_or("?");
        crate::engine::publish_last_error(EngineError::Sink(format!(
            "skipped {} unparseable payload row(s); first: {first}",
            skipped.len()
        )));
    }

    Ok(order)
}

/// The LWW column's bind value (canonical text: the tombstone clear
/// compares against it, and the batch collapse compares it against op
/// timestamps — lexicographic == chronological for canonical values,
/// which is part of the `SyncRow` contract: an LWW bind that isn't
/// canonical would silently mis-order the collapse).
fn lww_value<T: SyncRow>(row: &T) -> String {
    let idx = T::COLUMNS
        .iter()
        .position(|c| Some(*c) == T::LWW)
        .unwrap_or(0);
    row.params()[idx].clone()
}

/// Apply snapshot tombstones: upsert each one, never regressing a newer
/// local tombstone (a pending offline delete must outlive the snapshot
/// that predates it).
pub async fn apply_tombstones(db: &Pglite, tombstones: &[Tombstone]) -> Result<(), EngineError> {
    // Group by table so the SQL can quote the name (same statement shape
    // as SyncRow::tombstone_upsert_sql, but the table comes from the row).
    let mut by_table: std::collections::HashMap<Table, Vec<&Tombstone>> =
        std::collections::HashMap::new();
    for t in tombstones {
        by_table.entry(t.table).or_default().push(t);
    }
    for (table, rows) in by_table {
        for chunk in rows.chunks(2000) {
            let mut sql =
                String::from("INSERT INTO tombstones (table_name, id, deleted_at) VALUES ");
            sql.push_str(
                &chunk
                    .iter()
                    .enumerate()
                    .map(|(r, _)| format!("('{}', ${}, ${})", table.as_str(), r * 2 + 1, r * 2 + 2))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            sql.push_str(
                " ON CONFLICT (table_name, id) DO UPDATE SET deleted_at = EXCLUDED.deleted_at \
                 WHERE EXCLUDED.deleted_at > tombstones.deleted_at",
            );
            let params: Vec<String> = chunk
                .iter()
                .flat_map(|t| [t.id.to_string(), t.deleted_at.canonical_text()])
                .collect();
            db.query(&sql, &params).await.map_err(EngineError::from)?;
        }
    }
    Ok(())
}
