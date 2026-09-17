//! The synced-row contract, back in the lib (#31): one `COLUMNS` list is
//! the single source of truth for every statement on both ends — the
//! client's engine and the server's apply path execute the SAME
//! generated SQL, so a hand-written copy can never drift again (that
//! drift shipped once: a SELECT missing a column the row mapping read).
//! Pure string building — no sqlx, no js — so the trait compiles in
//! every graph; side-specific decoding lives behind features (see
//! `from_row`). App crates implement this for their row types (one
//! declaration feeds both appliers, #30); the lib only consumes it
//! generically and never names a row (boundary rule 1).
//!
//! SQL is generated, not abstracted away: defaults are plain Rust
//! defaults, so anything bespoke (joined views, filtered lists) overrides
//! them or hands its own hand-written `Query` to `use_query`. Dynamic
//! dispatch stays at the table/sink boundary; this trait is the generic
//! side.

use crate::SCHEMA;
use crate::contract::table::{SyncTable, SyncTableWire};
use crate::protocol::Op;
use crate::protocol::timestamp::Timestamp;
use serde::Serialize;
use uuid::Uuid;

/// The SQL type of a column, where the generated statement loses the
/// target-column context and an explicit cast is the only way to keep
/// text-bound params typing correctly (see [`SyncRow::TYPES`]). The
/// JS-side sibling is `contract::from_row::Type` — that enum describes
/// what a BRIDGE VALUE is (decode errors); this one describes what the
/// DATABASE column is (generation-time casts). `Number` cannot be the
/// same thing: JS numbers are f64, Postgres has int4/int8/float8, and
/// the counter's `count INTEGER` was the first consumer to care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    Uuid,
    Text,
    Integer,
    BigInt,
    Real,
    DoublePrecision,
    Boolean,
    Timestamptz,
}

impl SqlType {
    /// The SQL type name as it appears in an explicit cast.
    pub fn as_sql(self) -> &'static str {
        match self {
            SqlType::Uuid => "uuid",
            SqlType::Text => "text",
            SqlType::Integer => "integer",
            SqlType::BigInt => "bigint",
            SqlType::Real => "real",
            SqlType::DoublePrecision => "double precision",
            SqlType::Boolean => "boolean",
            SqlType::Timestamptz => "timestamptz",
        }
    }
}

impl std::fmt::Display for SqlType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_sql())
    }
}

// The conversions live behind `client` because their other side does:
// `from_row::Type` (the bridge-value shapes) is decode machinery, so in
// a server-only build there is no `Type` to convert to or from.
#[cfg(feature = "client")]
impl From<SqlType> for crate::contract::from_row::Type {
    /// Storage → value shape: what a bridge value of this column looks
    /// like. Total and honest — a uuid arrives as text, any Postgres
    /// number arrives as a JS number, timestamptz arrives date-shaped.
    fn from(t: SqlType) -> Self {
        match t {
            SqlType::Uuid => Self::Text,
            SqlType::Text => Self::Text,
            SqlType::Integer | SqlType::BigInt | SqlType::Real | SqlType::DoublePrecision => {
                Self::Number
            }
            SqlType::Boolean => Self::Boolean,
            SqlType::Timestamptz => Self::Date,
        }
    }
}

#[cfg(feature = "client")]
impl From<crate::contract::from_row::Type> for SqlType {
    /// Value shape → storage, BEST-EFFORT by construction: `Number`
    /// carries no precision (f64 is what it is → DoublePrecision), the
    /// date-ish family lands on the LWW axis's storage type, and the
    /// non-storage shapes (Array/Object/Function — a synced row is
    /// scalar) fall to Text. The `From` cannot fail, so it is
    /// deliberately lossy: declare [`SyncRow::TYPES`] explicitly for a
    /// real schema instead of deriving it from the decode type.
    fn from(t: crate::contract::from_row::Type) -> Self {
        match t {
            crate::contract::from_row::Type::Text => Self::Text,
            crate::contract::from_row::Type::Number => Self::DoublePrecision,
            crate::contract::from_row::Type::Boolean => Self::Boolean,
            crate::contract::from_row::Type::Date => Self::Timestamptz,
            crate::contract::from_row::Type::Array
            | crate::contract::from_row::Type::Object
            | crate::contract::from_row::Type::Function => Self::Text,
        }
    }
}

/// The contract a synced row type fulfills. Reads are a separate
/// contract (`FromRow`, client feature) — decoding is side-specific,
/// while this trait is what both sides share. The table carries the
/// wire serde (`SyncTableWire`) because the row feeds the engine's
/// generic message paths, not just SQL generation.
pub trait SyncRow: Sized {
    /// The synced table this row type belongs to (the app's own enum —
    /// the lib never names one).
    type Table: SyncTableWire;

    /// The synced table's identity, for SQL generation and
    /// invalidation.
    const TABLE: Self::Table;

