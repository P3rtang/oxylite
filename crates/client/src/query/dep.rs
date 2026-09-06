//! What invalidates a query.

use shared::Table;
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
    Table(Table),
    Row(Uuid),
}

impl Dep {
    pub(crate) fn matches(self, table: Table, row_id: Uuid) -> bool {
        match self {
            Dep::Table(t) => t == table,
            Dep::Row(id) => id == row_id,
        }
    }
}
