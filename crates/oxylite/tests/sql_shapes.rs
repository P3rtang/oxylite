//! SQL-shape pins for the generated statements (`SyncRow` defaults).
//! Unit tests can't execute them against a real database on either side
//! (the client's runtime is PGlite in a browser), so their exact shape
//! is asserted here — a malformed statement (the double-`r.` alias that
//! once shipped) only surfaces as a runtime 42P01, never at compile
//! time. Semantic validity (parse, name resolution, param types) is
//! covered by the prepare-checks in
//! `crates/server/tests/generated_sql_prepares.rs`.
//!
//! Runs against a LOCAL fixture row (#31): the lib no longer depends on
//! the app's `shared` crate — a consuming app's row impl is exactly
//! this shape, and the pins travel with the lib.

use enum_iterator::Sequence;
use oxylite::sync_row::SyncRow;
use oxylite::table::SyncTable;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The consuming app's table enum, fixture flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FixtureTable {
    Notes,
}

impl SyncTable for FixtureTable {
    fn as_str(self) -> &'static str {
        "notes"
    }

    fn from_name(name: &str) -> Option<Self> {
        (name == "notes").then_some(Self::Notes)
    }
}

struct TestNote {
    id: Uuid,
    title: String,
    updated_at: String,
}

impl SyncRow for TestNote {
    type Table = FixtureTable;
    const TABLE: FixtureTable = FixtureTable::Notes;
    const COLUMNS: &'static [&'static str] = &["id", "title", "updated_at"];
    const LWW: Option<&'static str> = Some("updated_at");

    fn params(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            self.title.clone(),
            self.updated_at.clone(),
        ]
    }

    fn pk(&self) -> Uuid {
        self.id
    }
}

#[test]
fn guarded_upsert_selects_aliased_columns_once() {
    let sql = TestNote::guarded_upsert_sql(2);
    // Every column is prefixed exactly once: `r.<col>`, never `r.r.`.
    assert!(
        sql.contains(
            "INSERT INTO notes (id, title, updated_at) \
             SELECT r.id, r.title, r.updated_at FROM (VALUES "
        ),
        "bad select list: {sql}"
    );
    assert!(sql.contains(") AS r(id, title, updated_at)"));
    assert!(sql.contains("WHERE NOT EXISTS (SELECT 1 FROM tombstones tb"));
    assert!(sql.contains("tb.id = r.id AND tb.deleted_at >= r.updated_at"));
    assert!(sql.contains("WHERE EXCLUDED.updated_at > notes.updated_at"));
    assert!(!sql.contains("r.r."), "double alias prefix: {sql}");
    // PK cast to uuid, LWW to timestamptz, the rest to text; row-major params.
    assert!(sql.contains("($1::uuid, $2::text, $3::timestamptz)"));
    assert!(sql.contains("($4::uuid, $5::text, $6::timestamptz)"));
}

#[test]
fn tombstone_clear_binds_pk_and_lww_pairs() {
    let sql = TestNote::tombstone_clear_sql(2);
    assert!(sql.starts_with("DELETE FROM tombstones t USING (VALUES "));
    assert!(sql.contains("($1::uuid, $2::timestamptz), ($3::uuid, $4::timestamptz)"));
    assert!(sql.contains("t.id = w.id AND t.deleted_at < w.at"));
}

#[test]
fn delete_sql_tombstones_only_what_it_removed() {
    let sql = TestNote::delete_sql(1);
    assert!(sql.starts_with("WITH gone AS ("));
    assert!(sql.contains(
        "DELETE FROM notes AS t USING (VALUES ($1::uuid, $2::timestamptz)) AS w(id, at)"
    ));
    assert!(sql.contains("WHERE t.id = w.id AND t.updated_at < w.at RETURNING t.id"));
    assert!(sql.contains("WHERE w.id IN (SELECT id FROM gone)"));
    assert!(sql.contains("ON CONFLICT (table_name, id) DO UPDATE"));
    assert!(sql.contains("WHERE EXCLUDED.deleted_at > tombstones.deleted_at"));
}

#[test]
fn delete_sql_without_lww_has_no_tombstone() {
    struct Plain;
    impl SyncRow for Plain {
        type Table = FixtureTable;
        const TABLE: FixtureTable = FixtureTable::Notes;
        const COLUMNS: &'static [&'static str] = &["id", "title"];
        fn params(&self) -> Vec<String> {
            unimplemented!()
        }
        fn pk(&self) -> Uuid {
            unimplemented!()
        }
    }
    let sql = Plain::delete_sql(2);
    assert!(sql.starts_with("DELETE FROM notes WHERE id IN ($1::uuid, $2::uuid)"));
    assert!(!sql.contains("tombstones"));
}
