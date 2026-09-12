//! Live-query registration and the RAII subscription guard.

use crate::client::engine::unsubscribe;
use dioxus::prelude::Signal;
use std::rc::Rc;
use uuid::Uuid;

use super::dep::Dep;

/// Subscription id: identifies a registered query for unregistration.
pub type SubscriptionId = Uuid;

/// A live query registered with the engine.
#[derive(Clone)]
pub(crate) struct Subscription {
    pub(crate) id: SubscriptionId,
    pub(crate) deps: Vec<Dep>,
    pub(crate) rev: Signal<u64>,
}

/// RAII subscription guard: the "unsub callback" is its Drop impl, so a
/// component literally cannot leak a registration — when the LAST handle
/// drops (component unmounted, hook state released), the engine forgets
/// the query. Cloning shares the same subscription; the id stays
/// engine-internal (callers never need it).
#[derive(Clone)]
pub struct SubscriptionGuard {
    /// Only purpose is Drop timing: releasing the last Rc unsubscribes.
    _inner: Rc<SubGuardInner>,
}

struct SubGuardInner {
    id: SubscriptionId,
}

impl Drop for SubGuardInner {
    fn drop(&mut self) {
        crate::client::engine::unsubscribe(self.id);
    }
}

impl SubscriptionGuard {
    pub(crate) fn new(id: SubscriptionId) -> Self {
        Self {
            _inner: Rc::new(SubGuardInner { id }),
        }
    }
}
