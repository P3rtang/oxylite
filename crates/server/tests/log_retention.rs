//! Retention contracts (#35, roadmap 3.4): the age-based prune is the
//! pure OPERATION (no scheduler ships — reviewer decision: "only support
//! this when we have an event bus or a cron task system"), and the
//! clamp in pull_since is what makes ANY pruning gap-free. Runtime
//! queries — test targets don't own `.sqlx` entries.

use oxylite::server::{prune_sync_log, push};
use shared::Table;
use uuid::Uuid;

fn upsert_op(id: Uuid, title: &str, at: &str) -> oxylite::protocol::Op<Table> {
    oxylite::protocol::Op {
        table: Table::Notes,
        id,
        data: serde_json::json!({
            "id": id, "title": title, "body": "", "updated_at": at,
        }),
        updated_at: oxylite::protocol::timestamp::Timestamp::parse(at).unwrap(),
    }
}

async fn push_note(pool: &sqlx::PgPool, id: Uuid, title: &str, at: &str) -> i64 {
    push(pool, &[upsert_op(id, title, at)], &server::sync::Apply)
        .await
        .unwrap()
}

#[sqlx::test]
async fn prune_sync_log_removes_only_older_than_cutoff(pool: sqlx::PgPool) {
    server::sync::migrator().run(&pool).await.unwrap();

    // Three ops, one per push: seqs 1, 2, 3.
    for (i, title) in ["a", "b", "keep"].iter().enumerate() {
        let _ = push_note(
            &pool,
            Uuid::now_v7(),
            title,
            &format!("2026-01-01T00:00:0{i}.000Z"),
        )
        .await;
    }

    // Backdate two rows' server-arrival times (logged_at DEFAULTs to
    // now(); the ops' own updated_at is the client's clock and must
    // never drive pruning).
    sqlx::query("UPDATE oxylite.sync_log SET logged_at = now() - interval '3 days' WHERE seq <= 2")
        .execute(&pool)
        .await
        .unwrap();

    let removed = prune_sync_log(&pool, chrono::Utc::now() - chrono::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(removed, 2);

    let left: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM oxylite.sync_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 1);
    let survivor: String = sqlx::query_scalar("SELECT payload->>'title' FROM oxylite.sync_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(survivor, "keep");
}

#[sqlx::test]
async fn a_client_below_the_pruned_floor_gets_a_snapshot(pool: sqlx::PgPool) {
    server::sync::migrator().run(&pool).await.unwrap();
    // The correctness linchpin: a client whose next needed seq was
    // pruned away never learned those state changes — replaying from
    // any surviving seq would diverge from clients that saw them. The
    // server must hand it a SNAPSHOT (full state at head), not a
    // replay. A client at or above the floor keeps replaying: it saw
    // everything up to its cursor, so the survivors are enough.
    use oxylite::protocol::{ClientMsg, ServerMsg};
    use oxylite::server::{Session, log_floor};

    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let c = Uuid::now_v7();
    push_note(&pool, a, "a", "2026-01-01T00:00:01.000Z").await;
    push_note(&pool, b, "b", "2026-01-01T00:00:02.000Z").await;
    push_note(&pool, c, "c", "2026-01-01T00:00:03.000Z").await;

    sqlx::query("UPDATE oxylite.sync_log SET logged_at = now() - interval '3 days' WHERE seq <= 1")
        .execute(&pool)
        .await
        .unwrap();
    let removed = prune_sync_log(&pool, chrono::Utc::now() - chrono::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(log_floor(&pool).await.unwrap(), Some(2));

    let mut session = Session::new(
        server::sync::Apply,
        server::sync::SyncRowSnapshots,
        oxylite::protocol::SchemaVersion::parse(shared::SCHEMA_VERSION)
            .expect("The version of the project does not match the major.minor.patch standard. This is required for schema alignment between server and client."),
    );

    // A stone-age client (cursor 0, below the floor): SNAPSHOT, full state.
    let pull = serde_json::to_string(&ClientMsg::<Table>::Pull { since: 0 }).unwrap();
    let reply = session.on_text(&pool, &pull).await.unwrap().unwrap();
    assert!(matches!(reply, ServerMsg::Snapshot { .. }), "got {reply:?}");

    // A client exactly at the floor's edge (cursor 1 = saw the pruned row
    // pre-prune): replay of the survivors — NOT a snapshot, not a gap.
    let mut session = Session::new(
        server::sync::Apply,
        server::sync::SyncRowSnapshots,
        oxylite::protocol::SchemaVersion::parse(shared::SCHEMA_VERSION).unwrap(),
    );
    let pull = serde_json::to_string(&ClientMsg::<Table>::Pull { since: 1 }).unwrap();
    let reply = session.on_text(&pool, &pull).await.unwrap().unwrap();
    let ServerMsg::Events { events, cursor } = reply else {
        panic!("expected replay, got {reply:?}");
    };
    let titles: Vec<String> = events
        .iter()
        .map(|op| {
            op.data
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(titles, vec!["b".to_string(), "c".to_string()]);
    assert_eq!(cursor, 3);
}
