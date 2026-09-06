use crate::pglite::{self, Pglite};
use shared::{ClientMsg, Note, Op, ServerMsg, NOTES_TABLE};
use uuid::Uuid;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::WebSocket;

// Queues bridging the JS-callback world and Rust async tasks. JS callbacks
// (socket onmessage) run without a dioxus scope, so they must not spawn
// tasks; they only enqueue, and the single sync-session loop drains.

/// Server messages received over the websocket, awaiting processing.
static INBOX: std::sync::Mutex<Vec<ServerMsg>> = std::sync::Mutex::new(Vec::new());

/// Local writes that could not be delivered while offline.
static PENDING: std::sync::Mutex<Vec<Op>> = std::sync::Mutex::new(Vec::new());

pub fn push_inbox(msg: ServerMsg) {
    INBOX.lock().unwrap().push(msg);
}

pub fn take_inbox() -> Vec<ServerMsg> {
    std::mem::take(&mut *INBOX.lock().unwrap())
}

pub fn push_pending(op: Op) {
    PENDING.lock().unwrap().push(op);
}

pub fn take_pending() -> Vec<Op> {
    std::mem::take(&mut *PENDING.lock().unwrap())
}

/// Console logging for debugging in Chrome devtools.
pub fn log(kind: &str, msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("[{kind}] {msg}")));
}

/// Persisted sync cursor helpers (survive reloads, stored in PGlite itself).
pub async fn load_cursor(pglite: &Pglite) -> i64 {
    pglite
        .query("SELECT value FROM meta WHERE key = 'cursor'", &[])
        .await
        .ok()
        .and_then(|r| {
            pglite::rows_of(&r)
                .first()
                .and_then(|row| pglite::str_field(row, "value")?.parse().ok())
        })
        .unwrap_or(-1)
}

pub async fn save_cursor(pglite: &Pglite, cursor: i64) {
    let _ = pglite
        .query(
            "INSERT INTO meta (key, value) VALUES ('cursor', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            &[cursor.to_string()],
        )
        .await;
}

/// Open the sync websocket. Incoming messages are parsed and enqueued; the
/// caller drives everything else by polling `take_inbox()` and the socket's
/// `ready_state()`.
pub fn open_socket(url: &str) -> Result<WebSocket, JsValue> {
    let ws = WebSocket::new(url)?;

    let onmessage = wasm_bindgen::closure::Closure::wrap(
        Box::new(move |e: web_sys::MessageEvent| {
            if let Some(text) = e.data().as_string()
                && let Ok(msg) = serde_json::from_str::<ServerMsg>(&text)
            {
                push_inbox(msg);
            }
        }) as Box<dyn FnMut(web_sys::MessageEvent)>,
    );
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    Ok(ws)
}

pub fn send(ws: &WebSocket, msg: &ClientMsg) {
    if let Ok(text) = serde_json::to_string(msg) {
        let _ = ws.send_with_str(&text);
    }
}

/// Sync URL derived from the current page origin (single-origin deployment:
/// the client, the vendored PGlite bundle and the WS all live on the server).
pub fn sync_url() -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_else(|| "http://localhost:3000".into());
    format!("{}/sync", origin.replacen("http", "ws", 1))
}

/// Apply remote events into local PGlite (LWW upsert; server is the source
/// of truth for concurrent edits on the same row).
pub async fn apply_events(pglite: &Pglite, events: &[Op]) {
    for ev in events {
        if ev.table != NOTES_TABLE {
            continue;
        }
        let Ok(note) = serde_json::from_value::<Note>(ev.data.clone()) else {
            continue;
        };
        let _ = pglite
            .query(
                "INSERT INTO notes (id, title, body, updated_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (id) DO UPDATE
                   SET title = EXCLUDED.title,
                       body = EXCLUDED.body,
                       updated_at = EXCLUDED.updated_at
                 WHERE EXCLUDED.updated_at > notes.updated_at",
                &[
                    note.id.to_string(),
                    note.title,
                    note.body,
                    note.updated_at,
                ],
            )
            .await;
    }
}

pub fn note_to_op(note: &Note) -> Op {
    Op {
        table: NOTES_TABLE.into(),
        id: note.id,
        data: serde_json::to_value(note).unwrap(),
        updated_at: note.updated_at.clone(),
    }
}

pub fn new_note(title: &str) -> Note {
    Note {
        id: Uuid::new_v4(),
        title: title.into(),
        body: String::new(),
        updated_at: now_iso(),
    }
}

pub fn now_iso() -> String {
    js_sys::Date::new_0().to_iso_string().into()
}
