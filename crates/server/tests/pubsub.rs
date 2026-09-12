//! Live contracts for the wake adapter (2.2, step 2): the 0011 trigger
//! fires on push, the adapter coalesces each commit-burst to ONE wake at
//! the max seq, and bursts deliver in order. Runtime queries — test
//! targets don't own `.sqlx` entries.

use oxylite::pubsub::{Bus, Subscription};
use server::sync::{OPS_CHANNEL, push, spawn_wake_adapter};
use shared::Table;
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

/// Generous: the adapter must wake within this after a push.
const WAKE_BUDGET: Duration = Duration::from_secs(5);
/// Idle probe: after a burst is consumed, this long of silence means no
/// spurious trailing wakes (the only publisher is the adapter, and the
/// burst's notifies all landed at commit).
const IDLE_BUDGET: Duration = Duration::from_millis(150);

/// The adapter announces arm (and every reconnect) with a ZERO wake —
/// skip-when-current makes it a no-op for current sessions, so it is a
/// pure readiness signal. Tests await it instead of racing the LISTEN
/// setup (a push committing before arm loses its notify for good).
async fn await_armed(sub: &mut Subscription<i64>) {
    assert_eq!(
        timeout(WAKE_BUDGET, sub.recv()).await.unwrap(),
        Some(0),
        "first wake must be the arm sentinel"
    );
}

fn upsert_op(id: Uuid, title: &str, at: &str) -> oxylite::protocol::Op<Table> {
    oxylite::protocol::Op {
        table: Table::Notes,
        id,
        data: serde_json::json!({
            "id": id, "title": title, "body": "", "updated_at": at,
        }),
        updated_at: oxylite::protocol::timestamp::Timestamp::parse(at).unwrap(),
    }
}

async fn push_notes(pool: &sqlx::PgPool, titles: &[&str]) -> i64 {
    let ops: Vec<_> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| upsert_op(Uuid::now_v7(), t, &format!("2026-01-01T00:00:{i:02}.000Z")))
        .collect();
    push(pool, &ops).await.unwrap()
}

#[sqlx::test]
async fn the_trigger_notifies_the_adapter_channel(pool: sqlx::PgPool) {
    server::sync::migrator().run(&pool).await.unwrap();
    // The channel is a literal in the migration SQL and a const in the
    // adapter — this is the guard that the pair never drifts (a checked
    // macro cannot see DDL; the migrated schema is the truth). The
    // literal lives in the FUNCTION body, so the source is what's pinned.
    let src: String =
        sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname = 'notify_sync_log'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        src.contains(&format!("'{}'", OPS_CHANNEL)),
        "trigger function must notify {OPS_CHANNEL}: {src}"
    );
}

#[sqlx::test]
async fn a_push_burst_arrives_as_one_wake_at_the_max_seq(pool: sqlx::PgPool) {
    server::sync::migrator().run(&pool).await.unwrap();
    let bus: Bus<i64> = Bus::new();
    spawn_wake_adapter(pool.clone(), bus.clone());
    let mut sub = bus.subscribe(OPS_CHANNEL);
    await_armed(&mut sub).await;

    // Three ops in ONE push = one transaction = three notifies at
    // COMMIT. The coalescer publishes the max of what is QUEUED when it
    // drains — normally that is ONE wake at 3, but the protocol only
    // promises order and max-last (a descheduled forwarder may surface
    // per-row seqs as separate wakes). So: drain until idle, the last
    // must be the max, none may regress, nothing may trail.
    push_notes(&pool, &["a", "b", "c"]).await;

    let mut last = timeout(WAKE_BUDGET, sub.recv()).await.unwrap().unwrap();
    while let Ok(Some(seq)) = timeout(IDLE_BUDGET, sub.recv()).await {
        assert!(seq > last, "wakes must be monotonic: {last} → {seq}");
        last = seq;
    }
    assert_eq!(last, 3, "the burst ends at its max seq");
}

#[sqlx::test]
async fn sequential_pushes_deliver_monotonic_wakes(pool: sqlx::PgPool) {
    server::sync::migrator().run(&pool).await.unwrap();
    let bus: Bus<i64> = Bus::new();
    spawn_wake_adapter(pool.clone(), bus.clone());
    let mut sub = bus.subscribe(OPS_CHANNEL);
    await_armed(&mut sub).await;

    push_notes(&pool, &["a"]).await;
    assert_eq!(timeout(WAKE_BUDGET, sub.recv()).await.unwrap(), Some(1));
    push_notes(&pool, &["b"]).await;
    assert_eq!(timeout(WAKE_BUDGET, sub.recv()).await.unwrap(), Some(2));
    // A bigger burst collapses onto its own max, continuing the order.
    push_notes(&pool, &["c", "d", "e"]).await;
    assert_eq!(timeout(WAKE_BUDGET, sub.recv()).await.unwrap(), Some(5));
}
