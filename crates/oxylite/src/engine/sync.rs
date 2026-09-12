//! The engine's connection lifecycle: the leader's run loop (election,
//! epoch gate, reconnect backoff), one connected session, the incoming
//! message handling (handshake, Ack retirement, Events/Snapshot apply),
//! and the durable op log (write-ahead push + connect flush). The socket
//! helpers and the cursor persistence close it out.

use uuid::Uuid;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::WebSocket;

use super::relay::{Role, TabMsg};
use super::{Engine, EngineError, engine, log, timer_pause};
use crate::SCHEMA;
use crate::contract::table::SyncTableWire;
use crate::pglite::{self, Pglite};
use crate::protocol::{ClientMsg, Op, ServerMsg};
use crate::timestamp::Timestamp;

use super::tabs;

impl<T: SyncTableWire> Engine<T> {
    /// The single long-lived task: claim the browser's engine slot, then
    /// either lead (socket + DB) or follow until the leader dies.
    pub async fn run(&self) {
        if self.tabs_channel.borrow().is_none() {
            // No BroadcastChannel: tab coordination is impossible. Every
            // tab self-leads — today's per-tab engines, clobbers and all.
            self.run_leader().await;
            return;
        }
        match tabs::try_acquire().await {
            Some(_hold) => self.run_leader().await,
            None => {
                self.role.set(Some(Role::Follower));
                self.set_status_relayed("the leader tab owns sync…");
                self.post(&TabMsg::Hello);
                log("tabs", "subordinate tab — following the leader");
                // Queued with the browser from this moment; resolves when
                // the leader's document dies and the lock comes to us.
                tabs::wait_acquire().await;
                log("tabs", "promoted to leader — the previous one died");
                self.run_leader().await
            }
        }
    }

    /// The leader's lifetime: gate the local DB's schema epoch, open THE
    /// PGlite instance, serve subordinate tab requests, and run the
    /// connect/pull/push loop until the tab dies.
    async fn run_leader(&self) {
        self.role.set(Some(Role::Leader));
        // The epoch gate runs before PGlite boots: the IDB version is the
        // GENERATED id (the migration list's length — monotonic, only
        // ever appends), and a stored version newer than this bundle's
        // means this wasm predates the local DB it cannot read (#34).
        match pglite::ensure_local_compat(pglite::DATA_DIR, self.migrations.len() as u32).await {
            Ok(()) => {}
            Err(e) => {
                log(
                    "sync",
                    &format!("local db incompatible with this build: {e:?}"),
                );
                if reload_once() {
                    return; // reloading — the new bundle re-boots cleanly
                }
                // Already reloaded once and still stale: the banner IS the
                // message (D1). The tab stays inert — its DB is unreadable.
                self.set_status(
                    "stale build — this tab predates the local database; hard refresh required",
                );
                return;
            }
        }
        let db = match Pglite::init(self.migrations).await {
            Ok(p) => p,
            Err(e) => {
                self.set_status(&format!("pglite failed: {e}"));
                return;
            }
        };
        log("boot", "pglite ready — this tab is the engine's leader");
        self.cursor.set(load_cursor(&db).await);
        self.set_status("offline — local data loaded");
        log("boot", &self.debug());

        // One dedicated task serves every subordinate's query/write/push
        // for the lifetime of the leadership (it re-joins the PGlite
        // singleton, so it never races `run` for initialization).
        let servicer = engine::<T>();
        wasm_bindgen_futures::spawn_local(async move {
            servicer.serve_tabs().await;
        });

        loop {
            self.set_status("connecting…");
            match open_socket::<T>(&sync_url()) {
                Ok(sock) => {
                    *self.sock.borrow_mut() = Some(sock.clone());
                    self.set_status("connected");
                    log("sync", "connected");
                    self.session(&db, &sock).await;
                    *self.sock.borrow_mut() = None;
                    log("sync", "disconnected — retrying in 3s");
                    self.set_status("offline — will retry…");
                    timer_pause(3000).await;
                }
                Err(e) => {
                    log("sync", &format!("connect failed: {e:?}"));
                    self.set_status("offline — will retry…");
                    timer_pause(3000).await;
                }
            }
        }
    }

