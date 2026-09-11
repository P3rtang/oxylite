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
    /// The column was present but held SQL NULL and the mapper requires
    /// text. Distinct from `MissingColumn` because the fixes differ
    /// (drifted SELECT vs. nullability the mapper must decide on).
    #[error("column {0:?} is null — the mapper requires text")]
    NullColumn(String),
    /// The column was present but held a value the read path can't shape
    /// (and isn't null): `Type` says what it actually was, so new value
    /// shapes extend the enum — not the error type.
    #[error("column {1:?} is {0}, not text")]
    InvalidType(Type, String),
    /// A column's value didn't parse into the mapper's type.
    #[error("column {0:?} is not a valid uuid")]
    BadUuid(String),
    /// A timestamp column held text that doesn't parse — typed so the
    /// malformed value rides the error, and LWW text comparisons never
    /// see it.
    #[error("column {0:?}: {1}")]
    BadTimestamp(String, crate::timestamp::TimestampError),
}

use std::fmt;

/// The JS-side type family a row value arrived as (PGlite hands rows back
/// as plain JSON objects). `Number` covers JS numbers and bigints alike —
/// the distinction never changes what a mapper should do. Extends as the
/// read path learns to shape more types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Type {
    Number,
    Boolean,
    Array,
    Object,
    Function,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Type::Number => "number",
            Type::Boolean => "boolean",
            Type::Array => "array",
            Type::Object => "object",
            Type::Function => "function",
        })
    }
}

#[cfg(feature = "client")]
mod client {
    use crate::from_row::{RowError, Type};
    use wasm_bindgen::JsValue;

    /// A raw row from the local DB: a JS object keyed by column name, with
    /// string values (our schema is text-only; see the PGlite bridge).
    pub type Row = js_sys::Object;

    /// Row -> typed result, mirroring sqlx's `FromRow`. The engine stays
    /// row-generic; result types own their conversion.
    pub trait FromRow: Sized {
        fn from_row(row: &Row) -> Result<Self, crate::from_row::RowError>;
    }

    /// A string field off a raw row, null-aware. The three-layer result
    /// keeps every reason distinct — `Ok(None)` is SQL NULL (a real value
    /// the mapper decides on, for nullable columns), `Err(MissingColumn)`
    /// is a column the SELECT dropped, `Err(InvalidType)` a value the
    /// read path can't shape. Plain text passes through; PGlite's JS
    /// `Date` (what a `timestamptz` column comes back as) normalizes to
    /// canonical ISO at this edge — the one place the read path changes
    /// shape (the same rule as the bridge's typed reads).
    pub fn str_field(row: &Row, key: &str) -> Result<Option<String>, RowError> {
        use wasm_bindgen::JsCast as _;
        let key_js = key.into();
        if !js_sys::Reflect::has(row, &key_js).unwrap_or(false) {
            return Err(RowError::MissingColumn(key.into()));
        }
        // Degenerate: a getter threw — the value is present but unreadable,
        // which no type names honestly; plain JSON rows never get here.
        let value = js_sys::Reflect::get(row, &key_js)
            .map_err(|_| RowError::InvalidType(Type::Object, key.into()))?;
        if value.is_null() || value.is_undefined() {
            return Ok(None);
        }
        if let Some(s) = value.as_string() {
            return Ok(Some(s));
        }
        if value.is_instance_of::<js_sys::Date>() {
            return Ok(Some(js_sys::Date::from(value).to_iso_string().into()));
        }
        Err(RowError::InvalidType(classify(&value), key.into()))
    }

    /// The required-text form: for columns the mapper's schema says NOT
    /// NULL, SQL NULL is its own error — never folded into a fabricated
    /// "missing column".
    pub fn str_field_req(row: &Row, key: &str) -> Result<String, RowError> {
        str_field(row, key)?.ok_or_else(|| RowError::NullColumn(key.into()))
    }

    /// The type family of a value that fell through the text/null/Date
    /// shaping. Predicates over casts — this wasm-bindgen has no typeof
    /// bridge, and the cast IS the classification that matters (a value
    /// a mapper could convert is not a mapping error).
    fn classify(value: &JsValue) -> Type {
        if value.is_function() {
            Type::Function
        } else if value.as_f64().is_some() || value.is_bigint() {
            Type::Number
        } else if value.as_bool().is_some() {
            Type::Boolean
        } else if js_sys::Array::is_array(value) {
            Type::Array
        } else {
            Type::Object
        }
    }
}

#[cfg(feature = "client")]
pub use client::{FromRow, Row, str_field, str_field_req};
