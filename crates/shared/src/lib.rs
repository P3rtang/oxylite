//! The notes app's data crate — the lib/app seam (#31). The protocol
//! (wire DTOs, `Timestamp`, `SyncRow`, `SyncTable`, `FromRow`) is the
//! LIB's now (`sync`); this crate is what the APP owns: the table enum,
//! the row type, and the migration files of the app's replicated
//! tables. The dependency arrow points the right way: shared → sync.
//!
//! The concrete aliases (`Op`, `ClientMsg`, …) pin the protocol
//! generics to THIS app's table enum, so app call sites stay
//! unchanged — `shared::Op` is `sync::Op<Table>`.

pub mod note;

pub use note::Note;
pub use oxylite::{SyncRow, SyncTable, Timestamp, TimestampError};

use enum_iterator::Sequence;
use serde::{Deserialize, Serialize};

/// Tables that participate in sync. Exhaustive on purpose: the compiler
/// forces every match site (apply, invalidate, subscribe) to handle new
/// tables, and methods can live here. The wire name stays stable via
/// `as_str` (that's what sync_log's `table_name` column stores).
/// Iteration over all variants is derived (`Sequence`), so new variants
/// join `enum_iterator::all::<Table>()` automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Table {
    Notes,
}

impl SyncTable for Table {
    fn as_str(self) -> &'static str {
        match self {
            Self::Notes => "notes",
        }
    }

    /// Inverse of `as_str`; used when replaying sync_log rows. Unknown
    /// names (newer table from a newer client) are skipped by callers.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "notes" => Some(Self::Notes),
            _ => None,
        }
    }
}

// ---- concrete protocol aliases (this app's instantiation) ----

/// An operation the client pushes, or the server replays from sync_log.
pub type Op = oxylite::protocol::Op<Table>;

/// Client -> server.
pub type ClientMsg = oxylite::protocol::ClientMsg<Table>;

/// Server -> client.
pub type ServerMsg = oxylite::protocol::ServerMsg<Table>;

/// One table's snapshot: every live row, payload-shaped.
pub type TableData = oxylite::protocol::TableData<Table>;

/// A recorded deletion, riding the Snapshot (lib-owned type; the docs
/// live on the generic definition).
pub type Tombstone = oxylite::protocol::Tombstone<Table>;

/// The app's SCHEMA version: the crate version itself, derived from
/// this manifest — one source of truth, so the wire const and any
/// consumer (client, server, tests) can never drift apart by accident
/// (reviewer: no out-of-date version numbers). The lib compares
/// MAJOR.MINOR on the wire; the local DB's IDB epoch is GENERATED from
/// the migration count and needs no maintenance (#34). Bump discipline
/// on the manifest version: MAJOR = breaking change to shipped
/// schema/protocol, MINOR = additive schema or protocol change (stale
/// tabs reload), PATCH = anything compatible.
pub const SCHEMA_VERSION: &str = env!("CARGO_PKG_VERSION");

// The demo app's MIGRATION LIST is generated at build time (see
// build.rs): the UNION of the lib's protocol-table migrations
// (`../sync/migrations`) and this crate's app-table migrations
// (`migrations/`), merged by version. One list drives BOTH demo sides —
// the server's `sqlx::migrate!` (app tables; the lib runs its own via
// `sync::server::migrate`) and the client's apply-once boot.
include!(concat!(env!("OUT_DIR"), "/migrations.rs"));
