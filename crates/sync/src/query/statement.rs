//! The read the component writes: `Query` — SQL, bound params, deps.

use super::dep::Dep;
use uuid::Uuid;

/// A read to run against the local DB. `id` is generated at construction
/// and is the registry key for `listen`/`unsubscribe` (a subscription rides
/// the component that built the query; see `use_query`).
#[derive(Clone)]
pub struct Query {
    pub(crate) id: Uuid,
    pub(crate) sql: String,
    pub(crate) params: Vec<String>,
    pub(crate) deps: Vec<Dep>,
}

impl Query {
    pub fn new(sql: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            sql: sql.into(),
            params: Vec::new(),
            deps: Vec::new(),
        }
    }

    #[allow(dead_code)] // planned API (SYNC_API.md); first use is a detail view
    pub fn param(mut self, value: impl Into<String>) -> Self {
        self.params.push(value.into());
        self
    }

    pub fn dep(mut self, dep: Dep) -> Self {
        self.deps.push(dep);
        self
    }
}
