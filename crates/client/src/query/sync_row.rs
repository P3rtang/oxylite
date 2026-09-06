//! The synced-row contract: FromRow + the SQL shape the engine needs, plus
//! the generic batch applier built on it.

use crate::pglite::Pglite;
use shared::Table;
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
    /// means batch order decides.
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
}

/// Apply a batch of payload rows (Events or a Snapshot): parse, dedup to
/// the last row per PK (log order is the LWW tiebreak), then one multi-row
/// upsert per chunk. A fresh IndexedDB must not pay one round-trip per row.
pub(crate) async fn bulk_upsert<T>(
    db: &Pglite,
    rows: &[serde_json::Value],
) -> Result<Vec<Uuid>, String>
where
    T: SyncRow + serde::de::DeserializeOwned,
{
    let mut order: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut by_pk: std::collections::HashMap<Uuid, T> =
        std::collections::HashMap::with_capacity(rows.len());
    for v in rows {
        let row: T = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if !by_pk.contains_key(&row.pk()) {
            order.push(row.pk());
        }
        // Same PK twice in one batch: the later log entry wins.
        by_pk.insert(row.pk(), row);
    }

    // Stay well under PGlite's host-parameter limit.
    let chunk_size = (4000 / T::COLUMNS.len()).max(1);
    let items: Vec<T> = order.iter().map(|pk| by_pk.remove(pk).unwrap()).collect();
    for chunk in items.chunks(chunk_size) {
        let params: Vec<String> = chunk.iter().flat_map(|r| r.params().into_iter()).collect();
        db.query(&T::upsert_sql(chunk.len()), &params)
            .await
            .map_err(|e| crate::engine::error_text(&e))?;
    }

    Ok(order)
}
