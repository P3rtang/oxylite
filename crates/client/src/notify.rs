//! Centralized error display: one overlay attached to the root app, one
//! global notice queue. The API expects `Display` (any error's `to_string`
//! renders as the message); sources that need structure build a `Notice`
//! with their own title + message. Client-side on purpose — the sync lib
//! stays UI-agnostic.

use dioxus::prelude::*;
use oxylite::engine::timer_pause;
use std::fmt::Display;
use uuid::Uuid;

/// One toast: a title and a message, rendered until auto-dismissed.
#[derive(Debug, Clone)]
pub struct Notice {
    pub id: Uuid,
    pub title: String,
    pub message: String,
}

impl Notice {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7(),
            title: title.into(),
            message: message.into(),
        }
    }

    /// Default shape for any `Display`-able error: `to_string()` becomes
    /// the message. For call sites without a better title.
    #[allow(dead_code)]
    pub fn error(err: impl Display) -> Self {
        Self::new("Something went wrong", err.to_string())
    }
}

/// The queue the overlay renders. GlobalSignal — same pattern as the
/// engine's STATUS: it must initialize outside any dioxus scope.
pub static NOTICES: Global<Signal<Vec<Notice>>, Vec<Notice>> = Signal::global(Vec::new);

/// Show a notice, auto-dismissed after a few seconds. Callable from any
/// task (event handlers, spawned writes).
pub fn notify(notice: Notice) {
    let id = notice.id;

    NOTICES.write_unchecked().push(notice);

    spawn(async move {
        timer_pause(4000).await;
        NOTICES.write_unchecked().retain(|n| n.id != id);
    });
}

/// Fixed-position stack at the bottom-right; render once from the root.
#[component]
pub fn NoticeOverlay() -> Element {
    let notices = NOTICES.read().clone();
    rsx! {
        div { style: "position:fixed; bottom:16px; right:16px; display:flex; flex-direction:column; gap:8px; z-index:1000",
            for n in notices.iter() {
                div {
                    key: "{n.id}",
                    style: "background:#222; color:#fff; padding:10px 14px; border-radius:8px; box-shadow:0 2px 8px rgba(0,0,0,.35); max-width:320px",
                    strong { "{n.title}" }
                    div { style: "font-size:0.9em; color:#ddd; margin-top:2px", "{n.message}" }
                }
            }
        }
    }
}
