//! The protocol's timestamp type. Every `updated_at`/`deleted_at` on the
//! wire is a `Timestamp`, not a `String`: validity is established once,
//! at the parse boundary, so no call site ever re-checks (and no
//! malformed text can reach the LWW comparisons where it would *silently*
//! corrupt merge decisions — a bad timestamp doesn't error, it
//! mis-orders).
//!
//! The inner value is a real `chrono::DateTime<Utc>`: Eq/Ord/Hash are
//! instant semantics, never text semantics. The wire/storage TEXT form
//! stays canonical — the JS `Date.toISOString()` shape,
//! `YYYY-MM-DDTHH:MM:SS.mmmZ` — and [`Timestamp::parse`] normalizes any
//! RFC 3339 variant (`…01Z`, `…01.00Z`, `…01.000+00:00`) to it, so the
//! wire format is unchanged and byte-stable. (Sub-millisecond precision
//! is truncated: the protocol's LWW axis is milliseconds, which is also
//! every client clock's resolution.)
//!
//! Columns are `timestamptz` (migration 0007): Postgres rejects anything
//! that isn't a timestamp at the storage layer too, and the server's
//! sqlx decodes/binds real `DateTime`s — the only place TEXT ↔ timestamp
//! conversion still happens is the wire (serde) and the client bridge,
//! which both normalize to the canonical form.

use chrono::{DateTime, Utc};
use serde::de::{Deserialize, Deserializer, Error as DeError, Unexpected, Visitor};
use serde::{Serialize, Serializer};
use std::fmt;

/// A validated timestamp. Construct via [`Timestamp::parse`] (wire),
/// [`Timestamp::from_epoch_millis`] (clocks), or
/// [`Timestamp::from_datetime`] (already-typed sources like sqlx decodes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    /// Parse and canonicalize. Accepts any RFC 3339 timestamp; the wire
    /// text is always the millisecond-precision Zulu form, so `…01Z`,
    /// `…01.000Z` and `…01.000+00:00` parse to equal values.
    pub fn parse(input: &str) -> Result<Self, TimestampError> {
        let dt = DateTime::parse_from_rfc3339(input).map_err(|e| TimestampError {
            input: input.to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self(dt.with_timezone(&Utc)))
    }

    /// From epoch milliseconds — the clock door. The client's only clock
    /// is `js_sys::Date` (`get_time()` → f64 epoch millis); this is
    /// infallible for every real date, so producers carry no failure path.
    /// `None` only for values outside chrono's representable range.
    pub fn from_epoch_millis(ms: i64) -> Option<Self> {
        DateTime::from_timestamp_millis(ms).map(Self)
    }

    /// From an already-typed source — sqlx timestamptz decodes. Infallible:
    /// the value is a timestamp by construction.
    pub fn from_datetime(dt: DateTime<Utc>) -> Self {
        Self(dt)
    }

    /// The instant as a `DateTime<Utc>` (Copy) — sqlx binds, chrono
    /// arithmetic.
    pub fn as_datetime(&self) -> DateTime<Utc> {
        self.0
    }

    /// The canonical wire/storage text: millisecond-precision Zulu, the
    /// JS `Date.toISOString()` shape. This is what binds into SQL TEXT
    /// contexts and what serde emits.
    pub fn canonical_text(&self) -> String {
        self.0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical_text())
    }
}

/// Why a string isn't a `Timestamp`: carries the offending input (so the
/// error is self-describing without a dump of the surrounding message)
/// and the parser's reason. Serializable — relayed across tabs inside
/// `RowError`/`EngineError`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimestampError {
    pub input: String,
    pub reason: String,
}

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a valid RFC 3339 timestamp: {}",
            self.input, self.reason
        )
    }
}

impl std::error::Error for TimestampError {}

// The wire carries the canonical text; deserialization is the parse
// boundary for everything that arrives as JSON (WS messages, op
// payloads, snapshots) — a malformed timestamp fails there, with the
// raw input in the error, rather than flowing on as a `String`.
impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical_text())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Timestamp;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a canonical ISO-8601 timestamp (RFC 3339)")
            }

            fn visit_str<E: DeError>(self, input: &str) -> Result<Timestamp, E> {
                Timestamp::parse(input)
                    .map_err(|e| E::invalid_value(Unexpected::Str(e.input.as_str()), &self))
            }
        }
        deserializer.deserialize_str(V)
    }
}
