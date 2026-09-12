//! The in-process pub/sub bus (ROADMAP 2.2): named topics, best-effort
//! wake delivery, RAII subscriptions. Transport-free and sqlx-free — it
//! stands on its own (reviewer, 2026-09-12), which is why it lives at
//! the crate root under its own `pubsub` feature instead of inside
//! `server/`. Tokio's mpsc is its only machinery.
//!
//! Wake semantics, not delivery guarantees: publish does a bounded
//! `try_send` — a slow subscriber whose channel is full has that wake
//! DROPPED, never blocks the publisher. Correctness lives in the
//! at-least-once pull layer, whose fallback tick covers any lost wake.
//! A dropped subscription is removed on `Drop` — socket disconnect = sub
//! gone, the engine's registry pattern (#31).
//!
//! First consumer (2.2): one `PgListener` adapter feeds the bus; each
//! WS session subscribes and pulls against its own cursor. The bus
//! carries the NEWS (the seq), never the rows — per-socket cursors
//! force per-socket pulls. Future topics (per-table, per-user at 4.2,
//! snapshot invalidation, presence) ride the same API additively.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Per-subscription channel bound: past it, wakes are dropped for that
/// subscriber (see the module docs — wake semantics). 64 matches the
/// socket loop's outbound Tick channel.
pub const SUB_CAPACITY: usize = 64;

pub struct Bus<M: Clone + Send + 'static> {
    inner: Arc<Inner<M>>,
}

struct Inner<M> {
    topics: Mutex<HashMap<String, Vec<Sub<M>>>>,
    next_id: AtomicU64,
}

struct Sub<M> {
    id: u64,
    tx: mpsc::Sender<M>,
}

impl<M: Clone + Send + 'static> Bus<M> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                topics: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Register a subscriber on `topic`. The returned [`Subscription`]
    /// owns the receiving half; dropping it deregisters — no explicit
    /// unsubscribe call to forget.
    pub fn subscribe(&self, topic: impl Into<String>) -> Subscription<M> {
        let topic = topic.into();
        let (tx, rx) = mpsc::channel(SUB_CAPACITY);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .topics
            .lock()
            .unwrap()
            .entry(topic.clone())
            .or_default()
            .push(Sub { id, tx });
        Subscription {
            id,
            topic,
            rx,
            bus: Arc::clone(&self.inner),
        }
    }

    /// Best-effort fan-out to every live subscriber on `topic`. Full
    /// channel = the wake is dropped for that subscriber (it stays
    /// registered); disconnected = purged (defensive — `Drop` is the
    /// normal removal). No subscribers = no-op, so publishing before
    /// anyone listens is free.
    pub fn publish(&self, topic: &str, msg: M) {
        let mut topics = self.inner.topics.lock().unwrap();
        let Some(subs) = topics.get_mut(topic) else {
            return;
        };
        subs.retain(|sub| !matches!(sub.tx.try_send(msg.clone()), Err(TrySendError::Closed(_))));
        if subs.is_empty() {
            topics.remove(topic);
        }
    }

    /// Live subscriber count for `topic` — observability + test seams.
    pub fn subscriber_count(&self, topic: &str) -> usize {
        self.inner
            .topics
            .lock()
            .unwrap()
            .get(topic)
            .map_or(0, |subs| subs.len())
    }
}

impl<M: Clone + Send + 'static> Default for Bus<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Clone + Send + 'static> Clone for Bus<M> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// One topic subscription: the receiving half plus its own deregistration.
/// `recv` blocks while idle and never resolves `None` through the public
/// API — the sub holds its own sender registered until `Drop`, so the
/// receiving half cannot observe closure. A `None` would mean the bus's
/// `Arc` is gone everywhere, which owning this sub prevents.
pub struct Subscription<M> {
    id: u64,
    topic: String,
    rx: mpsc::Receiver<M>,
    bus: Arc<Inner<M>>,
}

impl<M> Subscription<M> {
    pub async fn recv(&mut self) -> Option<M> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<M, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

impl<M> Drop for Subscription<M> {
    fn drop(&mut self) {
        let mut topics = self.bus.topics.lock().unwrap();
        if let Some(subs) = topics.get_mut(&self.topic) {
            subs.retain(|sub| sub.id != self.id);
            if subs.is_empty() {
                // Forget emptied topics — otherwise a churn of
                // connect/disconnect cycles grows the map forever.
                topics.remove(&self.topic);
            }
        }
    }
}

/// The wake-adapter's burst policy — NOT a bus policy (the bus publishes
/// what it is given; presence later may not want collapsing): await one
/// seq, then drain everything already queued, keep the MAX. A 10-op push
/// fires 10 per-row notifies, a 1000-op backfill 1000 — each should cost
/// the fan-out ONE wake. `None` = the source closed (the adapter task
/// exits); with the source alive it blocks for the next seq.
pub async fn coalesce_burst(rx: &mut mpsc::Receiver<i64>) -> Option<i64> {
    let mut seq = rx.recv().await?;
    while let Ok(next) = rx.try_recv() {
        seq = seq.max(next);
    }
    Some(seq)
}
