use axum::{Router, routing::get};
// The binary is thin on purpose: routes + serving; the sync machinery
// (session decisions + WS transport) lives in the lib targets
// (crates/oxylite, features `server` + `axum`), the app's table plug-ins
// in crates/server/src/sync.rs.
use server::sync;

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://sync:sync@localhost:5432/offline_notes".into());

    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("connect to postgres");

    // Migrations split along the lib/app boundary (#31) — the lib owns
    // its protocol tables, the app its own; they apply as ONE migrator
    // over the merged list (see sync::migrator for why).
    sync::migrator().run(&db).await.expect("apply migrations");

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        // The vendored PGlite bundle (ES module + wasm + data), served with
        // proper MIME types so the client can dynamically `import()` it.
        // Lives in the sync crate — it owns the whole PGlite chain.
        .nest_service(
            "/pglite",
            tower_http::services::ServeDir::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../oxylite/assets/pglite"
            ))
            .append_index_html_on_directories(false),
        )
        // The built dioxus client (crates/client/dist) — everything on one
        // origin, so the browser ESM import and the WS connection are trivial.
        .fallback_service(tower_http::services::ServeDir::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../client/dist"
        )))
        // The WS sync endpoint is a lib drop-in (sync crate, `axum`
        // feature); the app plugs in its table impls.
        .merge(sync::sync_router(
            db.clone(),
            sync::Apply,
            sync::SyncRowSnapshots,
        ))
        .layer(tower_http::cors::CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    println!("server listening on http://0.0.0.0:3000 (ws sync at /sync)");

    axum::serve(listener, app).await.unwrap();
}
