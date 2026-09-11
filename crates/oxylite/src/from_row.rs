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
    #[error("column {1:?} is {0}")]
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
    Text,
    Number,
    Boolean,
    Date,
    Array,
    Object,
    Function,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Type::Text => "text",
            Type::Number => "number",
            Type::Boolean => "boolean",
            Type::Date => "date",
            Type::Array => "array",
            Type::Object => "object",
            Type::Function => "function",
        })
    }
}

#[cfg(feature = "client")]
mod client {
    use crate::from_row::{RowError, Type};
    use wasm_bindgen::{JsCast as _, JsValue};

    /// A raw row from the local DB: a JS object keyed by column name. A
    /// newtype (not an alias) so the typed readers are methods ON the row —
    /// one namespace per shape family instead of free functions per type.
    pub struct Row(JsValue);

    impl Row {
        /// The single construction edge: `pglite::rows_of` wraps every row
        /// PGlite hands back; nothing else builds one.
        pub fn from_value(value: JsValue) -> Self {
            Row(value)
        }

        /// The shared read core, null-aware: `Ok(None)` is SQL NULL (a
        /// real value the typed reader decides on), `Err(MissingColumn)`
        /// a column the SELECT dropped. Presence is checked explicitly —
        /// JS cannot distinguish an absent key from a null value through
        /// `get` alone.
        fn raw(&self, key: &str) -> Result<Option<JsValue>, RowError> {
            let key_js = key.into();
            if !js_sys::Reflect::has(&self.0, &key_js).unwrap_or(false) {
                return Err(RowError::MissingColumn(key.into()));
            }
            // Degenerate: a getter threw — the value is present but
            // unreadable, which no type names honestly; plain JSON rows
            // never get here.
            js_sys::Reflect::get(&self.0, &key_js)
                .map(|v| (!v.is_null() && !v.is_undefined()).then_some(v))
                .map_err(|_| RowError::InvalidType(Type::Object, key.into()))
        }

        /// A text field: plain text passes through; PGlite's JS `Date`
        /// (what a `timestamptz` column comes back as) normalizes to
        /// canonical ISO at this edge — the one place the read path
        /// changes shape (the same rule as the bridge's typed reads).
        pub fn str_field(&self, key: &str) -> Result<Option<String>, RowError> {
            self.raw(key)?
                .map(|v| {
                    if let Some(s) = v.as_string() {
                        return Ok(s);
                    }
                    if v.is_instance_of::<js_sys::Date>() {
                        return Ok(js_sys::Date::from(v).to_iso_string().into());
                    }
                    Err(RowError::InvalidType(classify(&v), key.into()))
                })
                .transpose()
        }

        /// The required-text form: for columns the mapper's schema says
        /// NOT NULL, SQL NULL is its own error — never folded into a
        /// fabricated "missing column".
        pub fn str_field_req(&self, key: &str) -> Result<String, RowError> {
            self.str_field(key)?
                .ok_or_else(|| RowError::NullColumn(key.into()))
        }

        /// A numeric field: JS numbers cast via `as_f64`; bigints coerce
        /// the JS way (`Number(bigint)`) — i64-scale values above 2^53
        /// lose precision, so a bigint-exact reader is the escape hatch
        /// if a table ever needs it.
        pub fn num_field(&self, key: &str) -> Result<Option<f64>, RowError> {
            self.raw(key)?
                .map(|v| {
                    if let Some(n) = v.as_f64() {
                        return Ok(n);
                    }
                    if let Some(b) = v.dyn_ref::<js_sys::BigInt>() {
                        // Number(bigint) — the JS coercion. This js-sys
                        // binds no Number methods, so the global
                        // constructor is reached reflectively.
                        let coerced = js_sys::Reflect::get(&js_sys::global(), &"Number".into())
                            .ok()
                            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
                            .and_then(|f| f.call1(&JsValue::NULL, b).ok())
                            .and_then(|n| n.as_f64());
                        return match coerced {
                            Some(n) => Ok(n),
                            None => Err(RowError::InvalidType(Type::Number, key.into())),
                        };
                    }
                    Err(RowError::InvalidType(classify(&v), key.into()))
                })
                .transpose()
        }

        /// The required-number form (NOT NULL columns).
        pub fn num_field_req(&self, key: &str) -> Result<f64, RowError> {
            self.num_field(key)?
                .ok_or_else(|| RowError::NullColumn(key.into()))
        }

        /// A boolean field.
        pub fn bool_field(&self, key: &str) -> Result<Option<bool>, RowError> {
            self.raw(key)?
                .map(|v| {
                    v.as_bool()
                        .ok_or_else(|| RowError::InvalidType(classify(&v), key.into()))
                })
                .transpose()
        }

        /// The required-boolean form (NOT NULL columns).
        pub fn bool_field_req(&self, key: &str) -> Result<bool, RowError> {
            self.bool_field(key)?
                .ok_or_else(|| RowError::NullColumn(key.into()))
        }
    }

    /// Row -> typed result, mirroring sqlx's `FromRow`. The engine stays
    /// row-generic; result types own their conversion.
    pub trait FromRow: Sized {
        fn from_row(row: &Row) -> Result<Self, crate::from_row::RowError>;
    }

    /// The type family of a value that fell through the typed reader's
    /// shaping — named in `InvalidType` so new value shapes extend the
    /// enum, not the error type.
    fn classify(value: &JsValue) -> Type {
        if value.is_function() {
            Type::Function
        } else if value.as_f64().is_some() || value.is_bigint() {
            Type::Number
        } else if value.as_bool().is_some() {
            Type::Boolean
        } else if value.is_instance_of::<js_sys::Date>() {
            Type::Date
        } else if js_sys::Array::is_array(value) {
            Type::Array
        } else if value.is_string() {
            Type::Text
        } else {
            Type::Object
        }
    }
}

#[cfg(feature = "client")]
pub use client::{FromRow, Row};
