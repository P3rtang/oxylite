//! The counter example's sync server — the demo's crates/server in
//! miniature: the WS transport and the generic sync machinery are the
//! LIB's; this binary is routes + the one-table hookup (the Apply arm)
//! + serving the built page and the bundle on ONE origin.
//!
//! Port 3002 (env PORT); database `counter_demo` — a consumer's own
//! database, created on boot if missing (the demo assumed compose init;
//! a template needs the ensure step — gap, 2026-09-15).

mod sync;

use axum::Router;
use oxylite::protocol::SchemaVersion;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://sync:sync@localhost:5432/counter_demo".into());

    ensure_database(&db_url).await.expect("ensure database");

    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("connect to postgres");

    // One migrator over the UNION — the lib merges the app's list
    // (`APP_MIGRATIONS`, the macro-embedded migrations/) with its own
    // protocol tables.
    oxylite::server::migrator(counter_shared::APP_MIGRATIONS)
        .run(&db)
        .await
        .expect("apply migrations");

    // The stream driver's fallback cadence — env first, demo-ruled.
    let fallback_tick = std::env::var("SYNC_FALLBACK_TICK_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(oxylite::ws::DEFAULT_FALLBACK_TICK);

    let app = Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        // The vendored PGlite bundle — the CLIENT's copy (served with
        // proper MIME types so the boot snippet can import it).
        .nest_service(
            "/pglite",
            ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/pglite"))
                .append_index_html_on_directories(false),
        )
        // The WS sync endpoint is a lib drop-in; the app plugs in its
        // table impls and its schema version.
        .merge(oxylite::ws::sync_router(
            db.clone(),
            sync::Apply,
            oxylite::server::SyncRowSnapshots,
            SchemaVersion::parse(counter_shared::SCHEMA_VERSION)
                .expect("SCHEMA_VERSION must be MAJOR.MINOR.PATCH"),
            fallback_tick,
        ))
        .layer(CorsLayer::permissive())
        // The built dioxus client — everything on one origin, so the
        // browser ESM import and the WS connection are trivial.
        .fallback_service(ServeDir::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/dx/counter/debug/web/public"
        )));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3002);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind counter server port");

    println!("counter server on http://localhost:{port} (ws sync at /sync, pglite at /pglite)");

    axum::serve(listener, app).await.unwrap();
}

/// Create the consumer's database if it does not exist — the
/// bootstrap the demo never needed (its compose init created
/// `offline_notes`). `CREATE DATABASE` cannot run inside a transaction,
/// so the check goes over the maintenance database first.
async fn ensure_database(db_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (base, _) = db_url.rsplit_once('/').ok_or("unparsable DATABASE_URL")?;
    let maint = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{base}/postgres"))
        .await?;
    // The consumer's database is the URL's last segment (not a const —
    // a deploy re-points the URL, the name follows).
    let name = db_url
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .filter(|t| !t.is_empty())
        .ok_or("unparsable DATABASE_URL")?;
    let exists: bool =
        sqlx::query_scalar("select exists (select 1 from pg_database where datname = $1)")
            .bind(name)
            .fetch_one(&maint)
            .await?;
    if !exists {
        println!("creating database {name}…");
        // Identifiers cannot bind — the name is this binary's const.
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&maint)
            .await?;
    }
    maint.close().await;
    Ok(())
}
