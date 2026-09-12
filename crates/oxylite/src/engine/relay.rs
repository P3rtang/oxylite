//! The cross-tab layer of the engine: election roles, the
//! BroadcastChannel protocol, and both sides of the proxy — the leader
//! serving subordinate requests, subordinates forwarding theirs. The
//! transport primitives (Web Locks, the channel itself) live in
//! [`crate::tabs`]; this module is what the ENGINE does with them.

use dioxus::prelude::WritableExt;
use uuid::Uuid;
use wasm_bindgen::JsValue;

use super::{Engine, EngineError, LAST_ERROR, STATUS, log, timer_pause};
use crate::contract::from_row::FromRow;
use crate::contract::table::SyncTableWire;
use crate::pglite::{self, BridgeError, Pglite};
use crate::protocol::Op;
use crate::query::Query;

/// This tab's role in the browser. Exactly one leader holds the Web Lock;
/// subordinates proxy every DB access to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Leader,
    Follower,
}

/// Tab messages over BroadcastChannel (JSON, like the websocket wire).
/// Requests flow subordinate → leader; replies and broadcasts flow back.
///
/// Deliberately T-FREE (#31): the relay rides the tables' WIRE NAMES —
/// the same identity the server logs — and the pushed op as raw JSON.
/// The leader re-attaches the typed table via `T::from_name` when it
/// acts. This keeps the derive's serde bounds out of generic territory
/// (a `T`-generic derive here fights the `DeserializeOwned` bound —
/// E0283) and makes the cross-tab plumbing reusable as-is.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) enum TabMsg {
    /// Run a read in the leader's PGlite and ship the raw result back.
    Query {
        id: u64,
        sql: String,
        params: Vec<String>,
    },
    /// Run a local write in the leader's PGlite (the only live instance).
    Exec {
        id: u64,
        sql: String,
        params: Vec<String>,
        touched: Vec<(String, Uuid)>,
    },
    /// Deliver an op through the leader's socket/pending queue: the
    /// op's wire JSON, re-parsed into `Op<T>` by the leader.
    Push { op: serde_json::Value },
    /// A newly-subordinate tab asks for the leader's current status.
    Hello,
    /// Query reply: the PGlite result JSON-stringified (the same shape a
    /// direct call would return), or the boundary error that failed it.
    Rows {
        id: u64,
        result: Result<String, BridgeError>,
    },
    /// Exec reply: the write committed (and the bump went out), or failed.
    ExecDone {
        id: u64,
        result: Result<(), BridgeError>,
    },
    /// Invalidation fan-out: local echo. A write anywhere re-runs the
    /// matching live queries in every tab.
    Bump { touched: Vec<(String, Uuid)> },
    /// The leader's connection state, so every tab shows the same thing.
    Status { text: String },
    /// Relay of an apply failure so the notice overlay works everywhere.
    ApplyError { error: EngineError },
}

impl<T: SyncTableWire> Engine<T> {
    /// Block until the election in `run` has decided this tab's role.
    pub(super) async fn wait_role(&self) -> Role {
        loop {
            if let Some(role) = self.role.get() {
                return role;
            }
            timer_pause(25).await;
        }
    }

    /// Post a message to the browser's other tabs (no-op without a
    /// channel — the degraded per-tab-engine mode).
    pub(super) fn post(&self, msg: &TabMsg) {
        if let Ok(text) = serde_json::to_string(msg)
            && let Some(channel) = self.tabs_channel.borrow().as_ref()
        {
            channel.post(&text);
        }
    }

    /// Await the reply to one of this tab's subordinate requests. `None`
    /// means the leader died (or stalled) — callers retry.
    async fn await_tab(&self, id: u64) -> Option<TabMsg> {
        let mut waited = 0u32;
        loop {
            if let Some(reply) = self.tab_replies.borrow_mut().remove(&id) {
                return Some(reply);
            }
            if waited >= 5000 {
                return None;
            }
            timer_pause(20).await;
            waited += 20;
        }
    }

    pub(super) fn set_status(&self, text: &str) {
        *STATUS.write_unchecked() = text.into();
        *self.status_text.borrow_mut() = text.to_string();
        // The leader is the authority: fan the state out to every tab.
        self.post(&TabMsg::Status {
            text: text.to_string(),
        });
    }

    /// Subordinates render the leader's state behind a role marker: sync
    /// is delegated to another tab, and tests can tell the roles apart.
    pub(super) fn set_status_relayed(&self, text: &str) {
        let shown = format!("subordinate — {text}");
        *STATUS.write_unchecked() = shown.clone();
        *self.status_text.borrow_mut() = shown;
    }

    pub(super) fn set_last_error(&self, error: EngineError) {
        *LAST_ERROR.write_unchecked() = Some(error.clone());
        if self.role.get() == Some(Role::Leader) {
            self.post(&TabMsg::ApplyError { error });
        }
    }

    /// Entry point for the BroadcastChannel callback. Same discipline as
    /// `recv_text`: parse and either enqueue (requests for the leader's
    /// servicer) or apply synchronously (replies and broadcasts — bump,
    /// status, errors are all synchronous work).
    pub(super) fn recv_tab(&self, text: &str) {
        let msg = match serde_json::from_str::<TabMsg>(text) {
            Ok(msg) => msg,
            Err(e) => {
                log("tabs", &format!("bad tab message: {e}"));
                return;
            }
        };
        match &msg {
            // Requests: the leader's servicer drains these.
            TabMsg::Query { .. } | TabMsg::Exec { .. } | TabMsg::Push { .. } | TabMsg::Hello => {
                if self.role.get() == Some(Role::Leader) {
                    self.tab_inbox.borrow_mut().push(msg);
                } else {
                    log("tabs", "request arrived while not leading — dropped");
                }
            }
            // Replies to this tab's own subordinate requests.
            TabMsg::Rows { id, .. } | TabMsg::ExecDone { id, .. } => {
                self.tab_replies.borrow_mut().insert(*id, msg);
            }
            // Broadcasts, applied inline.
            TabMsg::Bump { touched } => {
                let touched = touched_from_names::<T>(touched.clone());
                self.bump(&touched);
            }
            TabMsg::Status { text } => self.set_status_relayed(text),
            TabMsg::ApplyError { error } => {
                *LAST_ERROR.write_unchecked() = Some(error.clone());
            }
        }
    }

