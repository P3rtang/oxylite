//! Reads: the raw row type and the mapping trait that turns one into a
//! typed result.

/// A raw row from the local DB: a JS object keyed by column name, with
/// string values (our schema is text-only; see pglite.rs).
pub type Row = js_sys::Object;

/// Why mapping a raw row to `T` failed. Typed so call sites can match on
/// the shape of the failure instead of parsing prose. Serializable: it
/// rides inside `EngineError` when apply failures relay across tabs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum RowError {
    /// The row lacks a column the mapper reads — usually a SELECT that
    /// drifted from the row type.
    #[error("missing column {0:?}")]
    MissingColumn(String),
    /// A column's value didn't parse into the mapper's type.
    #[error("column {0:?} is not a valid uuid")]
    BadUuid(String),
}

/// Row -> typed result, mirroring sqlx's `FromRow` so a shared type maps
/// with `query_as` on the server and with this trait on the client. The
/// engine stays row-generic; result types own their conversion.
pub trait FromRow: Sized {
    fn from_row(row: &Row) -> Result<Self, RowError>;
}
