//! CI guard for the client's generated SQL (`sync::query::SyncRow`
//! defaults): those statements only ever execute inside a browser's
//! PGlite, so a malformed shape surfaces as a runtime 42P01 at apply
//! time — never at compile time, and only if an e2e spec happens to hit
//! the broken path (the double-`r.` alias shipped exactly this way).
//! `PREPARE` parses and name-resolves the full statement against the
//! real schema without executing it: alias typos, unknown columns and
//! param-type mistakes fail here, for every chunk size, in seconds.
//! Complements the exact-shape pins in `sync_row.rs::tests`.
//!
//! Lives in the server crate because that's where the host Postgres test
//! harness is (`#[sqlx::test]`); the generators themselves are
//! feature-independent and compile host-side.

use shared::SyncRow;
use shared::Table;
use uuid::Uuid;

/// The notes shape with an LWW axis — every guarded/tombstone statement.
struct GuardedRow;

impl SyncRow for GuardedRow {
    type Table = Table;
    const TABLE: Table = Table::Notes;
    const COLUMNS: &'static [&'static str] = &["id", "title", "updated_at"];
    const LWW: Option<&'static str> = Some("updated_at");
    fn params(&self) -> Vec<String> {
        unimplemented!()
    }
    fn pk(&self) -> Uuid {
        unimplemented!()
    }
}

/// Same shape minus LWW — the plain-upsert and plain-delete fallbacks
/// (no tombstone axis, no guard clause).
struct PlainRow;

impl SyncRow for PlainRow {
    type Table = Table;
    const TABLE: Table = Table::Notes;
    const COLUMNS: &'static [&'static str] = &["id", "title"];
    fn params(&self) -> Vec<String> {
        unimplemented!()
    }
    fn pk(&self) -> Uuid {
        unimplemented!()
    }
}

/// `uuid` for the pk, `timestamptz` for the LWW column (its protocol
/// type — migration 0007), `text` for the rest. Only needed for
/// statements without inline casts (the INSERT path; everything with a
/// VALUES alias casts explicitly).
fn declared_types<T: SyncRow>() -> String {
    T::COLUMNS
        .iter()
        .map(|c| {
            if *c == T::PK {
                "uuid"
            } else if Some(*c) == T::LWW {
                "timestamptz"
            } else {
                "text"
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn prepare(conn: &mut sqlx::PgConnection, name: &str, types: &str, sql: &str) {
    let head = if types.is_empty() {
        format!("PREPARE {name} AS ")
    } else {
        format!("PREPARE {name} ({types}) AS ")
    };
    sqlx::query(&(head + sql))
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("PREPARE failed\nstatement: {sql}\nerror: {e}"));
}

#[sqlx::test]
async fn every_generated_client_statement_prepares(pool: sqlx::PgPool) {
    // The harness makes a fresh DB only; the UNION migrator applies
    // lib protocol tables + app tables (the app #2 pattern, #31).
    server::sync::migrator().run(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut n_stmt = 0;

    // n-independent statements.
    for (types, sql) in [
        (
            String::from("uuid, text, timestamptz"),
            GuardedRow::insert_sql(),
        ),
        (String::new(), GuardedRow::select_all_sql()),
        (String::from("uuid, text"), PlainRow::insert_sql()),
    ] {
        n_stmt += 1;
        prepare(&mut conn, &format!("s{n_stmt}"), &types, &sql).await;
    }

    // Chunked statements across a few sizes: 1, a typical batch, and the
    // real chunk cap for three columns (4000 / 3).
    for n in [1, 7, 1333] {
        for (types, sql) in [
            (declared_types::<GuardedRow>(), GuardedRow::upsert_sql(n)),
            (String::new(), GuardedRow::guarded_upsert_sql(n)),
            (String::new(), GuardedRow::tombstone_clear_sql(n)),
            (String::new(), GuardedRow::delete_sql(n)),
            (declared_types::<PlainRow>(), PlainRow::upsert_sql(n)),
            (String::new(), PlainRow::delete_sql(n)),
        ] {
            n_stmt += 1;
            prepare(&mut conn, &format!("s{n_stmt}"), &types, &sql).await;
        }
    }

    sqlx::query("DEALLOCATE ALL")
        .execute(&mut *conn)
        .await
        .unwrap();
}