    /// A subordinate's read: the leader runs it in THE PGlite instance and
    /// ships the raw result back as JSON. The leader can die mid-request
    /// (tab close → re-election in flight); requests are read-only, so
    /// re-asking is safe and the fresh leader answers.
    pub(super) async fn query_remote<R: FromRow>(&self, q: &Query) -> Result<Vec<R>, EngineError> {
        loop {
            let id = self.next_req.get();
            self.next_req.set(id + 1);
            self.post(&TabMsg::Query {
                id,
                sql: q.sql.clone(),
                params: q.params.clone(),
            });
            if let Some(TabMsg::Rows { result, .. }) = self.await_tab(id).await {
                return match result {
                    Ok(json) => {
                        let parsed: JsValue = js_sys::JSON::parse(&json)
                            .map_err(|e| EngineError::Sql(BridgeError::from_rejection(&e)))?;
                        pglite::rows_of(&parsed)
                            .iter()
                            .map(|row| R::from_row(row))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(EngineError::from)
                    }
                    Err(e) => Err(EngineError::Sql(e)),
                };
            }
            log("tabs", "query timed out — leader died? asking again");
        }
    }

    /// A subordinate's write: forwarded to the leader, which owns the only
    /// live PGlite instance (two instances over one IndexedDB lose writes
    /// — impl/multi-tab.md). Retried on leader death: a write that lost
    /// its leader either never ran (retry is required) or committed to
    /// IndexedDB before the tab died (the retry surfaces a duplicate-key
    /// error — harmless, LWW syncs the row back from the server).
    pub(super) async fn exec_remote(
        &self,
        sql: &str,
        params: &[String],
        touched: &[(T, Uuid)],
    ) -> Result<(), EngineError> {
        loop {
            let id = self.next_req.get();
            self.next_req.set(id + 1);
            self.post(&TabMsg::Exec {
                id,
                sql: sql.into(),
                params: params.to_vec(),
                // The relay rides wire names; the leader re-attaches the
                // typed table via `from_name`.
                touched: touched
                    .iter()
                    .map(|(t, r)| (t.as_str().to_string(), *r))
                    .collect(),
            });
            if let Some(TabMsg::ExecDone { result, .. }) = self.await_tab(id).await {
                return result.map_err(EngineError::Sql);
            }
            log("tabs", "write timed out — leader died? asking again");
        }
    }

    /// Serve subordinate tab requests: every DB access in the browser
    /// funnels through the leader's PGlite instance.
    pub(super) async fn serve_tabs(&self) {
        loop {
            let msgs = std::mem::take(&mut *self.tab_inbox.borrow_mut());
            for msg in msgs {
                self.serve_tab(msg).await;
            }
            timer_pause(50).await;
        }
    }

    async fn serve_tab(&self, msg: TabMsg) {
        let db = match Pglite::init(self.migrations).await {
            Ok(db) => db,
            Err(e) => {
                log("tabs", &format!("db unavailable for tab request: {e}"));
                return;
            }
        };
        match msg {
            // A subordinate just joined: tell it where sync stands (it has
            // no other way to learn a status the leader set before it
            // existed).
            TabMsg::Hello => self.post(&TabMsg::Status {
                text: self.status_text.borrow().clone(),
            }),
            TabMsg::Query { id, sql, params } => {
                let result = match db.query(&sql, &params).await {
                    Ok(value) => match js_sys::JSON::stringify(&value) {
                        Ok(s) => Ok(JsValue::from(s).as_string().unwrap_or_default()),
                        Err(e) => Err(BridgeError::from_rejection(&e)),
                    },
                    Err(e) => Err(e),
                };
                self.post(&TabMsg::Rows { id, result });
            }
            TabMsg::Exec {
                id,
                sql,
                params,
                touched,
            } => {
                let touched = touched_from_names::<T>(touched);
                let result = match db.query(&sql, &params).await {
                    Ok(_) => {
                        self.bump(&touched);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                self.post(&TabMsg::ExecDone { id, result });
            }
            TabMsg::Push { op } => match serde_json::from_value::<Op<T>>(op) {
                Ok(op) => self.push_local(op).await,
                Err(e) => log("tabs", &format!("relayed op unparsable: {e}")),
            },
            // Replies and broadcasts are addressed to subordinates.
            _ => log("tabs", "reply/broadcast arrived at the leader — dropped"),
        }
    }
}

/// Re-attach typed tables to relayed touched pairs: the tab wire rides
/// wire names (`SyncTable::as_str`), the engine's internals are typed.
/// Unknown names (from a newer app version) are skipped — the compat
/// boundary, same rule as server replay.
fn touched_from_names<T: SyncTableWire>(touched: Vec<(String, Uuid)>) -> Vec<(T, Uuid)> {
    touched
        .into_iter()
        .filter_map(|(name, id)| Some((T::from_name(&name)?, id)))
        .collect()
}
