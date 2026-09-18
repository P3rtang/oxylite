//! Server-side sync machinery (feature `server`): the snapshot pipeline
//! every backend of this protocol needs, generic over the app's table
//! type. Tables are defined by the implementing repo — they derive
//! `enum_iterator::Sequence`, implement [`crate::contract::table::SyncTable`] for
//! the wire names, and implement [`SnapshotSource`] for the per-table
//! extraction (which stays app-side: that's where the sqlx macros can
//! see the SQL literals, and where the exhaustive match forces new
//! tables to be handled). The `sync_log`/`snapshots` schema comes from
//! the lib's own migrations.
//!
//! Layout: [`ops`] the op pipeline (sync_log reads/writes, the
//! [`OpApply`] hook, retention, the runtime `apply_one`); [`snapshot`]
//! the bulk-load pipeline (extract, rebuild, serve); [`session`] the
//! per-connection protocol state behind a transport loop ([`crate::ws`]
//! is the axum flavor); [`wake`] the PgListener→bus adapter (2.2).

mod ops;
mod session;
mod snapshot;
mod wake;

pub use ops::{OpApply, apply_one, current_cursor, log_floor, prune_sync_log, pull_since, push};
pub use session::Session;
pub use snapshot::{
    SNAPSHOT_AFTER_OPS, SnapshotSource, SyncRowSnapshots, TableRows, load_or_build_snapshot,
    rebuild_snapshots,
};
pub use wake::{OPS_CHANNEL, spawn_wake_adapter};

use std::borrow::Cow;
use thiserror::Error;

/// One sqlx Migrator over the UNION of the lib's protocol migrations and
/// the app's `app` list — pass `oxylite::migrations!("migrations")`, the
/// SAME list the client boots with. sqlx validates every applied row
/// against its list over ONE `_sqlx_migrations` table, so lib and app
/// migrations must ride as ONE migrator (two separate ones fail
/// VersionMissing on each other's rows); the same merge the client's
/// `Pglite::init` does for PGlite, so both engines apply identical SQL
/// in identical order. Version = the NNNN stem (the macro validates the
/// shape at compile time, so the parse cannot fail). The app's own
/// `migrator()` becomes one line — the demo's hand-rolled construction
/// lived in crates/server/src/sync.rs before this existed.
pub fn migrator(app: &[(&'static str, &'static str)]) -> sqlx::migrate::Migrator {
    let migrations = crate::migrations_merged(app)
        .into_iter()
        .map(|(name, sql)| {
            // The version is the name's FULL digit run (the macro
            // validates the shape at compile time, so the parse
            // cannot fail). A fixed four-digit slice would read the
            // timestamps' first four characters — every consumer
            // migration collapsing to version 2026 (the PK
            // collision the fresh rebuild caught).
            let digits = name.chars().take_while(|c| c.is_ascii_digit()).count();
            let version: i64 = name[..digits]
                .parse()
                .expect("numeric version stem — migrations! validates the shape");
            sqlx::migrate::Migration::new(
                version,
                name.into(),
                sqlx::migrate::MigrationType::Simple,
                sql.into(),
                false,
            )
        })
        .collect();

    sqlx::migrate::Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
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
