//! What invalidates a query.
//!
//! A table dependency is the table's WIRE NAME (`&'static str` from
//! `SyncTable::as_str`), not the enum value (#31): it keeps the
//! reactive plumbing free of the app's table type — bespoke queries
//! stay `Query::new(sql).dep(Dep::Table(Note::TABLE.as_str()))` with no
//! turbofish, and the name is the same identity the server logs in
//! `sync_log.table_name`. The trait guarantees the name is static and
//! stable, so nothing stringly-typed sneaks in.

use uuid::Uuid;

/// What invalidates a query. `Table` fires on any change to that table;
/// `Row` only when that specific row changes. Both may be listed — the
/// engine matches either against the (table, row_id) of every applied
/// change, local or remote.
///
/// `Row` is defined now (approved design) and used once a detail view
/// exists; `param` likewise.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    Table(&'static str),
    Row(Uuid),
}

impl Dep {
    pub(crate) fn matches(self, table: &str, row_id: Uuid) -> bool {
        match self {
            Dep::Table(t) => t == table,
            Dep::Row(id) => id == row_id,
        }
    }
}