    /// One connected session: flush the initial pull + pending pushes, then
    /// drain the inbox until the socket closes (or the handshake times out).
    async fn session(&self, db: &Pglite, sock: &WebSocket) {
        let mut flushed = false;
        let mut connecting_ms = 0u32;
        loop {
            match sock.ready_state() {
                WebSocket::CONNECTING => {
                    // A handshake can hang (firewall, proxy, mock); give up
                    // and let `run` reconnect rather than waiting forever.
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
                        // Handshake first: a version-mismatched session
                        // ends before any Pull/Push work is wasted (#34).
                        self.send(&ClientMsg::Hello {
                            version: self.version.clone(),
                        });
                        self.send(&ClientMsg::Pull {
                            since: self.cursor.get(),
                        });
                        self.flush_pending(db).await;
                    }
                    let messages = std::mem::take(&mut *self.inbox.borrow_mut());
                    for msg in messages {
                        self.handle_msg(db, msg).await;
                    }
                    timer_pause(150).await;
                }
                _ => return, // closed or closing: reconnect in `run`
            }
        }
    }

    async fn handle_msg(&self, db: &Pglite, msg: ServerMsg<T>) {
        match msg {
            ServerMsg::Ready { version } => {
                log("sync", &format!("schema handshake ok — server {version}"));
            }
            ServerMsg::Incompatible { server, client } => {
                // The wire gate (D2): this bundle's major.minor is behind
                // (or ahead of) the server's. One guarded reload picks up
                // the new bundle AND its migrations; failing that, the
                // status line is the banner.
                log(
                    "sync",
                    &format!("schema mismatch — server {server}, this build {client}: reloading"),
                );
                self.set_status("schema updated — reloading…");
                if !reload_once() {
                    self.set_status(
                        "stale build — reload did not resolve a schema mismatch; hard refresh required",
                    );
                }
            }
            ServerMsg::Ack { batch, .. } => {
                // The server confirmed this batch: retire its pending
                // rows. A failed delete is logged — the next flush
                // resends the batch (LWW no-op) and acks again.
                if let Err(e) = db
                    .query(
                        &format!("DELETE FROM {SCHEMA}.pending_ops WHERE batch_id = $1"),
                        &[batch.to_string()],
                    )
                    .await
                {
                    log("sync", &format!("ack retirement failed: {e}"));
                }
                // The server may hold events we haven't seen; pull again.
                self.send(&ClientMsg::Pull {
                    since: self.cursor.get(),
                });
            }
            ServerMsg::Events { events, cursor: c } => {
                log(
                    "sync",
                    &format!("received {} events, cursor -> {}", events.len(), c),
                );
                // Every event already IS the invalidation notice — its
                // (table, row_id) rides along with the payload, so phase 1
                // needs no extra protocol messages (phase 2 can send a
                // payload-less Invalidate instead).
                let mut unique: Vec<T> = Vec::new();
                for op in &events {
                    if !unique.contains(&op.table) {
                        unique.push(op.table);
                    }
                }
                let mut touched = Vec::with_capacity(events.len());
                let mut failed: Option<EngineError> = None;
                for table in unique {
                    let ops: Vec<Op<T>> = events
                        .iter()
                        .filter(|o| o.table == table)
                        .cloned()
                        .collect();
                    match self.apply_batch(db, table, &ops).await {
                        Ok(ids) => touched.extend(ids.into_iter().map(|id| (table, id))),
                        Err(e) => {
                            log("sync", &format!("apply failed: {e}"));
                            failed = Some(e);
                        }
                    }
                }
                // One bump per batch, not per op: replaying a backlog must
                // re-run each query once, not once per row (the UI would
                // visibly re-render row by row).
                self.bump(&touched);
                if let Some(e) = failed {
                    // Apply failed: the cursor must NOT advance past data
                    // the client never wrote (the old order — save, then
                    // apply — permanently skipped anything a crash or a
                    // failing chunk interrupted). The batch stays pending:
                    // the next ack or reconnect re-pulls it (at-least-once,
                    // LWW-idempotent), and LAST_ERROR keeps the stuck batch
                    // visible instead of silently skipped.
                    self.set_last_error(e);
                    return;
                }
                self.cursor.set(c);
                save_cursor(db, c).await;
                // The batch is a window into the backlog: keep pulling
                // until the server returns an empty one. (At-least-once;
                // LWW makes replays idempotent.)
                if !events.is_empty() {
                    self.send(&ClientMsg::Pull { since: c });
                }
            }
            ServerMsg::Snapshot {
                seq,
                tables,
                tombstones,
            } => {
                let rows_count: usize = tables.iter().map(|t| t.rows.len()).sum();
                log(
                    "sync",
                    &format!(
                        "snapshot at {seq}: {rows_count} rows + {} tombstones",
                        tombstones.len()
                    ),
                );
                // Any apply failure fails the WHOLE snapshot — the Events
                // contract (#33) one layer up. The tombstones gate the
                // guarded upserts (applying rows past a failed tombstone
                // batch risks resurrections), and a half-applied snapshot
                // must never advance the cursor past data the client
                // didn't write: the old order logged, advanced, and
                // pulled from above every real row — rows stranded
                // silently. The retry is the reconnect: the cursor keeps
                // its old value, so the fresh session's Pull re-enters
                // the server's snapshot/replay decision (its
                // once-per-connection flag is spent on THIS session,
                // which is why the socket closes here — a healthy socket
                // would otherwise idle forever). LAST_ERROR keeps the
                // reason visible; a persistently poisoned snapshot loops
                // visibly instead of wedging or diverging.
                let mut failed: Option<EngineError> = None;
                if let Err(e) = crate::query::apply_tombstones(db, &tombstones).await {
                    log("sync", &format!("snapshot tombstones apply failed: {e}"));
                    failed = Some(e);
                }
                let mut touched = Vec::with_capacity(rows_count);
                for table_data in &tables {
                    // Snapshot rows are payload-shaped live rows: upserts
                    // by construction (deleted rows are excluded
                    // server-side). `id`/`updated_at` are read from the
                    // payload by the sink's row parsing.
                    let ops: Vec<Op<T>> = table_data
                        .rows
                        .iter()
                        .map(|row| Op {
                            table: table_data.table,
                            id: Uuid::nil(),
                            data: row.clone(),
                            // Snapshot rows re-enter as upserts. For
                            // upserts the applier never reads the
                            // envelope timestamp — the payload's own
                            // updated_at binds (and serde-validated on
                            // the way in) — so this epoch placeholder is
                            // inert; a parse here just keeps the type
                            // honest instead of smuggling a raw String.
                            updated_at: Timestamp::parse("1970-01-01T00:00:00.000Z").unwrap(),
                        })
                        .collect();
                    match self.apply_batch(db, table_data.table, &ops).await {
                        Ok(ids) => {
                            touched.extend(ids.into_iter().map(|id| (table_data.table, id)));
                        }
                        Err(e) => {
                            log("sync", &format!("snapshot apply failed: {e}"));
                            failed = Some(e);
                        }
                    }
                }
                if let Some(e) = failed {
                    self.set_last_error(e);
                    self.close_socket();
                    return;
                }
                self.cursor.set(seq);
                save_cursor(db, seq).await;
                self.bump(&touched);
                // The snapshot may already be behind the live log head —
                // pull the remainder right away (same re-pull as Ack).
                self.send(&ClientMsg::Pull { since: seq });
            }
        }
    }

    pub(super) async fn push_local(&self, op: Op<T>) {
        // The op log must be reachable for EVERY push now (write-ahead):
        // its row is what makes the send survivable.
        let db = match Pglite::init(self.migrations).await {
            Ok(db) => db,
            Err(e) => {
                log("sync", &format!("op log unavailable: {e}"));
                self.set_last_error(EngineError::Sink(format!(
                    "write could not be persisted: {e}"
                )));
                return;
            }
        };
        let json = match serde_json::to_string(&op) {
            Ok(json) => json,
            Err(e) => {
                log("sync", &format!("op serialization failed: {e}"));
                self.set_last_error(EngineError::Sink(format!(
                    "write could not be serialized: {e}"
                )));
                return;
            }
        };
        // One batch id per push: the invariant that lets an Ack retire
        // exactly the row(s) it confirms (flush sends one row per
        // message under its own batch id). v7: time-ordered, so a
        // batch's log position roughly tracks its creation.
        let batch = Uuid::now_v7();
        if let Err(e) = db
            .query(
                &format!("INSERT INTO {SCHEMA}.pending_ops (op, batch_id) VALUES ($1, $2)"),
                &[json, batch.to_string()],
            )
            .await
        {
            log("sync", &format!("op log write failed: {e}"));
            self.set_last_error(EngineError::Sql(e));
            return;
        }
        let open = self
            .sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
        if open {
            self.send(&ClientMsg::Push {
                ops: vec![op],
                batch,
            });
        }
        // Not open: the row waits for the connect flush. Either way the
        // Ack — not this send — decides when the row is retired.
    }

    /// Drain the durable op log on connect: resend every pending batch.
    /// Rows are NOT deleted here — retirement is the Ack's job
    /// (`handle_msg`): a send the socket accepted tells us nothing about
    /// whether the server read it. An unacked batch simply resends on
    /// every connect until acknowledged; LWW makes the replay a no-op.
    /// One row per message under its own batch id (the persist-first
    /// invariant: a batch is exactly one push_local call).
    async fn flush_pending(&self, db: &Pglite) {
        let rows = match db
            .query(
                &format!(
                    "SELECT seq::text, op, COALESCE(batch_id::text, '') AS batch_id \
                     FROM {SCHEMA}.pending_ops ORDER BY seq"
                ),
                &[],
            )
            .await
        {
            Ok(result) => pglite::rows_of(&result),
            Err(e) => {
                log("sync", &format!("op log read failed: {e}"));
                return;
            }
        };
        for row in &rows {
            // A row the mapper can't read must not wedge the flush: log
            // the reason (now typed — missing vs null vs wrong shape) and
            // keep it for the next connect.
            let seq = match row.field_req::<String>("seq") {
                Ok(seq) => seq,
                Err(e) => {
                    log("sync", &format!("op log row unreadable — keeping it: {e}"));
                    continue;
                }
            };
            let op: Op<T> = match row
                .field_req::<String>("op")
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
            {
                Some(op) => op,
                None => {
                    log("sync", &format!("op log row {seq} unparsable — keeping it"));
                    continue;
                }
            };
            // batch_id is nullable in the schema (ALTER-added), so the
            // null-aware read: None lands in the keep-it branch too.
            let batch = match row
                .field::<String>("batch_id")
                .ok()
                .flatten()
                .and_then(|b| b.parse().ok())
            {
                Some(batch) => batch,
                None => {
                    // Unreachable since migration 0009 backfills ids —
                    // a row without one must not fly under a fabricated
                    // id that could collide with a live batch's ack.
                    log(
                        "sync",
                        &format!("op log row {seq} has no batch id — keeping it"),
                    );
                    continue;
                }
            };
            // A refused send keeps its row; the next connect retries.
            self.send(&ClientMsg::Push {
                ops: vec![op],
                batch,
            });
        }
    }

    /// Entry point for the socket's onmessage callback. Runs outside any
    /// dioxus scope, so it must never spawn: it parses and enqueues only.
    pub(super) fn recv_text(&self, text: &str) {
        match serde_json::from_str::<ServerMsg<T>>(text) {
            Ok(msg) => self.inbox.borrow_mut().push(msg),
            Err(e) => log("sync", &format!("bad message: {e}")),
        }
    }

    fn send(&self, msg: &ClientMsg<T>) {
        let _ = self.send_ok(msg);
    }

    /// Serialize and hand to the socket buffer. `Ok` means the frame was
    /// ACCEPTED (buffered), not delivered — see `flush_pending` for the
    /// durability consequence.
    fn send_ok(&self, msg: &ClientMsg<T>) -> bool {
        let Ok(text) = serde_json::to_string(msg) else {
            return false;
        };
        self.sock
            .borrow()
            .as_ref()
            .is_some_and(|ws| ws.send_with_str(&text).is_ok())
    }

    /// Drop the sync socket and let `run` reconnect after its backoff —
    /// the failure-retry driver for a session that cannot make progress
    /// (a snapshot that failed to apply: the server's once-per-connection
    /// snapshot flag is spent, so only a fresh session re-serves it, and
    /// the kept cursor makes that fresh session re-request the state).
    fn close_socket(&self) {
        if let Some(ws) = self.sock.borrow_mut().take() {
            let _ = ws.close();
        }
    }
}

