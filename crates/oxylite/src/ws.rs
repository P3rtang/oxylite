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
use sqlx::postgres::PgPool;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::protocol::{SchemaVersion, ServerMsg};
use crate::server::{OpApply, Session, SnapshotSource, current_cursor, pull_since};
use crate::table::SyncTableWire;

/// Ticker cadence: stream any server changes to connected sockets. The
/// mechanism is a DB poll today — ROADMAP 2.2 (Postgres pub/sub)
/// replaces the mechanism without touching the loop shape.
const STREAM_TICK: Duration = Duration::from_millis(500);

/// The WS route any axum backend mounts, e.g.
/// `Router::new().merge(sync_router(db, Apply, Snapshots))`. The
/// plug-ins are the app's [`OpApply`] + [`SnapshotSource`] impls (ZSTs —
/// cloned per connection; `Send + Sync` because `&self` crosses awaits
/// in the session and the on-upgrade future must be Send).
pub fn sync_router<T, A, S>(db: PgPool, applier: A, source: S, version: SchemaVersion) -> Router
where
    T: SyncTableWire,
    A: OpApply<T> + Clone + Send + Sync + 'static,
    S: SnapshotSource<T> + Clone + Send + Sync + 'static,
{
    let route = move |ws: WebSocketUpgrade| async move {
        ws.on_upgrade(move |socket| {
            handle_socket::<T, A, S>(
                socket,
                db.clone(),
                applier.clone(),
                source.clone(),
                version.clone(),
            )
        })
    };
    Router::new().route("/sync", get(route))
}

async fn handle_socket<
    T: SyncTableWire,
    A: OpApply<T> + Send + Sync,
    S: SnapshotSource<T> + Send + Sync,
>(
    mut socket: WebSocket,
    db: PgPool,
    applier: A,
    source: S,
    version: SchemaVersion,
) {
    // Per-connection protocol state + the app's table plug-ins; the
    // decisions live in the transport-free `Session`.
    let mut session = Session::new(applier, source, version);
    // Where this connection has streamed so far; starts at the current
    // server cursor so we only push changes that happen *during* this
    // connection. Older events arrive via explicit Pull.
    let mut stream_cursor = current_cursor(&db).await;

    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg<T>>(64);

    // Ticker: stream any server changes to this connection.
    let ticker_db = db.clone();
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(STREAM_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match pull_since::<T>(&ticker_db, stream_cursor).await {
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
                                // An Incompatible handshake closes the
                                // session right after the frame lands —
                                // the client reloads (#34).
                                let incompatible = matches!(reply, ServerMsg::Incompatible { .. });
                                if !send_msg(&mut socket, &reply).await {
                                    break;
                                }
                                if incompatible {
                                    // Dropping the socket closes it; the
                                    // frame already landed.
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
async fn send_msg<T: SyncTableWire>(socket: &mut WebSocket, msg: &ServerMsg<T>) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}
