//! Reads: the raw row type and the mapping trait that turns one into a
//! typed result. CLIENT-side by nature (`Row` is a JS object straight
//! out of PGlite) — feature-gated so the host graph never mixes types
//! in; the server consumes rows through sqlx instead. `RowError` is
//! pure data and stays ungated: it rides inside sync's `EngineError`
//! when apply failures relay across tabs.
//!
//! Lib-owned again (#31) — this was sync's until #30 parked it in the
//! app's shared crate; the inversion moved it back with the rest of the
//! protocol surface.

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
    /// A timestamp column held text that doesn't parse — typed so the
    /// malformed value rides the error, and LWW text comparisons never
    /// see it.
    #[error("column {0:?}: {1}")]
    BadTimestamp(String, crate::timestamp::TimestampError),
}

#[cfg(feature = "client")]
mod client {
    /// A raw row from the local DB: a JS object keyed by column name, with
    /// string values (our schema is text-only; see the PGlite bridge).
    pub type Row = js_sys::Object;

    /// Row -> typed result, mirroring sqlx's `FromRow`. The engine stays
    /// row-generic; result types own their conversion.
    pub trait FromRow: Sized {
        fn from_row(row: &Row) -> Result<Self, crate::from_row::RowError>;
    }

    /// A string field off a raw row: plain text passes through; PGlite's
    /// JS `Date` (what a `timestamptz` column comes back as) normalizes
    /// to canonical ISO at this edge — the one place the read path
    /// changes shape (the same rule as the bridge's typed reads).
    pub fn str_field(row: &Row, key: &str) -> Option<String> {
        use wasm_bindgen::JsCast as _;
        let value = js_sys::Reflect::get(row, &key.into()).ok()?;
        if value.as_string().is_some() {
            return value.as_string();
        }
        if value.is_instance_of::<js_sys::Date>() {
            return Some(js_sys::Date::from(value).to_iso_string().into());
        }
        None
    }
}

#[cfg(feature = "client")]
pub use client::{FromRow, Row, str_field};
