//! The wake adapter (roadmap 2.2): ONE `PgListener` per DB feeding the
//! pub/sub bus. Postgres is the source publisher (the 0011 trigger on
//! `sync_log`); everything in-process rides [`crate::pubsub::Bus`] —
//! nothing else touches LISTEN. The bus carries the NEWS (the seq),
//! never the rows: per-socket cursors force per-socket pulls, so the
//! adapter's only job is fan-out of "seq N exists".
//!
//! Reconnect is the adapter's own loop — sqlx does NOT auto-reconnect.
//! A silently dead connection just looks like an idle LISTEN (recv
//! blocks forever with no events), which is precisely the failure the
//! demoted fallback tick exists to cover.

use sqlx::postgres::{PgListener, PgPool};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::pubsub::{Bus, SUB_CAPACITY, coalesce_burst};

/// The pg channel the 0011 trigger notifies on and the bus topic it
/// feeds — one const, both sides agree by convention; the live pubsub
/// tests pin the pair against the migrated schema.
pub const OPS_CHANNEL: &str = "oxylite_ops";

/// Reconnect delay after the listener stream dies. Missed notifies
/// during the gap are covered by the fallback tick; 1s keeps the window
/// small at a trivial retry cost (a stopped DB retries at 1qps — noise).
const RECONNECT: Duration = Duration::from_secs(1);

/// Spawn the adapter task: lives for the server's lifetime, loops
/// listen → forward → coalesce → publish with backoff reconnects. The
/// returned handle exists for shutdown (`abort`) — nothing else reads it.
pub fn spawn_wake_adapter(db: PgPool, bus: Bus<i64>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&db, &bus).await {
                // connect/listen failed — the operator sees it, the
                // clients keep working off the fallback tick (3.2 will
                // replace the print).
                eprintln!("wake adapter: {e}");
            }
            tokio::time::sleep(RECONNECT).await;
        }
    })
}

/// One listener lifetime: LISTEN, forward raw seqs, coalesce-publish.
/// Returns when the stream dies — the forwarder drops its sender, the
/// coalescer drains the remainder (no wake lost across the reconnect),
/// and the caller reconnects.
async fn run_once(db: &PgPool, bus: &Bus<i64>) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(db).await?;
    listener.listen(OPS_CHANNEL).await?;

    // Armed — announce with a ZERO wake. The consumer contract (the
    // session wiring in 2.2 step 3 must implement it): the sentinel
    // ALWAYS triggers a pull — it is the readiness signal AND, on a
    // reconnect, the re-pull nudge for anything missed while the
    // stream was down (notifies sent before LISTEN is live are gone
    // for good) — while REAL seqs skip when wake ≤ cursor. That keeps
    // the fallback tick as last resort only.
    bus.publish(OPS_CHANNEL, 0);

    let (tx, mut rx) = mpsc::channel::<i64>(SUB_CAPACITY);
    // Detached on purpose: the forwarder's lifetime is the channel's —
    // it exits when the listener dies (sender dropped) or when `rx` is
    // gone (sends fail). See the tail comment for the full lifecycle.
    tokio::spawn(async move {
        // Idle-forever is HEALTHY for LISTEN (no events to read); a
        // silently dead connection just looks idle — the fallback tick
        // is the surface, by design.
        while let Ok(n) = listener.recv().await {
            let Ok(seq) = n.payload().parse::<i64>() else {
                // The channel is lib-owned, so a foreign payload is not
                // expected — skip it rather than kill the stream; the
                // fallback tick stays the floor.
                eprintln!("wake adapter: unparseable notify payload");
                continue;
            };
            if tx.send(seq).await.is_err() {
                break; // coalescer gone — nothing left to feed
            }
        }
    });

    while let Some(seq) = coalesce_burst(&mut rx).await {
        bus.publish(OPS_CHANNEL, seq);
    }
    Ok(())
    // No abort needed for the forwarder: this loop ends only when the
    // forwarder's sender dropped (the task already finished), and if
    // publish panicked past this point the unwind drops `rx`, whose
    // full channel makes the forwarder's next send fail — it exits
    // itself.
}
