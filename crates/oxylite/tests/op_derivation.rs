//! Pin the derivable-op default (`SyncRow::op`) — the pure half of the
//! write-path API (#41): table, pk, wire JSON, and the ordering axis
//! (the row's own LWW when the table has one, else the caller-injected
//! clock — the trait never reads a clock). The engine's upsert/delete
//! build on this; composite app types compose it; a regression here
//! changes what every client ships as ops.

use enum_iterator::Sequence;
use oxylite::contract::sync_row::SyncRow;
use oxylite::contract::table::SyncTable;
use oxylite::protocol::Op;
use oxylite::protocol::timestamp::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Sequence, Serialize, Deserialize)]
enum Widgets {
    W,
}

impl SyncTable for Widgets {
    fn as_str(self) -> &'static str {
        "widgets"
    }
    fn from_name(name: &str) -> Option<Self> {
        (name == "widgets").then_some(Widgets::W)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Widget {
    id: Uuid,
    name: String,
    updated_at: String,
}

impl SyncRow for Widget {
    type Table = Widgets;
    const TABLE: Widgets = Widgets::W;
    const COLUMNS: &'static [&'static str] = &["id", "name", "updated_at"];
    const LWW: Option<&'static str> = Some("updated_at");
    const PK: &'static str = "id";
    fn params(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            self.name.clone(),
            self.updated_at.clone(),
        ]
    }
    fn pk(&self) -> Uuid {
        self.id
    }
}

/// The LWW-less sibling: batch order decides, so the op's timestamp is
/// whatever the caller injects (the engine's clock in practice).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Plain {
    id: Uuid,
    name: String,
}

impl SyncRow for Plain {
    type Table = Widgets;
    const TABLE: Widgets = Widgets::W;
    const COLUMNS: &'static [&'static str] = &["id", "name"];
    fn params(&self) -> Vec<String> {
        vec![self.id.to_string(), self.name.clone()]
    }
    fn pk(&self) -> Uuid {
        self.id
    }
}

fn canonical(millis: i64) -> String {
    Timestamp::from_epoch_millis(millis)
        .expect("in range")
        .canonical_text()
}

fn ts(millis: i64) -> Timestamp {
    Timestamp::parse(&canonical(millis)).expect("canonical text parses")
}

#[test]
fn an_lww_row_ops_itself_its_own_timestamp_wins_over_the_injected_clock() {
    let id = Uuid::now_v7();
    let row = Widget {
        id,
        name: "w".into(),
        updated_at: canonical(1_000),
    };
    // The injected `now` is deliberately different — an LWW row's op
    // carries the ROW's ordering axis, never the caller's clock.
    let op: Op<Widgets> = row.op(ts(2_000));

    assert_eq!(op.table, Widgets::W);
    assert_eq!(op.id, id);
    assert_eq!(op.data, serde_json::to_value(&row).unwrap());
    assert_eq!(op.updated_at.canonical_text(), canonical(1_000));
}

#[test]
fn an_lww_less_row_takes_the_injected_clock() {
    let id = Uuid::now_v7();
    let row = Plain {
        id,
        name: "p".into(),
    };
    let op: Op<Widgets> = row.op(ts(3_000));

    assert_eq!(op.updated_at.canonical_text(), canonical(3_000));
    assert_eq!(op.data, serde_json::to_value(&row).unwrap());
}

#[test]
fn the_op_round_trips_through_the_wire_shape() {
    // The data payload is the row's serde form — the applier decodes it
    // back through the same type, so the derivation must be the plain
    // to_value, nothing custom.
    let row = Widget {
        id: Uuid::now_v7(),
        name: "w".into(),
        updated_at: canonical(0),
    };
    let op: Op<Widgets> = row.op(ts(1));
    let json = serde_json::to_string(&op).unwrap();
    let back: Op<Widgets> = serde_json::from_str(&json).unwrap();
    assert_eq!(back.data["name"], "w");
    assert_eq!(back.updated_at.canonical_text(), canonical(0));
}

#[test]
fn the_upsert_couple_emits_op_and_step_together() {
    // The couple's whole point: the statement and the op it rides leave
    // ONE function — a caller composing rows into a composite batch
    // cannot produce the step without producing its op.
    let row = Widget {
        id: Uuid::now_v7(),
        name: "w".into(),
        updated_at: canonical(5_000),
    };
    let mut ops = Vec::new();
    let (sql, params) = row.upsert_with_ops(ts(9_000), &mut ops);

    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].id, row.id);
    assert_eq!(ops[0].updated_at.canonical_text(), canonical(5_000));
    assert_eq!(sql, Widget::guarded_upsert_sql(1));
    assert_eq!(params, row.params());
}

#[test]
fn the_delete_couple_emits_the_tombstone_op_and_step() {
    let id = Uuid::now_v7();
    let mut ops = Vec::new();
    let (sql, params) = Widget::delete_with_ops(id, ts(7_000), &mut ops);

    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].id, id);
    assert_eq!(ops[0].data, serde_json::Value::Null);
    assert_eq!(ops[0].updated_at.canonical_text(), canonical(7_000));
    assert_eq!(sql, Widget::delete_sql(1));
    assert_eq!(params, vec![id.to_string(), canonical(7_000)]);
}

#[test]
fn rows_compose_into_one_batch_via_the_couple() {
    // The 1-to-many shape: a base op and N child ops accumulate into one
    // vec — one batch, applied in composition order.
    let base = Widget {
        id: Uuid::now_v7(),
        name: "base".into(),
        updated_at: canonical(1),
    };
    let children: Vec<Plain> = (0..2)
        .map(|i| Plain {
            id: Uuid::now_v7(),
            name: format!("child{i}"),
        })
        .collect();

    let mut ops: Vec<Op<Widgets>> = Vec::new();
    let mut steps = vec![base.upsert_with_ops(ts(9), &mut ops).0];
    for c in &children {
        steps.push(c.upsert_with_ops(ts(9), &mut ops).0);
    }

    assert_eq!(ops.len(), 3);
    assert_eq!(ops[0].id, base.id);
    assert_eq!(ops[1].data["name"], "child0");
    assert_eq!(ops[2].data["name"], "child1");
    assert_eq!(steps.len(), 3);
}
