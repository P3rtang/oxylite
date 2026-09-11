//! The synced-row contract, SHARED between the sides (moved from sync,
//! #30): one `COLUMNS` list is the single source of truth for every
//! statement on both ends — the client's engine and the server's apply
//! path execute the SAME generated SQL, so a hand-written copy can never
//! drift again (that drift shipped once: a SELECT missing a column the
//! row mapping read). Pure string building — no sqlx, no js — so the
//! trait compiles in every graph; side-specific decoding lives behind
//! the crate features (see `from_row`, `note`).
//!
//! SQL is generated, not abstracted away: defaults are plain Rust
//! defaults, so anything bespoke (joined views, filtered lists) overrides
//! them or hands its own hand-written `Query` to `use_query`. Dynamic
//! dispatch stays at the `Table` enum boundary; this trait is the
//! generic side.

use crate::Table;
use uuid::Uuid;

/// The contract a synced row type fulfills. Reads are a separate
/// contract (`FromRow`, client feature) — decoding is side-specific,
/// while this trait is what both sides share.
pub trait SyncRow: Sized {
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

    /// Bind values in `COLUMNS` order (schema is text-only, so strings;
    /// the generated SQL casts each slot explicitly, which is what makes
    /// the same statement valid for PGlite's string params and for the
    /// server's runtime binds).
    fn params(&self) -> Vec<String>;

    /// The row's primary key — sync row ids are UUIDs by protocol design.
    fn pk(&self) -> Uuid;

    /// The LWW column's bind value (canonical text: the tombstone clear
    /// compares against it, and the batch collapse compares it against op
    /// timestamps — lexicographic == chronological for canonical values,
    /// which is part of this contract: an LWW bind that isn't canonical
    /// would silently mis-order the collapse).
    fn lww_value(&self) -> String {
        let idx = Self::COLUMNS
            .iter()
            .position(|c| Some(*c) == Self::LWW)
            .unwrap_or(0);
        self.params()[idx].clone()
    }

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
