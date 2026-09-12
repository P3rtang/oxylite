//! Pure contracts for the pub/sub bus (2.2, step 1 — reviewer: build the
//! bus first, proven by tests, socket integration after). No DB, no
//! transport: the wake semantics the whole push design leans on are
//! pinned here, transport-free.

use oxylite::pubsub::{Bus, SUB_CAPACITY, coalesce_burst};
use tokio::sync::mpsc;

#[tokio::test]
async fn fan_out_delivers_to_every_subscriber() {
    let bus: Bus<i64> = Bus::new();
    let mut a = bus.subscribe("ops");
    let mut b = bus.subscribe("ops");
    bus.publish("ops", 7);
    assert_eq!(a.recv().await, Some(7));
    assert_eq!(b.recv().await, Some(7));
}

#[tokio::test]
async fn dropping_a_subscription_removes_it() {
    let bus: Bus<i64> = Bus::new();
    let sub = bus.subscribe("ops");
    assert_eq!(bus.subscriber_count("ops"), 1);
    drop(sub);
    assert_eq!(bus.subscriber_count("ops"), 0);
    // The emptied topic must behave as never-subscribed, not resurrect.
    bus.publish("ops", 1);
    assert_eq!(bus.subscriber_count("ops"), 0);
}

#[tokio::test]
async fn topics_are_isolated() {
    let bus: Bus<i64> = Bus::new();
    let mut ops = bus.subscribe("ops");
    let mut presence = bus.subscribe("presence");
    bus.publish("ops", 1);
    assert_eq!(ops.recv().await, Some(1));
    assert_eq!(
        presence.try_recv(),
        Err(mpsc::error::TryRecvError::Empty),
        "a wake on one topic must not leak into another"
    );
}

#[tokio::test]
async fn overflow_drops_wakes_but_never_the_subscriber() {
    let bus: Bus<i64> = Bus::new();
    let mut sub = bus.subscribe("ops");
    let total = SUB_CAPACITY as i64 + 10;
    for seq in 0..total {
        bus.publish("ops", seq);
    }
    // The bound's worth of wakes buffered; the overflow dropped for this
    // subscriber — and the subscriber itself survives to receive again.
    for seq in 0..SUB_CAPACITY as i64 {
        assert_eq!(sub.recv().await, Some(seq));
    }
    bus.publish("ops", 999);
    assert_eq!(sub.recv().await, Some(999));
}

#[tokio::test]
async fn publishing_to_a_topic_without_subscribers_is_a_noop() {
    let bus: Bus<i64> = Bus::new();
    bus.publish("ops", 1);
    assert_eq!(bus.subscriber_count("ops"), 0);
    // And the topic comes into existence cleanly on later subscribe.
    let mut sub = bus.subscribe("ops");
    bus.publish("ops", 2);
    assert_eq!(sub.recv().await, Some(2));
}

#[tokio::test]
async fn coalesce_burst_takes_the_max_of_the_queued_burst() {
    let (tx, mut rx) = mpsc::channel(16);
    for seq in [3, 1, 7, 2] {
        tx.send(seq).await.unwrap();
    }
    drop(tx);
    assert_eq!(coalesce_burst(&mut rx).await, Some(7));
}

#[tokio::test]
async fn coalesce_burst_blocks_while_the_source_is_alive() {
    // A single seq with the sender still open: the helper returns that
    // seq (nothing queued behind it), so an adapter loop can keep
    // awaiting the next burst.
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(5).await.unwrap();
    assert_eq!(coalesce_burst(&mut rx).await, Some(5));
    tx.send(9).await.unwrap();
    assert_eq!(coalesce_burst(&mut rx).await, Some(9));
}

#[tokio::test]
async fn coalesce_burst_reports_a_closed_source() {
    let (tx, mut rx) = mpsc::channel::<i64>(16);
    drop(tx);
    assert_eq!(coalesce_burst(&mut rx).await, None);
}
