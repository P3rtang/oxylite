//! The migration upgrade contract (#48), pinned server-side against
//! real Postgres with the REAL sqlx migrator: an old library version
//! (lib 0001–0008) plus two consumer migrations are already applied
//! when the lib ships a new migration (0009). The upgrade must —
//! (1) apply exactly the missing lib migration and nothing else; (2)
//! leave every pre-existing history row untouched (checksums verbatim,
//! the sqlx boot validation's two checks — membership + per-version
//! checksum — hold); (3) be a NO-OP on a second run (idempotence is
//! the history table, not "one full pass"); (4) survive the history's
//! version-keyed shape even though the new row's `installed_on` is
//! newer than stamps with SMALLER versions (sqlx stores and validates
//! no application order — migrator.rs's run()).
//! `#[sqlx::test]` makes a fresh database per test; runtime queries
//! only (no test-target `.sqlx` entries — the #27 rule).

use oxylite::MIGRATIONS;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

/// The consumer's two migrations as `migrate up` stamps them — their
/// own clock, eleven orders of magnitude above the lib's integers (the
/// gap the merge rule leans on: the streams never cross-compare).
const APP: &[(&str, &str)] = &[
    (
        "20260918235015_app_t1",
        "-- app t1 — the consumer's own table",
    ),
    (
        "20260918235025_app_t2",
        "-- app t2 — the consumer's own table",
    ),
];

/// A Migrator over the merged list, mimicking `oxylite::server::migrator`'s
/// construction (the version is the name's full digit run). Building it
/// here lets the test pin a PAST library state (`old_lib` = MIGRATIONS
/// minus its last entry — the state before the lib "shipped 0009"),
/// which the real `migrator()` cannot: it always merges the CURRENT
/// MIGRATIONS.
fn migrator_over(list: &[(&'static str, &'static str)]) -> Migrator {
    let migrations = list
        .iter()
        .map(|(name, sql)| {
            let digits = name.chars().take_while(|c| c.is_ascii_digit()).count();
            let version: i64 = name[..digits]
                .parse()
                .expect("numeric version stem — the macro validates the shape");
            sqlx::migrate::Migration::new(
                version,
                Cow::Borrowed(*name),
                sqlx::migrate::MigrationType::Simple,
                Cow::Borrowed(*sql),
                false,
            )
        })
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

/// The "old" oxylite state: every lib migration except the LAST one —
/// exactly what a consumer pinned to the previous release had applied
/// (the last file IS the migration a newer version adds).
fn old_list_appended(app: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut old: Vec<(&'static str, &'static str)> = MIGRATIONS
        .iter()
        .take(MIGRATIONS.len() - 1)
        .copied()
        .collect();
    old.extend(app.iter().copied());
    old
}

#[sqlx::test]
async fn a_new_lib_migration_applies_into_an_already_migrated_db(
    pool: sqlx::PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    // The old world: lib 0001–0008 + the app's two stamps, applied by
    // the old version's own merged migrator.
    migrator_over(&old_list_appended(APP)).run(&pool).await?;
    let before = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await?;

    // The upgrade: the new version's merged list (the lib gained its
    // 0009 — MIGRATIONS here is the "new" crate's list).
    migrator_over(
        MIGRATIONS
            .iter()
            .copied()
            .chain(APP.iter().copied())
            .collect::<Vec<_>>()
            .as_slice(),
    )
    .run(&pool)
    .await?;

    let after = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await?;
    assert_eq!(after, before + 1, "exactly the new lib migration applies");

    // The new row is the lib's 0009 — its version derived from the name,
    // success recorded.
    let last = MIGRATIONS.last().unwrap().0;
    let digits = last.chars().take_while(|c| c.is_ascii_digit()).count();
    let new_version: i64 = last[..digits].parse()?;
    let applied_new = sqlx::query!(
        "SELECT success FROM _sqlx_migrations WHERE version = $1",
        new_version
    )
    .fetch_one(&pool)
    .await?;
    assert!(applied_new.success);

    // Version-keyed global history: read back ORDER BY version — the
    // fresh lib 0009 sorts before the consumer's stamps even though it
    // was APPLIED later (sqlx compares checksums by version, stores no
    // order — the ordering invariant is the merge rule's, not the
    // history table's).
    let versions: Vec<i64> = sqlx::query!("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|r| r.version)
        .collect();
    let stamps: Vec<i64> = APP
        .iter()
        .map(|(n, _)| {
            n[..n.chars().take_while(|c| c.is_ascii_digit()).count()]
                .parse()
                .unwrap()
        })
        .collect();
    let mut expected: Vec<i64> = MIGRATIONS
        .iter()
        .map(|(n, _)| {
            n[..n.chars().take_while(|c| c.is_ascii_digit()).count()]
                .parse()
                .unwrap()
        })
        .collect();
    expected.extend(stamps);
    assert_eq!(versions, expected);

    // Idempotence is the history table: a second run of the SAME new
    // migrator is a full no-op.
    migrator_over(
        MIGRATIONS
            .iter()
            .copied()
            .chain(APP.iter().copied())
            .collect::<Vec<_>>()
            .as_slice(),
    )
    .run(&pool)
    .await?;
    let after_second = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await?;
    assert_eq!(after_second, after, "second boot must apply nothing");
    Ok(())
}

#[sqlx::test]
async fn every_preexisting_row_survives_the_upgrade_intact(
    pool: sqlx::PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    migrator_over(&old_list_appended(APP)).run(&pool).await?;

    // Snapshot the old world's rows.
    let old_rows = sqlx::query!("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
        .fetch_all(&pool)
        .await?;

    // Upgrade.
    let full: Vec<(&'static str, &'static str)> = MIGRATIONS
        .iter()
        .copied()
        .chain(APP.iter().copied())
        .collect();
    migrator_over(full.as_slice()).run(&pool).await?;

    // Every old row: same version, same checksum — the upgrade did not
    // re-run or re-stamp anything (a checksum change would have panicked
    // the run; pinning the equality makes the WHY visible, not just the
    // absence of the panic).
    for old in old_rows {
        let now = sqlx::query!(
            "SELECT version, checksum FROM _sqlx_migrations WHERE version = $1",
            old.version
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(now.version, old.version);
        assert_eq!(now.checksum, old.checksum);
    }
    Ok(())
}
