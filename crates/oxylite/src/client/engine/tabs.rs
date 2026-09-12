//! Cross-tab plumbing: Web Locks leader election + BroadcastChannel
//! transport (docs/impl/multi-tab.md). One tab in the browser holds the
//! exclusive [`TAB_LOCK`] and runs the engine; the rest follow and proxy
//! their DB access over [`TAB_CHANNEL`]. A leader's lock is released only
//! by its document dying (tab close, reload, crash) — at which point the
//! browser hands the lock to one of the tabs queued on it, which promotes
//! in place. No new runtime pieces, and if either API is missing the
//! caller degrades to today's per-tab engines.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Function, JsNullable};
use wasm_bindgen::{JsCast, JsValue, prelude::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{BroadcastChannel, LockOptions, MessageEvent};

/// The browser-wide exclusive lock naming THE sync engine.
pub const TAB_LOCK: &str = "sync-engine-leader";

/// The BroadcastChannel every tab of the browser joins.
pub const TAB_CHANNEL: &str = "sync-engine-tabs";

/// An exclusive claim on the browser's single engine slot. The lock is
/// held for the tab's lifetime: the request callback returned a promise
/// that never settles, and the browser releases the lock only when that
/// promise settles or the document is destroyed. There is nothing to
/// release explicitly — closing the tab is the release.
pub struct LockHold {
    _private: (),
}

/// Try to become the leader without waiting: `Some` when the lock was free
/// (this tab now holds it), `None` when another tab leads.
pub async fn try_acquire() -> Option<LockHold> {
    acquire(true).await
}

/// Queue for leadership: resolves only when this tab is granted the lock —
/// i.e. when the previous leader released it by dying. The request stays
/// queued with the browser from the moment this is called, so promotion
/// happens even if every other tab is gone.
pub async fn wait_acquire() -> LockHold {
    acquire(false)
        .await
        .expect("a blocking lock request is always granted")
}

/// Fire one `navigator.locks.request`. The grant callback resolves a
/// JS-promise "signal" Rust awaits (carrying the granted lock, or null
/// under `if_available`) and returns a never-settling promise that keeps
/// the lock held. The callback is forgotten: it lives until the document
/// dies, which is exactly the hold's lifetime.
async fn acquire(if_available: bool) -> Option<LockHold> {
    let window = web_sys::window().expect("no window");
    let locks = window.navigator().locks();
    let opts = LockOptions::new();
    opts.set_if_available(if_available);

    let (signal, resolve_slot) = settle_promise();
    let grant: Closure<dyn Fn(JsValue) -> js_sys::Promise> =
        Closure::new(move |lock: JsValue| -> js_sys::Promise {
            if let Some(resolve) = resolve_slot.borrow().as_ref() {
                let _ = resolve.call1(&JsValue::NULL, &lock);
            }
            hold_forever()
        });
    // web-sys types the callback as Function<fn(JsNullable<Lock>) ->
    // Promise>; our closure travels as a plain JS function value. The
    // call itself cannot fail as a binding (it returns the request's
    // promise; Web Locks are baseline in all modern browsers).
    type LockCallback = Function<fn(JsNullable<web_sys::Lock>) -> js_sys::Promise>;
    let _request = locks.request_with_options(
        TAB_LOCK,
        &opts,
        &grant.into_js_value().unchecked_into::<LockCallback>(),
    );

    // Ok(lock) = granted; Ok(null) = busy under `if_available`; Err = the
    // signal promise rejected (never observed in practice — degrade to
    // leading rather than hang the boot).
    match JsFuture::from(signal).await {
        Ok(lock) if !lock.is_null() => Some(LockHold { _private: () }),
        Ok(_) => None,
        Err(e) => {
            super::log(
                "tabs",
                &format!("lock signal rejected ({e:?}) — self-leading"),
            );
            Some(LockHold { _private: () })
        }
    }
}

/// A promise plus the resolve function to settle it from a JS callback.
/// The `Promise::new` executor runs synchronously, so the slot is filled
/// by the time this returns.
fn settle_promise() -> (js_sys::Promise, Rc<RefCell<Option<Function>>>) {
    let slot: Rc<RefCell<Option<Function>>> = Rc::default();
    let capture = slot.clone();
    let promise = js_sys::Promise::new(&mut move |resolve, _| {
        *capture.borrow_mut() = Some(resolve);
    });
    (promise, slot)
}

/// A promise that never settles: the browser's lock is held until it
/// settles or the document dies. Nothing ever calls the resolvers.
fn hold_forever() -> js_sys::Promise {
    js_sys::Promise::new(&mut |_, _| {})
}

/// The cross-tab transport. Messages are serde JSON strings — same style
/// as the websocket — posted under a fixed channel name; every tab of the
/// browser receives every message (except its own posts).
pub struct TabsChannel {
    channel: BroadcastChannel,
}

impl TabsChannel {
    pub fn new() -> Result<Self, JsValue> {
        Ok(Self {
            channel: BroadcastChannel::new(TAB_CHANNEL)?,
        })
    }

    /// Register the incoming-message hook. Like the socket's onmessage,
    /// this callback runs outside any dioxus scope and only enqueues.
    pub fn set_on_message(&self, f: &Closure<dyn FnMut(MessageEvent)>) {
        self.channel.set_onmessage(Some(f.as_ref().unchecked_ref()));
    }

    pub fn post(&self, text: &str) {
        let _ = self.channel.post_message(&JsValue::from_str(text));
    }
}
