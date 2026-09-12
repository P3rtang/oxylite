//! The schema-version handshake (#34): Hello carries the client's
//! schema version; the server compares MAJOR.MINOR only (patch always
//! compatible) and answers Ready or Incompatible. Runtime queries —
//! test targets don't own `.sqlx` entries.

use oxylite::protocol::{ClientMsg, SchemaVersion, ServerMsg};
use oxylite::server::Session;

/// A minimal Session fixture: the plug-ins are never called by the
/// handshake (no Push/Pull reaches the DB in these tests), so ZSTs and
/// a pool that's never touched suffice.
fn session() -> Session<shared::Table, server::sync::Apply, server::sync::SyncRowSnapshots> {
    Session::new(
        server::sync::Apply,
        server::sync::SyncRowSnapshots,
        SchemaVersion::parse("2.3.9").unwrap(),
    )
}

fn hello_text(version: &str) -> String {
    serde_json::to_string(&ClientMsg::<shared::Table>::Hello {
        version: SchemaVersion::parse(version).unwrap(),
    })
    .unwrap()
}

#[sqlx::test]
async fn same_major_minor_is_ready_even_across_patches(pool: sqlx::PgPool) {
    let mut session = session();
    // The server's own patch (2.3.9) differs; patches never gate. The
    // pool rides the signature only — the handshake touches no tables.
    let reply = session
        .on_text(&pool, &hello_text("2.3.2"))
        .await
        .unwrap()
        .unwrap();
    let ServerMsg::Ready { version } = reply else {
        panic!("expected Ready, got {reply:?}");
    };
    assert_eq!(version.as_text(), "2.3.9");
}

#[sqlx::test]
async fn minor_mismatch_is_incompatible(pool: sqlx::PgPool) {
    let mut session = session();
    let reply = session
        .on_text(&pool, &hello_text("2.4.0"))
        .await
        .unwrap()
        .unwrap();
    let ServerMsg::Incompatible { server, client } = reply else {
        panic!("expected Incompatible, got {reply:?}");
    };
    assert_eq!(server.as_text(), "2.3.9");
    assert_eq!(client.as_text(), "2.4.0");
}

#[sqlx::test]
async fn major_mismatch_is_incompatible(pool: sqlx::PgPool) {
    let mut session = session();
    let reply = session
        .on_text(&pool, &hello_text("3.0.0"))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(reply, ServerMsg::Incompatible { .. }));
}
