//! examples/hello-world — the smallest oxylite consumer: boot the local
//! PGlite database (IndexedDB) and prove it on the page. No sync server,
//! no engine, no app tables — just `Pglite::init` applying the lib's own
//! migrations. Later steps (engine, live queries, a server) extend this
//! page progressively.
//!
//! Run: vendor the bundle once (`scripts/oxylite.sh init` from this
//! directory — the consumer bootstrap — or `scripts/vendor-pglite.sh
//! 0.5.8 examples/hello-world/assets/pglite`). Serving is this example's
//! own axum server (server/ — the demo's crates/server in miniature: one
//! origin for the built page and the bundle at /pglite/). `scripts/
//! oxylite.sh serve` builds the client + starts the server; `scripts/
//! oxylite.sh watch` adds dx's devserver with the Dioxus.toml proxy for
//! hot reload (the demo's dev.sh shape). The boot snippet dynamic-imports
//! /pglite/index.js from the page origin — a bare `dx serve` does NOT
//! provide that mount (dx only bundles manganis-processed assets).

use dioxus::prelude::*;
use oxylite::BridgeError;
use oxylite::client::pglite::{Pglite, rows_of};

fn main() {
    console_error_panic_hook::set_once();
    dioxus::launch(App);
}

/// What the page shows once boot settles: the server identity line and
/// the migrations the apply-once boot applied (the lib's protocol tables
/// in schema `oxylite`). Clone so the component can copy the signal's
/// value out before rendering.
#[derive(Clone)]
struct Boot {
    version: String,
    migrations: Vec<String>,
}

#[component]
fn App() -> Element {
    // None = boot in flight; Some(Err(e)) = init or a proof query failed.
    let mut boot = use_signal(|| None::<Result<Boot, BridgeError>>);

    // One boot task. `init` is a lib singleton — re-entry re-joins the
    // live instance, so this effect racing itself is harmless.
    use_effect(move || {
        spawn(async move {
            boot.set(Some(boot_pglite().await));
        });
    });

    // Clone out of the signal before the rsx (the repo's rsx idiom —
    // if/else chains, no match nodes).
    let state = boot.read().clone();

    rsx! {
        div {
            style: "max-width: 640px; margin: 2rem auto; font-family: system-ui, sans-serif",
            h1 { "Hello, oxylite" }
            if state.is_none() {
                p { "booting PGlite…" }
            }
            if let Some(Err(e)) = &state {
                p { style: "color:#b00", "boot failed: {e}" }
            }
            if let Some(Ok(b)) = &state {
                p { style: "color:#060", "PGlite booted ✓" }
                p { style: "color:#666", "{b.version}" }
                p { "migrations applied:" }
                ul { for m in &b.migrations { li { "{m}" } } }
            }
        }
    }
}

async fn boot_pglite() -> Result<Boot, BridgeError> {
    // THE MIGRATION STANDARD (≥0.1.1): the app drops NNNN_name.sql files
    // into migrations/ and calls `Pglite::init(oxylite::migrations!(
    // "migrations"))` — the macro embeds the app's list at compile time,
    // the lib's protocol migrations merge in at boot automatically; the
    // consumer never names them. This example pins the PUBLISHED 0.1.0
    // exactly as a second app would (see Cargo.toml), and 0.1.0's init
    // took the FULL list — so the pre-standard call below. One-line flip
    // (plus migrations/ + a rerun-if-changed build.rs) once 0.1.1 ships.
    let db = Pglite::init(oxylite::MIGRATIONS).await?;

    let version = db.query("select version()", &[]).await?;
    let version = rows_of(&version)[0]
        .field_req::<String>("version")
        // Row-read failures are display-shaped at the page boundary the
        // same way JS rejections arrive (a message rides the error).
        .map_err(|e| BridgeError::Js {
            message: e.to_string(),
        })?;

    let migs = db
        .query(
            "select key from oxylite.meta where key like 'migration:%' order by key",
            &[],
        )
        .await?;
    let migrations = rows_of(&migs)
        .iter()
        .map(|r| {
            r.field_req::<String>("key")
                .map(|k| k.strip_prefix("migration:").unwrap_or(&k).to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| BridgeError::Js {
            message: e.to_string(),
        })?;

    Ok(Boot {
        version,
        migrations,
    })
}
