//! examples/hello-world — the smallest oxylite consumer: boot the local
//! PGlite database (IndexedDB) and prove it on the page. No sync server,
//! no engine, no app tables of consequence — `Pglite::init` boots the
//! MERGED migration list (the lib's protocol tables + this app's
//! `migrations/`, the standard since 0.1.1). Later steps (engine, live
//! queries, sync) extend this page progressively.
//!
//! Run: `scripts/oxylite.sh serve` from this directory (builds the
//! client + starts server/, one origin for the page and the bundle at
//! /pglite/) or `scripts/oxylite.sh watch` for hot reload via the dx
//! devserver + Dioxus.toml proxy (the demo's dev.sh shape).

use dioxus::prelude::*;
use oxylite::client::pglite::{BridgeError, Pglite};

fn main() {
    console_error_panic_hook::set_once();
    dioxus::launch(App);
}

/// What the page shows once boot settles: the server identity line and
/// the migrations the apply-once boot applied — the lib's typed defaults
/// (`Pglite::version` / `Pglite::applied_migrations`) answer both; the
/// raw query door stays open for anything they don't cover. Clone so the
/// component can copy the signal's value out before rendering.
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
    // into migrations/ and passes `oxylite::migrations!("migrations")` —
    // the macro embeds the app's list at compile time; the lib's
    // protocol migrations merge in at boot automatically (sorted, one
    // applied list on both engines). The consumer never names them, and
    // this file (migrations/0001_hello.sql) was the whole job.
    //
    // The proof line uses the lib's typed defaults — no hand-written SQL
    // against the lib's own tables.
    let db = Pglite::init(oxylite::migrations!("migrations")).await?;

    Ok(Boot {
        version: db.version().await?,
        migrations: db.applied_migrations().await?,
    })
}
