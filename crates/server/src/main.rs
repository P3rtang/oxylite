mod sync;

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use shared::{ClientMsg, ServerMsg};
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://sync:sync@localhost:5432/offline_notes".into());

    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("connect to postgres");

    // Same migration files the clients run against PGlite (see shared::MIGRATIONS).
    sqlx::migrate!("../shared/migrations")
        .run(&db)
        .await
        .expect("apply migrations");

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/sync", get(sync_ws))
        // The vendored PGlite bundle (ES module + wasm + data), served with
        // proper MIME types so the client can dynamically `import()` it.
        .nest_service(
            "/pglite",
            tower_http::services::ServeDir::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../client/assets/pglite"
            ))
            .append_index_html_on_directories(false),
        )
        // The built dioxus client (crates/client/dist) — everything on one
        // origin, so the browser ESM import and the WS connection are trivial.
        .fallback_service(tower_http::services::ServeDir::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../client/dist"
        )))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(db);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("server listening on http://0.0.0.0:3000 (ws sync at /sync)");
    axum::serve(listener, app).await.unwrap();
}

async fn sync_ws(ws: WebSocketUpgrade, State(db): State<sqlx::PgPool>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, db))
}

async fn handle_socket(mut socket: WebSocket, db: sqlx::PgPool) {
    // Where this connection has streamed so far; starts at the current
    // server cursor so we only push changes that happen *during* this
    // connection. Older events arrive via explicit Pull.
    let mut stream_cursor = sync::current_cursor(&db).await;
    // One snapshot per connection, max: a follow-up Pull replays events
    // instead, otherwise a stale snapshot and the backlog would ping-pong.
    let mut snapshotted = false;

    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(64);

    // Ticker: stream any server changes to this connection.
    let ticker_db = db.clone();
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match sync::pull_since(&ticker_db, stream_cursor).await {
                Ok((events, cursor)) if !events.is_empty() => {
                    stream_cursor = cursor;
                    let _ = out_tx.send(ServerMsg::Events { events, cursor }).await;
                }
                Ok(_) => {}
                Err(e) => eprintln!("stream error: {e}"),
            }
        }
    });

    loop {
        tokio::select! {
            Some(msg) = out_rx.recv() => {
                if !send_msg(&mut socket, &msg).await {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let reply = match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(ClientMsg::Push { ops }) => match sync::push(&db, &ops).await {
                                Ok(cursor) => ServerMsg::Ack { cursor },
                                Err(e) => { eprintln!("push error: {e}"); continue; }
                            },
                            Ok(ClientMsg::Pull { since }) => {
                                // Far behind? Replay would be one upsert per
                                // logged op — hand over a snapshot instead
                                // (once per connection; a follow-up Pull
                                // replays events, so snapshot and backlog
                                // can't ping-pong).
                                let head = sync::current_cursor(&db).await;
                                if head - since > sync::SNAPSHOT_AFTER_OPS && !snapshotted {
                                    snapshotted = true;
                                    match sync::load_or_build_snapshot(&db).await {
                                        Ok((seq, tables)) => ServerMsg::Snapshot { seq, tables },
                                        Err(e) => {
                                            eprintln!("snapshot error: {e}");
                                            continue;
                                        }
                                    }
                                } else {
                                    match sync::pull_since(&db, since).await {
                                        Ok((events, cursor)) => ServerMsg::Events { events, cursor },
                                        Err(e) => { eprintln!("pull error: {e}"); continue; }
                                    }
                                }
                            }
                            Err(e) => { eprintln!("bad message: {e}"); continue; }
                        };
                        if !send_msg(&mut socket, &reply).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    ticker.abort();
}

/// Serialize and deliver one server message; false means the socket died.
async fn send_msg(socket: &mut WebSocket, msg: &ServerMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}
