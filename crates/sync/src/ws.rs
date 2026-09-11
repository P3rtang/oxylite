//! The WS transport, axum flavor (feature `axum`, implies `server`).
//! One drop-in route for any axum backend of the protocol: the socket
//! loop, the change-stream ticker and the framing live here; the
//! per-connection decisions are the transport-free [`Session`]
//! (`server` feature), and the per-table SQL is the app's plug-ins.
//! Framework mounts are separate features by design (reviewer decision,
//! #28 r2) — a future actix backend would add its own module over the
//! same core, no core changes.

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    routing::get,
};
use shared::{ServerMsg, Table};
use sqlx::postgres::PgPool;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::server::{OpApply, Session, SnapshotSource, current_cursor, pull_since};

/// Ticker cadence: stream any server changes to connected sockets. The
/// mechanism is a DB poll today — ROADMAP 2.2 (Postgres pub/sub)
/// replaces the mechanism without touching the loop shape.
const STREAM_TICK: Duration = Duration::from_millis(500);

/// The WS route any axum backend mounts, e.g.
/// `Router::new().merge(sync_router(db, Apply, Snapshots))`. The
/// plug-ins are the app's [`OpApply`] + [`SnapshotSource`] impls (ZSTs —
/// cloned per connection; `Send + Sync` because `&self` crosses awaits
/// in the session and the on-upgrade future must be Send).
pub fn sync_router<A, S>(db: PgPool, applier: A, source: S) -> Router
where
    A: OpApply + Clone + Send + Sync + 'static,
    S: SnapshotSource<Table> + Clone + Send + Sync + 'static,
{
    let route = move |ws: WebSocketUpgrade| async move {
        ws.on_upgrade(move |socket| {
            handle_socket(socket, db.clone(), applier.clone(), source.clone())
        })
    };
    Router::new().route("/sync", get(route))
}

async fn handle_socket<A: OpApply + Send + Sync, S: SnapshotSource<Table> + Send + Sync>(
    mut socket: WebSocket,
    db: PgPool,
    applier: A,
    source: S,
) {
    // Per-connection protocol state + the app's table plug-ins; the
    // decisions live in the transport-free `Session`.
    let mut session = Session::new(applier, source);
    // Where this connection has streamed so far; starts at the current
    // server cursor so we only push changes that happen *during* this
    // connection. Older events arrive via explicit Pull.
    let mut stream_cursor = current_cursor(&db).await;

    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(64);

    // Ticker: stream any server changes to this connection.
    let ticker_db = db.clone();
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(STREAM_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match pull_since(&ticker_db, stream_cursor).await {
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
                        // One poisoned message must not kill the stream:
                        // errors are logged, the connection stays.
                        match session.on_text(&db, &text).await {
                            Ok(Some(reply)) => {
                                if !send_msg(&mut socket, &reply).await {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!("sync error: {e}"),
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
