mod pglite;
mod sync;

use dioxus::prelude::*;
use pglite::{Pglite, rows_of, str_field};
use shared::{ClientMsg, Note, ServerMsg};
use sync::{
    apply_events, load_cursor, log, new_note, note_to_op, open_socket, push_pending,
    save_cursor, send, sync_url, take_inbox, take_pending,
};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::WebSocket;

fn main() {
    console_error_panic_hook::set_once();
    dioxus::launch(App);
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS notes (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL DEFAULT '',
        body TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

const LIST_SQL: &str =
    "SELECT id, title, body, updated_at FROM notes ORDER BY updated_at DESC";

const INSERT_SQL: &str = "
    INSERT INTO notes (id, title, body, updated_at)
    VALUES ($1, $2, $3, $4)";

#[component]
fn App() -> Element {
    let mut notes = use_signal(Vec::<Note>::new);
    let mut status = use_signal(|| "starting…".to_string());
    let mut cursor = use_signal(|| -1i64);
    let mut ws = use_signal(|| None::<WebSocket>);
    let mut title_input = use_signal(String::new);

    // Bootstrap and sync loop. This is the only long-lived task: JS
    // callbacks (socket onmessage) merely enqueue server messages, so no
    // task is ever spawned from outside a dioxus scope.
    use_effect(move || {
        spawn(async move {
            let pglite = match Pglite::init(SCHEMA).await {
                Ok(p) => p,
                Err(e) => {
                    status.set(format!("pglite failed: {}", pglite_error(&e)));
                    return;
                }
            };
            log("boot", "pglite ready");
            *cursor.write() = load_cursor(&pglite).await;
            refresh(&mut notes).await;
            status.set("offline — local data loaded".into());

            loop {
                status.set("connecting…".into());
                match open_socket(&sync_url()) {
                    Ok(sock) => {
                        ws.set(Some(sock.clone()));
                        status.set("connected".into());
                        log("sync", "connected");
                        sync_session(&sock, cursor, &pglite, notes).await;
                        ws.set(None);
                        log("sync", "disconnected — retrying in 3s");
                        status.set("offline — will retry…".into());
                        timer_pause(3000).await;
                    }
                    Err(e) => {
                        log("sync", &format!("connect failed: {e:?}"));
                        status.set("offline — will retry…".into());
                        timer_pause(3000).await;
                    }
                }
            }
        });
    });

    rsx! {
        div { style: "max-width: 640px; margin: 2rem auto; font-family: system-ui, sans-serif",
            h1 { "Offline Notes" }
            p { style: "color:#666",
                "{status} · local: PGlite (IndexedDB) · remote: Postgres via WS"
            }
            div { style: "display:flex; gap:8px",
                input {
                    style: "flex:1; padding:6px",
                    value: "{title_input}",
                    placeholder: "Note title…",
                    oninput: move |e| title_input.set(e.value().to_string()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            submit(title_input, ws, notes);
                        }
                    },
                }
                button {
                    style: "padding:6px 14px",
                    onclick: move |_| submit(title_input, ws, notes),
                    "Add"
                }
            }
            ul { style: "margin-top: 1rem; line-height: 1.8",
                for note in notes.read().iter() {
                    li { key: "{note.id}",
                        strong { "{note.title}" }
                        span { style: "color:#999", " · {note.updated_at}" }
                    }
                }
            }
        }
    }
}

/// Store the note locally, then push it (or queue it while offline) and
/// refresh the UI. Shared by the Add button and the Enter key.
fn submit(
    mut title_input: Signal<String>,
    ws: Signal<Option<WebSocket>>,
    mut notes: Signal<Vec<Note>>,
) {
    let t = title_input.read().clone();
    if t.is_empty() { return; }
    title_input.set(String::new());

    let note = new_note(&t);
    spawn(async move {
        let pglite = Pglite::init(SCHEMA).await.expect("pglite ready");
        pglite.query(INSERT_SQL, &[
            note.id.to_string(),
            note.title.clone(),
            note.body.clone(),
            note.updated_at.clone(),
        ]).await.expect("insert note");

        match &*ws.read() {
            Some(sock) if sock.ready_state() == WebSocket::OPEN => {
                send(sock, &ClientMsg::Push { ops: vec![note_to_op(&note)] });
            }
            _ => push_pending(note_to_op(&note)),
        }
        refresh(&mut notes).await;
    });
}

/// Reload the note list from the local DB into the UI signal.
async fn refresh(notes: &mut Signal<Vec<Note>>) {
    let pglite = Pglite::init(SCHEMA).await.expect("pglite ready");
    let rows = pglite.query(LIST_SQL, &[]).await.expect("query notes");
    notes.set(rows_of(&rows).into_iter().map(|row| Note {
        id: str_field(&row, "id").unwrap().parse().unwrap(),
        title: str_field(&row, "title").unwrap(),
        body: str_field(&row, "body").unwrap(),
        updated_at: str_field(&row, "updated_at").unwrap(),
    }).collect());
}

/// One connected session: flush the initial pull + pending pushes, then
/// drain the inbox until the socket closes. Returns to the caller's
/// reconnect loop.
async fn sync_session(
    sock: &WebSocket,
    mut cursor: Signal<i64>,
    pglite: &Pglite,
    mut notes: Signal<Vec<Note>>,
) {
    let mut flushed = false;
    let mut connecting_ms = 0u32;
    loop {
        match sock.ready_state() {
            WebSocket::CONNECTING => {
                // A handshake can hang (firewall, proxy, mock); give up and
                // let the caller reconnect rather than waiting forever.
                connecting_ms += 100;
                if connecting_ms > 5000 {
                    log("sync", "handshake timeout — treating as offline");
                    return;
                }
                timer_pause(100).await;
            }
            WebSocket::OPEN => {
                if !flushed {
                    flushed = true;
                    send(sock, &ClientMsg::Pull { since: *cursor.read() });
                    for op in take_pending() {
                        send(sock, &ClientMsg::Push { ops: vec![op] });
                    }
                }
                for msg in take_inbox() {
                    match msg {
                        ServerMsg::Ack { .. } => {
                            // The server may hold events we haven't seen; pull again.
                            send(sock, &ClientMsg::Pull { since: *cursor.read() });
                        }
                        ServerMsg::Events { events, cursor: c } => {
                            log("sync", &format!("received {} events, cursor -> {}", events.len(), c));
                            *cursor.write() = c;
                            apply_events(pglite, &events).await;
                            save_cursor(pglite, c).await;
                            refresh(&mut notes).await;
                        }
                    }
                }
                timer_pause(150).await;
            }
            _ => return, // closed or closing: reconnect in the outer loop
        }
    }
}

/// Sleep in the browser without tokio (wasm has no time driver).
async fn timer_pause(ms: u32) {
    let p = js_sys::Promise::new(&mut |resolve, _reject| {
        let cb = Closure::wrap(Box::new(move || {
            let _ = resolve.call0(&wasm_bindgen::JsValue::NULL);
        }) as Box<dyn FnMut()>);
        let _ = web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                ms as i32,
            );
        cb.forget();
    });
    wasm_bindgen_futures::JsFuture::from(p).await.ok();
}

fn pglite_error(e: &wasm_bindgen::JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