    /// Column names in bind order — feeds every generated statement.
    const COLUMNS: &'static [&'static str];

    /// Column → SQL type for the slots the generated SQL loses
    /// target-column context on (the guarded upsert consumes its rows
    /// through a `VALUES` alias, where an untyped param would default
    /// to text — 42804 on the first non-text column that is not the LWW
    /// axis; the counter's `count INTEGER` was the first to hit it).
    /// The PK casts `::uuid` and the LWW column `::timestamptz`
    /// automatically; everything UNDECLARED here binds `::text`, which
    /// is the demo-era assumption and still right for a text-schema
    /// table. Declared types ride the statement as explicit casts, so
    /// one list serves both engines.
    const TYPES: &'static [(&'static str, SqlType)] = &[];

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

    /// The op this row IS — the derivable default every engine write
    /// path builds on (and composite app types compose): the table, the
    /// pk, the row's wire JSON, and the ordering axis — the row's own
    /// LWW value when the table has one, else the caller's `now` (the
    /// trait never reads a clock by ruling; the caller injects it — the
    /// engine passes its js-clock `now()`, a composite save passes the
    /// one it stamped the batch with).
    ///
    /// Panics are construction-guaranteed invariants, not data paths:
    /// the wire JSON of a plain data row cannot fail to serialize, and
    /// `lww_value()` is contract-bound to canonical text, which parses
    /// losslessly. `Self: Serialize` stays a METHOD bound, not a trait
    /// supertrait (same single-proof-path reasoning as SyncTable's — a
    /// row that doesn't ride the wire serde overrides `op` instead).
    fn op(&self, now: Timestamp) -> Op<Self::Table>
    where
        Self: Serialize,
    {
        Op {
            table: Self::TABLE,
            id: self.pk(),
            data: serde_json::to_value(self)
                .expect("row wire serialization cannot fail for a plain data row"),
            updated_at: match Self::LWW {
                Some(_) => Timestamp::parse(&self.lww_value())
                    .expect("canonical LWW text round-trips into a Timestamp"),
                None => now,
            },
        }
    }

    /// The write couple: emit this row's op AND its upsert step in ONE
    /// call — the shape of the statement and the op it rides are
    /// structurally inseparable, so a composite save composing these
    /// (a base table plus its 1-to-many children) cannot forget an op
    /// per row. The engine's `upsert` is built on this; the ops
    /// accumulator is the caller's so several rows compose into one
    /// batch. `Self: Serialize` is a method bound for the same reason
    /// as `op`.
    fn upsert_with_ops(
        &self,
        now: Timestamp,
        ops: &mut Vec<Op<Self::Table>>,
    ) -> (String, Vec<String>)
    where
        Self: Serialize,
    {
        ops.push(self.op(now));
        (Self::guarded_upsert_sql(1), self.params())
    }

    /// The delete half of the couple — an associated function because
    /// there is no row to derive from: the op is the null payload
    /// (tombstone marker) stamped with the caller's clock, the step is
    /// the table's guarded delete bound `(pk, deleted_at)`.
    fn delete_with_ops(
        id: Uuid,
        now: Timestamp,
        ops: &mut Vec<Op<Self::Table>>,
    ) -> (String, Vec<String>) {
        ops.push(Op {
            table: Self::TABLE,
            id,
            data: serde_json::Value::Null,
            updated_at: now,
        });
        (
            Self::delete_sql(1),
            vec![id.to_string(), now.canonical_text()],
        )
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
            // tombstone/LWW columns), a declared TYPES entry casts to its
            // SQL type, everything else is text.
            let slots: Vec<String> = (1..=n_cols)
                .map(|c| {
                    let i = r * n_cols + c;
                    let col = cols[c - 1];
                    if col == Self::PK {
                        format!("${i}::uuid")
                    } else if Some(col) == Self::LWW {
                        format!("${i}::timestamptz")
                    } else if let Some((_, sql_type)) =
                        Self::TYPES.iter().find(|(name, _)| col == *name)
                    {
                        format!("${i}::{sql_type}")
                    } else {
                        format!("${i}::text")
                    }
                })
                .collect();
            sql.push_str(&format!("({})", slots.join(", ")));
        }
        sql.push_str(&format!(") AS r({})", cols.join(", ")));
        sql.push_str(&format!(
            " WHERE NOT EXISTS (SELECT 1 FROM {SCHEMA}.tombstones tb \
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
            "DELETE FROM {SCHEMA}.tombstones t USING (VALUES {}) AS w(id, at) \
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
                 INSERT INTO {SCHEMA}.tombstones (table_name, id, deleted_at) \
                 SELECT '{table}', w.id, w.at FROM (VALUES {vals}) AS w(id, at) \
                 WHERE w.{pk} IN (SELECT {pk} FROM gone) \
                 ON CONFLICT (table_name, id) DO UPDATE \
                   SET deleted_at = EXCLUDED.deleted_at \
                 WHERE EXCLUDED.deleted_at > {SCHEMA}.tombstones.deleted_at",
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
