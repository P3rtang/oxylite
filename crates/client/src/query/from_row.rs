//! Reads: the raw row type and the mapping trait that turns one into a
//! typed result.

/// A raw row from the local DB: a JS object keyed by column name, with
/// string values (our schema is text-only; see pglite.rs).
pub type Row = js_sys::Object;

/// Row -> typed result, mirroring sqlx's `FromRow` so a shared type maps
/// with `query_as` on the server and with this trait on the client. The
/// engine stays row-generic; result types own their conversion.
pub trait FromRow: Sized {
    fn from_row(row: &Row) -> Result<Self, String>;
}
