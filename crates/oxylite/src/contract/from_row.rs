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
    use crate::contract::from_row::{RowError, Type};
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

        /// Read one field, shaped by `T`: the generic reader that replaces
        /// per-family methods. The three layers stay distinct —
        /// `Ok(None)` is SQL NULL (the mapper decides, for nullable
        /// columns), `Err(MissingColumn)` a column the SELECT dropped,
        /// `Err(InvalidType)` a value `T` couldn't shape.
        pub fn field<T: FromJs>(&self, key: &str) -> Result<Option<T>, RowError> {
            self.raw(key)?
                .map(|v| T::from_js(&v).map_err(|ty| RowError::InvalidType(ty, key.into())))
                .transpose()
        }

        /// The required form: for columns the mapper's schema says NOT
        /// NULL, SQL NULL is its own error — never folded into a
        /// fabricated "missing column".
        pub fn field_req<T: FromJs>(&self, key: &str) -> Result<T, RowError> {
            self.field(key)?
                .ok_or_else(|| RowError::NullColumn(key.into()))
        }
    }

    /// A value shape the read path can produce out of a row — the
    /// extension point for custom Postgres types. The lib ships the
    /// standard shapes (text/number/boolean, with PGlite's JS `Date`
    /// normalized to canonical ISO on the text path); apps implement
    /// their own — enum text, jsonb payloads, domains — without
    /// touching the lib. The storage/wire contract stays text
    /// (`SyncRow` binds strings); this trait is the read side only.
    ///
    /// The contract: shape ONE non-null JS value. On mismatch, return
    /// the type family actually found — the field accessor wraps it
    /// into `RowError::InvalidType` with the column's name.
    pub trait FromJs: Sized {
        fn from_js(value: &JsValue) -> Result<Self, Type>;
    }

    impl FromJs for String {
        fn from_js(value: &JsValue) -> Result<Self, Type> {
            if let Some(s) = value.as_string() {
                return Ok(s);
            }
            if value.is_instance_of::<js_sys::Date>() {
                return Ok(js_sys::Date::from(value.clone()).to_iso_string().into());
            }
            Err(classify(value))
        }
    }

    impl FromJs for f64 {
        fn from_js(value: &JsValue) -> Result<Self, Type> {
            if let Some(n) = value.as_f64() {
                return Ok(n);
            }
            if let Some(b) = value.dyn_ref::<js_sys::BigInt>() {
                // Number(bigint) — the JS coercion. This js-sys binds no
                // Number methods, so the global constructor is reached
                // reflectively. i64-scale values above 2^53 lose
                // precision: a bigint-exact reader is the escape hatch
                // if a table ever needs one.
                return js_sys::Reflect::get(&js_sys::global(), &"Number".into())
                    .ok()
                    .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
                    .and_then(|f| f.call1(&JsValue::NULL, b).ok())
                    .and_then(|n| n.as_f64())
                    .ok_or_else(|| classify(value));
            }
            Err(classify(value))
        }
    }

    impl FromJs for bool {
        fn from_js(value: &JsValue) -> Result<Self, Type> {
            value.as_bool().ok_or_else(|| classify(value))
        }
    }

    /// Row -> typed result, mirroring sqlx's `FromRow`. The engine stays
    /// row-generic; result types own their conversion.
    pub trait FromRow: Sized {
        fn from_row(row: &Row) -> Result<Self, crate::contract::from_row::RowError>;
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
pub use client::{FromJs, FromRow, Row};