/// Reload this tab exactly ONCE per browsing session: the guarded
/// force-update (D1 — "just try and reload the wasm binary"). The
/// sessionStorage flag breaks any loop a bad cache or a stuck server
/// could cause; callers fall back to a banner when `false` comes back.
fn reload_once() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let storage = window.session_storage().ok().flatten();
    if storage
        .as_ref()
        .is_some_and(|s| s.get_item(RELOAD_FLAG).ok().flatten().is_some())
    {
        return false;
    }
    if let Some(s) = &storage {
        let _ = s.set_item(RELOAD_FLAG, "1");
    }
    let _ = window.location().reload();
    true
}

const RELOAD_FLAG: &str = "oxylite-reload-once";

/// Open the sync websocket; parsed messages are enqueued via
/// [`Engine::recv_text`] and everything else is driven by `run`.
fn open_socket<T: SyncTableWire>(url: &str) -> Result<WebSocket, JsValue> {
    let ws = WebSocket::new(url)?;

    let onmessage =
        wasm_bindgen::closure::Closure::wrap(Box::new(move |e: web_sys::MessageEvent| {
            if let Some(text) = e.data().as_string() {
                engine::<T>().recv_text(&text);
            }
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    Ok(ws)
}

/// Sync URL derived from the current page origin (single-origin deployment:
/// the client, the vendored PGlite bundle and the WS all live on the server).
fn sync_url() -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_else(|| "http://localhost:3000".into());
    format!("{}/sync", origin.replacen("http", "ws", 1))
}

/// Persisted sync cursor helpers (stored in PGlite's meta table so they
/// survive reloads).
async fn load_cursor(pglite: &Pglite) -> i64 {
    pglite
        .query(
            &format!("SELECT value FROM {SCHEMA}.meta WHERE key = 'cursor'"),
            &[],
        )
        .await
        .ok()
        .and_then(|r| {
            pglite::rows_of(&r)
                .first()
                .and_then(|row| row.field_req::<String>("value").ok()?.parse().ok())
        })
        .unwrap_or(-1)
}

async fn save_cursor(pglite: &Pglite, cursor: i64) {
    let _ = pglite
        .query(
            &format!(
                "INSERT INTO {SCHEMA}.meta (key, value) VALUES ('cursor', $1) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
            ),
            &[cursor.to_string()],
        )
        .await;
}
