//! The counter example's data crate — the lib/app seam in miniature,
//! mirroring the demo's `shared` (crates/shared). The protocol (wire
//! DTOs, Timestamp, SyncRow, SyncTable, FromRow) is the LIB's; this
//! crate is what the APP owns: the table enum, the row type, and the
//! migration files of the app's replicated tables. Dependency arrow:
//! counter-shared → oxylite (the published one).

pub use oxylite::{SyncRow, SyncTable, Timestamp, TimestampError};

use enum_iterator::Sequence;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The app's own migrations, embedded by the standard's macro — the
/// dir lives at the PROJECT ROOT (defaults: the CLI's provision/apply
/// looks there first), the macro call resolves it relative to THIS
/// crate's manifest (`../`). Self-hosted in the shared crate so
/// client and server compile-side share the one source; the lib's
/// protocol migrations merge in at the boot/migrator entry points —
/// the consumer never names them.
pub static APP_MIGRATIONS: &[(&str, &str)] = oxylite::migrations!("../migrations");

/// The app's SCHEMA version: the crate version itself (one source of
/// truth, demo-ruled). Bump discipline: MAJOR = breaking shipped
/// schema/protocol, MINOR = additive (stale tabs reload), PATCH = free.
pub const SCHEMA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tables that participate in sync — exhaustive on purpose: the
/// compiler forces every match site (apply, invalidate, subscribe) to
/// handle new tables, and the wire name stays stable via `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CounterTable {
    Counters,
}

impl oxylite::SyncTable for CounterTable {
    fn as_str(self) -> &'static str {
        "counters"
    }

    /// Inverse of `as_str`; unknown names (newer tables) are skipped by
    /// replaying sites — this side is the compat boundary.
    fn from_name(name: &str) -> Option<Self> {
        (name == "counters").then_some(Self::Counters)
    }
}

// ---- concrete protocol aliases (this app's instantiation) ----

pub type Op = oxylite::protocol::Op<CounterTable>;
pub type ClientMsg = oxylite::protocol::ClientMsg<CounterTable>;
pub type ServerMsg = oxylite::protocol::ServerMsg<CounterTable>;
pub type TableData = oxylite::protocol::TableData<CounterTable>;
pub type Tombstone = oxylite::protocol::Tombstone<CounterTable>;

/// One named counter — now a SYNCED row: it carries the LWW axis
/// (`updated_at`) because an LWW-less table cannot delete safely across
/// clients (batch order decides; the tombstone machinery needs a
/// timestamp to lose against). v7 ids keep rows creation-ordered on
/// both engines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counter {
    pub id: Uuid,
    pub name: String,
    pub count: i64,
    pub updated_at: Timestamp,
}

impl SyncRow for Counter {
    type Table = CounterTable;

    const TABLE: CounterTable = CounterTable::Counters;
    const COLUMNS: &'static [&'static str] = &["id", "name", "count", "updated_at"];
    const LWW: Option<&'static str> = Some("updated_at");
    const PK: &'static str = "id";
    // `count` is the first non-text column outside the LWW axis — the
    // guarded upsert's VALUES-alias form loses the target-column
    // context, so the type is declared here (42804 without it).
    const TYPES: &'static [(&'static str, oxylite::SqlType)] =
        &[("count", oxylite::SqlType::Integer)];

    fn params(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            self.name.clone(),
            self.count.to_string(),
            self.updated_at.canonical_text(),
        ]
    }

    fn pk(&self) -> Uuid {
        self.id
    }
}

#[cfg(feature = "client")]
mod client {
    //! Rows come back as JS objects; Counter knows how to build itself.

    use super::Counter;
    use oxylite::contract::from_row::{FromRow, Row, RowError};
    use oxylite::protocol::timestamp::Timestamp;

    impl FromRow for Counter {
        fn from_row(row: &Row) -> Result<Self, RowError> {
            // All four columns are NOT NULL — the required form reads
            // cleanest (SQL NULL is its own error, never a fabricated
            // "missing column").
            Ok(Counter {
                id: row
                    .field_req::<String>("id")?
                    .parse()
                    .map_err(|_| RowError::BadUuid("id".into()))?,
                name: row.field_req("name")?,
                count: row.field_req::<f64>("count")? as i64,
                updated_at: Timestamp::parse(row.field_req::<String>("updated_at")?.as_str())
                    .map_err(|e| RowError::BadTimestamp("updated_at".into(), e))?,
            })
        }
    }
}

#[cfg(feature = "server")]
mod server {
    //! sqlx's own `FromRow` so the server can `query_as::<Counter>` —
    //! the LWW column is `timestamptz`, so it decodes as a real DateTime
    //! and goes through the infallible constructor (the storage ring did
    //! the validating).

    use super::Counter;
    use oxylite::protocol::timestamp::Timestamp;

    impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Counter {
        fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
            use sqlx::Row as _;
            Ok(Counter {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                count: row.try_get("count")?,
                updated_at: Timestamp::from_datetime(row.try_get("updated_at")?),
            })
        }
    }
}
