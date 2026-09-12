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
use tokio::time::timeout;

use crate::contract::table::SyncTableWire;
use crate::protocol::{SchemaVersion, ServerMsg};
use crate::server::{OpApply, Session, SnapshotSource, SyncError, current_cursor, pull_since};

/// Ticker cadence: stream any server changes to connected sockets. The
/// mechanism is a DB poll today — ROADMAP 2.2 (Postgres pub/sub)
/// replaces the mechanism. The loop shape's invariant: a session that
/// cannot READ the log ends itself (stream failures are surfaced as a
/// disconnect, never streamed past silently — 2026-09-12).
const STREAM_TICK: Duration = Duration::from_millis(500);

/// Bound on ONE ticker pull: a vanished DB (stopped container, dropped
/// forward) does not always RST established connections — the query can
/// hang silently on a black-holed socket, which is WORSE than an error
/// (nothing surfaces at all). The bound turns the hang into a stream
/// failure like any other. 2s = 4 ticks.
const TICKER_PULL_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound on ONE client-driven DB operation inside the session (the
/// Push/Pull handlers, snapshot serving). Same reasoning: a hung
/// handler would wedge the whole select! loop (the Dead tick could
/// never be processed) and strand the client waiting for an Ack that
/// never comes. Generous — a push transaction may legitimately take a
/// moment — but finite.
const SESSION_OP_TIMEOUT: Duration = Duration::from_secs(5);

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

/// What the ticker hands the socket loop. `Dead` is the stream-failure
/// signal: the session must END, because a session that cannot read the
/// log is half-alive — it looks connected to the client while silently
/// starving it (the server-side twin of the #36 snapshot lesson). The
/// client's existing reconnect machinery is the surface: disconnect →
/// retry → a persistent failure renders as `offline — will retry…`
/// instead of a healthy-looking dead stream. No wire message needed.
enum Tick<T: SyncTableWire> {
    Msg(ServerMsg<T>),
    Dead,
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
    // connection. Older events arrive via explicit Pull. Bound like
    // every other DB op: on a vanished DB this returns 0 and the
    // ticker's own timeout ends the session momentarily.
    let mut stream_cursor = timeout(TICKER_PULL_TIMEOUT, current_cursor(&db))
        .await
        .unwrap_or(0);

    let (out_tx, mut out_rx) = mpsc::channel::<Tick<T>>(64);

    // Ticker: stream any server changes to this connection.
    let ticker_db = db.clone();
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(STREAM_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // The pull is bound: an error OR a hang (black-holed DB) is
            // a stream failure — printed for the operator (structured
            // logging is 3.2) and surfaced as Dead, ending the session
            // so the client reconnects into a truth it can see.
            match timeout(
                TICKER_PULL_TIMEOUT,
                pull_since::<T>(&ticker_db, stream_cursor),
            )
            .await
            {
                Ok(Ok((events, cursor))) if !events.is_empty() => {
                    stream_cursor = cursor;
                    if out_tx
                        .send(Tick::Msg(ServerMsg::Events { events, cursor }))
                        .await
                        .is_err()
                    {
                        break; // socket loop gone — nothing left to feed
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    eprintln!("stream error: {e}");
                    let _ = out_tx.send(Tick::Dead).await;
                    break;
                }
                Err(_) => {
                    // Elapsed — the pull hung, the worst kind of silent.
                    eprintln!("stream timeout: pull did not complete in {TICKER_PULL_TIMEOUT:?}");
                    let _ = out_tx.send(Tick::Dead).await;
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            msg = out_rx.recv() => match msg {
                Some(Tick::Msg(msg)) => {
                    if !send_msg(&mut socket, &msg).await {
                        break;
                    }
                }
                // Dead: the stream failed server-side. None: the ticker
                // is gone (its sender dropped) — same conclusion.
                Some(Tick::Dead) | None => break,
            },
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // The handler is bound like every DB op: a hang
                        // would wedge this select! loop (the ticker's
                        // Dead could never be processed) and strand the
                        // client waiting for an Ack that never comes.
                        // Elapsed therefore ends the session like any
                        // other DB failure.
                        match timeout(SESSION_OP_TIMEOUT, session.on_text(&db, &text)).await {
                            Ok(Ok(Some(reply))) => {
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
                            Ok(Ok(None)) => {}
                            Ok(Err(e)) => {
                                // The same rule as the ticker's Dead:
                                // a DB-level failure means the session
                                // cannot do its job — a Push whose
                                // apply failed would strand the client
                                // waiting for an Ack that never comes.
                                // Client-garbage (Json) stays: one
                                // poisoned message must not kill the
                                // stream, and at-least-once makes
                                // skipping harmless.
                                eprintln!("sync error: {e}");
                                if matches!(e, SyncError::Sql(_)) {
                                    break;
                                }
                            }
                            Err(_) => {
                                eprintln!(
                                    "sync timeout: handler did not complete in {SESSION_OP_TIMEOUT:?}"
                                );
                                break;
                            }
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
