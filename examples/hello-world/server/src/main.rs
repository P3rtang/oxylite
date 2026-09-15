//! The example's axum server — the demo's serving contract in miniature
//! (crates/server/src/main.rs): ONE origin serves the built page and the
//! vendored PGlite bundle, so the boot snippet's dynamic
//! `import("/pglite/index.js")` resolves and there is no CORS story. A
//! bare dx devserver does not serve raw asset files — this server is what
//! makes the example boot; `scripts/oxylite.sh serve`/`watch` build the
//! client and own this server's lifecycle (the demo's dev.sh shape).

use axum::{Router, routing::get};

#[tokio::main]
async fn main() {
    // The built dioxus client — oxylite.sh runs `dx build` BEFORE starting
    // this server, so the output dir always postdates the build (the
    // demo's dist-freshness rule, owned by the orchestrating script here
    // instead of a dist/ sync). DIST re-points it (release lives one mode
    // dir over).
    let dist = std::env::var("DIST").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/dx/hello-world/debug/web/public"
        )
        .into()
    });

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        // The vendored bundle (assets/pglite) with proper MIME types —
        // ServeDir sniffs extensions; a directory hit is a mistake, not a
        // page (same knob as the demo). No CORS layer: page and bundle
        // are same-origin by construction.
        .nest_service(
            "/pglite",
            tower_http::services::ServeDir::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../assets/pglite"
            ))
            .append_index_html_on_directories(false),
        )
        // Everything else: the built page. Port 3000 is the demo sync
        // server's; the example gets its own so both run side by side
        // (env-over-const, same deployment rule as the demo's knobs).
        .fallback_service(tower_http::services::ServeDir::new(dist));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind example server port");

    println!("hello-world server on http://localhost:{port} (pglite at /pglite, health at /health)");

    axum::serve(listener, app).await.unwrap();
}
