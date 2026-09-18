//! The migration standard's runtime half (the compile-time half is
//! `oxylite::migrations!` — oxylite-migrations). The lib owns its
//! protocol tables' migrations (crates/oxylite/migrations, generated
//! into [`MIGRATIONS`] by build.rs); the consumer owns its own set,
//! embedded by the macro (timestamp-named by the CLI scaffold, legacy
//! NNNN accepted). The lists merge as the #48 ruling has it: the LIB's
//! stream first (its own sorted order), then every consumer migration
//! in their order — the two never cross-compare, so a consumer's file
//! name carries no constraint beyond being a version. One numeric
//! version space remains load-bearing: the history tables (the server's
//! `_sqlx_migrations`, the client's apply-once tracker) are keyed BY
//! VERSION, and on both engines this is the type that must be global.

use crate::MIGRATIONS;

/// The version prefix of a migration name — every leading digit run
/// (`0002_sync_log` → 2, `20260919152000_hello` → its stamp). Names
/// without a leading numeric run are rejected by the macro at compile
/// time; a runtime panic here is the belt for the suspender.
fn version_of(stem: &str) -> i64 {
    let digits = stem.chars().take_while(|c| c.is_ascii_digit()).count();
    stem[..digits].parse().unwrap_or_else(|_| {
        panic!(
            "migration {stem:?} has no numeric version prefix — the macro \
             should have rejected it at compile time"
        )
    })
}

/// The union of the lib's protocol migrations and the consumer's `app`
/// list, THE RULE the reviewer set (#48): LIB MIGRATIONS FIRST — the
/// lib's own sequence, sorted by version — followed by every consumer
/// migration in their order. The streams never interleave, so the name
/// a consumer gives (timestamp stamp, legacy NNNN, whatever) carries no
/// ordering contract against the lib's — the only shared space is the
/// numeric version itself (the two history tables' keys), which the
/// collision check owns.
///
/// Panics on a version collision (within either stream or across
/// them): a version is a global identity on both engines — one
/// `_sqlx_migrations` (version = PRIMARY KEY) on the server, one
/// apply-once tracker in the client. The panic names both files.
pub fn migrations_merged(
    app: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let mut lib: Vec<(&'static str, &'static str)> = MIGRATIONS.to_vec();
    lib.sort_unstable_by_key(|m| version_of(m.0));
    let mut app: Vec<(&'static str, &'static str)> = app.to_vec();
    app.sort_unstable_by_key(|m| version_of(m.0));

    let mut seen: Vec<(i64, &'static str)> = Vec::new();
    let mut all = Vec::with_capacity(lib.len() + app.len());
    for m in lib.drain(..).chain(app.drain(..)) {
        let v = version_of(m.0);
        if let Some((_, name)) = seen.iter().find(|(sv, _)| *sv == v) {
            panic!(
                "migration version collision: {name:?} is already version {v}, \
                 now also claiming {new_name:?} — versions are GLOBAL across \
                 the lib's protocol migrations and the app's list (one \
                 _sqlx_migrations table, one apply-once tracker); the CLI \
                 scaffolds timestamps, which cannot collide",
                name = name,
                new_name = m.0,
                v = v,
            );
        }
        seen.push((v, m.0));
        all.push(m);
    }
    all
}

/// How many migrations a fresh database sees for this app list — the
/// lib's protocol migrations ride along automatically, so the client's
/// IndexedDB compat version (the applied count, engine/sync.rs) must
/// count the UNION, never just the app's list. Pure arithmetic: the
/// collision check lives in [`migrations_merged`] (and panics before
/// any count is wrong anyway — a dup would fail the boot).
pub const fn migrations_total(app: &[(&'static str, &'static str)]) -> usize {
    MIGRATIONS.len() + app.len()
}
