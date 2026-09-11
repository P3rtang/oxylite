//! The `Timestamp` contract: parse normalizes any RFC 3339 instant into
//! the canonical wire form; the inner value is a real
//! `chrono::DateTime<Utc>` (instant Eq/Ord), so offsets CONVERT instead
//! of being dropped (the naive-`timestamp` trap), and malformed input is
//! a typed error carrying the offending string. The serde path validates
//! too — it IS the wire boundary.

use sync::timestamp::{Timestamp, TimestampError};
use uuid::Uuid;

// Fixture table (#31): the suite instantiates the protocol standalone.
use enum_iterator::Sequence;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FixtureTable {
    Notes,
}

type Op = sync::protocol::Op<FixtureTable>;

impl sync::table::SyncTable for FixtureTable {
    fn as_str(self) -> &'static str {
        "notes"
    }
    fn from_name(name: &str) -> Option<Self> {
        (name == "notes").then_some(Self::Notes)
    }
}

#[test]
fn variants_canonicalize_to_one_wire_shape() {
    let full = Timestamp::parse("2026-09-09T10:00:01.000Z").unwrap();
    let no_fraction = Timestamp::parse("2026-09-09T10:00:01Z").unwrap();
    let offset = Timestamp::parse("2026-09-09T12:00:01.000+02:00").unwrap();
    // Instant equality (chrono), regardless of input spelling...
    assert_eq!(full, no_fraction);
    assert_eq!(full, offset);
    // ...and one byte-exact wire form (the js toISOString shape).
    assert_eq!(full.canonical_text(), "2026-09-09T10:00:01.000Z");
    assert_eq!(no_fraction.canonical_text(), "2026-09-09T10:00:01.000Z");
    assert_eq!(offset.canonical_text(), "2026-09-09T10:00:01.000Z");
}

#[test]
fn offsets_convert_instead_of_dropping() {
    // The naive-`timestamp` trap: Postgres's `timestamp` (without tz)
    // silently DISCARDS the "+02:00" and stores the wall time; timestamptz
    // + parse convert to the true instant. The contract pins conversion:
    // 14:00+02:00 is 12:00Z, not "14:00Z with the offset dropped".
    let offset_input = Timestamp::parse("2026-09-09T14:00:00.000+02:00").unwrap();
    let true_instant = Timestamp::parse("2026-09-09T12:00:00.000Z").unwrap();
    let naive_dropped = Timestamp::parse("2026-09-09T14:00:00.000Z").unwrap();
    assert_eq!(offset_input, true_instant);
    assert_ne!(offset_input, naive_dropped);
    assert_eq!(offset_input.canonical_text(), "2026-09-09T12:00:00.000Z");
}

#[test]
fn ordering_is_instant_ordering() {
    let t1 = Timestamp::parse("2026-09-09T10:00:01.000Z").unwrap();
    let t2 = Timestamp::parse("2026-09-09T10:00:02.000Z").unwrap();
    let t3 = Timestamp::parse("2026-09-09T10:00:03.000Z").unwrap();
    assert!(t1 < t2 && t2 < t3);
    // Cross-offset spellings order by instant, not by text.
    let t2_offset = Timestamp::parse("2026-09-09T12:00:02.000+02:00").unwrap();
    assert!(t1 < t2_offset && t2_offset < t3);
}

#[test]
fn malformed_input_is_typed_and_carries_the_string() {
    let err = Timestamp::parse("not-a-time").unwrap_err();
    assert_eq!(err.input, "not-a-time");
    assert!(!err.reason.is_empty());
    // Valid ISO shape, invalid calendar — rejected too (chrono's calendar
    // math, not shape-guessing).
    let err = Timestamp::parse("2026-13-01T00:00:00.000Z").unwrap_err();
    assert_eq!(err.input, "2026-13-01T00:00:00.000Z");
}

#[test]
fn epoch_millis_is_the_clock_door() {
    // Round-trips with parse: same instant, same canonical text.
    let parsed = Timestamp::parse("2026-09-09T10:00:01.000Z").unwrap();
    let millis = parsed.as_datetime().timestamp_millis();
    assert_eq!(Timestamp::from_epoch_millis(millis), Some(parsed));
    // Sub-second millis survive exactly (the js clock's resolution).
    let parsed = Timestamp::parse("2026-09-09T10:00:01.500Z").unwrap();
    assert_eq!(
        Timestamp::from_epoch_millis(parsed.as_datetime().timestamp_millis()),
        Some(parsed)
    );
    // Out-of-range values are None, not a wrong timestamp.
    assert_eq!(Timestamp::from_epoch_millis(i64::MAX), None);
}

#[test]
fn serde_carries_the_text_and_validates_on_deserialize() {
    let ts = Timestamp::parse("2026-09-09T10:00:01Z").unwrap();
    // Serializes as the plain canonical string — the wire shape is
    // unchanged.
    assert_eq!(
        serde_json::to_value(ts).unwrap(),
        serde_json::json!("2026-09-09T10:00:01.000Z")
    );
    // Deserializing validates AND canonicalizes, so a wire value can't
    // bypass the invariants.
    let ts: Timestamp = serde_json::from_value(serde_json::json!("2026-09-09T10:00:01Z")).unwrap();
    assert_eq!(ts.canonical_text(), "2026-09-09T10:00:01.000Z");
    // Malformed: the error message names the offending input.
    let err = serde_json::from_value::<Timestamp>(serde_json::json!("oops"))
        .expect_err("malformed timestamp must fail");
    assert!(err.to_string().contains("oops"), "error: {err}");
}

#[test]
fn ops_carry_timestamps_through_serde() {
    // The envelope type: a malformed updated_at fails the message parse,
    // naming the input — the failure a WS layer surfaces.
    let payload = serde_json::json!({
        "table": "notes",
        "id": Uuid::now_v7(),
        "data": serde_json::Value::Null,
        "updated_at": "definitely not a time",
    });
    let err = serde_json::from_value::<Op>(payload).expect_err("must fail");
    assert!(
        err.to_string().contains("definitely not a time"),
        "error: {err}"
    );
}

#[test]
fn timestamp_error_is_self_describing() {
    let err = Timestamp::parse("bad").unwrap_err();
    let echoed = TimestampError {
        input: err.input.clone(),
        reason: err.reason.clone(),
    };
    assert_eq!(err.to_string(), echoed.to_string());
    assert!(err.to_string().contains("bad"));
}
