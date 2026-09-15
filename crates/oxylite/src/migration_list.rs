//! The migration standard's runtime half (the compile-time half is
//! `oxylite::migrations!` — oxylite-migrations). The lib owns its
//! protocol tables' migrations (crates/oxylite/migrations, generated
//! into [`MIGRATIONS`] by build.rs); the consumer owns its own
//! `NNNN_name.sql` set, embedded by the macro. The two lists ride as ONE
//! on both engines — the client's apply-once PGlite boot and the
//! server's sqlx migrator share an identity space, so the merge happens
//! exactly once, here, with the collision named loudly.

use crate::MIGRATIONS;

/// The union of the lib's protocol migrations and the consumer's `app`
/// list, sorted lexicographically (= applied order on BOTH engines —
/// the same contract sqlx::migrate! and the client's apply-once tracker
/// follow).
///
/// Panics on a version collision: the NNNN stems must be globally unique
/// across the lib's protocol migrations and the app's list (one
/// `_sqlx_migrations` table on the server, one apply-once tracker in the
/// client). A collision is a programming error — it is named with both
/// sides, not skipped.
pub fn migrations_merged(
    app: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let mut all: Vec<(&'static str, &'static str)> =
        Vec::with_capacity(MIGRATIONS.len() + app.len());
    all.extend(MIGRATIONS.iter().copied());
    all.extend(app.iter().copied());
    all.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for w in all.windows(2) {
        if w[0].0 == w[1].0 {
            panic!(
                "migration version collision: {} exists in both the lib's \
                 protocol migrations (crates/oxylite/migrations) and the \
                 app's list — NNNN stems must be globally unique; pick a \
                 free number for the app's file",
                w[0].0
            );
        }
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
