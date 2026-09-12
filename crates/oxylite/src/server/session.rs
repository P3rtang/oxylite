//! Per-connection protocol decisions, transport-free: everything a
//! backend's WS loop needs EXCEPT the sockets. Holds the app's plug-ins
//! (apply arms + snapshot extraction) at construction, so the transport
//! loop stays a thin `select!` over [`Session::on_text`] and its ticker
//! (the axum flavor is [`crate::ws`]; other frameworks mount their own
//! loop over the same core).

use sqlx::postgres::PgPool;
use std::marker::PhantomData;

use super::SyncError;
use super::ops::{OpApply, current_cursor, log_floor, pull_since, push};
use super::snapshot::{SNAPSHOT_AFTER_OPS, SnapshotSource, load_or_build_snapshot};
use crate::contract::table::{SyncTable, SyncTableWire};
use crate::protocol::{ClientMsg, SchemaVersion, ServerMsg, TableData};

pub struct Session<T: SyncTableWire, A: OpApply<T>, S: SnapshotSource<T>> {
    applier: A,
    source: S,
    /// The server's own schema version — declared by the app at
    /// construction; the Hello handshake compares major.minor (#34).
    version: SchemaVersion,
    // One snapshot per connection, max: a follow-up Pull replays events
    // instead, otherwise a stale snapshot and the backlog would ping-pong.
    snapshotted: bool,
    // `T` appears only in the trait bounds — the session speaks
    // `ServerMsg<T>` — so the type parameter is pinned with a marker.
    _table: PhantomData<T>,
}

impl<T: SyncTableWire, A: OpApply<T>, S: SnapshotSource<T>> Session<T, A, S> {
    pub fn new(applier: A, source: S, version: SchemaVersion) -> Self {
        Self {
            applier,
            source,
            version,
            snapshotted: false,
            _table: PhantomData,
        }
    }

    /// The whole client-message dispatch — parse, Push→Ack, Pull→
    /// snapshot-or-replay. `Ok(None)` means nothing to send; the
    /// transport loop logs `Err` and keeps the connection (one poisoned
    /// message must not kill the stream — at-least-once makes skipping
    /// harmless).
    pub async fn on_text(
        &mut self,
        db: &PgPool,
        text: &str,
    ) -> Result<Option<ServerMsg<T>>, SyncError> {
        match serde_json::from_str::<ClientMsg<T>>(text)? {
            ClientMsg::Hello { version } => {
                if self.version.wire_compatible(&version) {
                    Ok(Some(ServerMsg::Ready {
                        version: self.version.clone(),
                    }))
                } else {
                    Ok(Some(ServerMsg::Incompatible {
                        server: self.version.clone(),
                        client: version,
                    }))
                }
            }
            ClientMsg::Push { ops, batch } => {
                let cursor = push(db, &ops, &self.applier).await?;
                Ok(Some(ServerMsg::Ack { cursor, batch }))
            }
            ClientMsg::Pull { since } => {
                // Far behind? Replay would be one upsert per logged op —
                // hand over a snapshot instead (once per connection; a
                // follow-up Pull replays events, so snapshot and backlog
                // can't ping-pong). #35 adds the PRUNED-FLOOR trigger: a
                // client whose next needed seq (since + 1) was pruned away
                // never learned those state changes — replaying from any
                // surviving seq would diverge, so the snapshot is not
                // optional for it. (The once-per-connection guard stays:
                // after a snapshot the cursor is the snapshot seq, which
                // postdates the floor, so the case cannot recur.)
                let head = current_cursor(db).await;
                let floor = log_floor(db).await?;
                let pruned_away = floor.is_some_and(|f| since + 1 < f);
                if (head - since > SNAPSHOT_AFTER_OPS || pruned_away) && !self.snapshotted {
                    self.snapshotted = true;
                    Ok(Some(snapshot_msg(db, &self.source).await?))
                } else {
                    let (events, cursor) = pull_since(db, since).await?;
                    Ok(Some(ServerMsg::Events { events, cursor }))
                }
            }
        }
    }
}

/// Full state for a far-behind client, wire-shaped: the generic
/// [`load_or_build_snapshot`] pipeline mapped onto the protocol's own
/// DTOs — generic since #31, so this maps directly with no per-app glue.
async fn snapshot_msg<T: SyncTable, S: SnapshotSource<T>>(
    db: &PgPool,
    source: &S,
) -> Result<ServerMsg<T>, SyncError> {
    let (seq, tables, tombstones) = load_or_build_snapshot::<T, S>(db, source).await?;
    Ok(ServerMsg::Snapshot {
        seq,
        tables: tables
            .into_iter()
            .map(|rows| TableData {
                table: rows.table,
                rows: rows.rows,
            })
            .collect(),
        tombstones,
    })
}
