//! The migration standard's merge contract (host tests — the merge is
//! pure), as the #48 ruling has it: the LIB's stream runs first (its
//! own numeric-version order), then every consumer migration in their
//! order — whatever theaNAMES say — and a numeric version collision
//! (within either stream or across them) is a named panic. The client's
//! `Pglite::init` and the server's `server::migrator` both funnel
//! through `migrations_merged` — pinning it here pins both engines'
//! applied shape.

use oxylite::migrations_merged;

// The app's list as the macro would embed it — CLI-stamped timestamps
// (the consumer owns their clock; the lib's integers live eleven
// orders of magnitude below, the gap the merge rule leans on).
const APP: &[(&str, &str)] = &[
    ("20260919100005_app_notes", "create table app_notes ();"),
    ("20260919101015_app_extra", "create table app_extra ();"),
    ("20260919102045_app_last", "create table app_last ();"),
];

#[test]
fn lib_first_then_the_app_stream_in_their_own_order() {
    let merged = migrations_merged(APP);
    let names: Vec<&str> = merged.iter().map(|(n, _)| *n).collect();

    // The lib's block is the HEAD, in its own version order; the app's
    // stamps follow, never interleaved (a 14-digit stamp cannot equal a
    // four-digit lib version — the streams' clocks do not cross-compare).
    assert_eq!(names.first(), Some(&"0001_sync_log"));
    assert!(names.contains(&"0009_sync_log_notify"));
    assert_eq!(
        *names.get(oxylite::MIGRATIONS.len()).unwrap(),
        "20260919100005_app_notes"
    );
    assert_eq!(names.last(), Some(&"20260919102045_app_last"));
}

#[test]
fn merged_len_is_lib_plus_app() {
    assert_eq!(
        migrations_merged(APP).len(),
        oxylite::MIGRATIONS.len() + APP.len()
    );
    // The compat-count helper agrees (the IDB epoch is this number).
    assert_eq!(
        oxylite::migrations_total(APP),
        oxylite::MIGRATIONS.len() + APP.len()
    );
}

#[test]
fn empty_app_list_is_the_lib_only() {
    let merged = migrations_merged(&[]);
    assert_eq!(merged.len(), oxylite::MIGRATIONS.len());
    assert_eq!(merged[0].0, "0001_sync_log");
}

#[test]
#[should_panic(expected = "version collision")]
fn a_version_collision_panics_with_both_sides_named() {
    // The app file claimed a version the lib already owns — the legacy
    // NNNN hole: hand-named files colliding with the re-indexed lib are
    // caught at the merge point (compile or boot), both files named,
    // instead of a mid-boot PK violation.
    let colliding: &[(&str, &str)] = &[("0002_sneak", "create table sneak ();")];
    let _ = migrations_merged(colliding);
}

#[test]
#[should_panic(expected = "version collision")]
fn a_dup_inside_the_app_stream_panics_too() {
    // The rule is one numeric version space: the app repeating its own
    // version is the same programming error as reusing a lib's.
    let duped: &[(&str, &str)] = &[
        ("20260919100005_one", "create table one ();"),
        ("20260919100005_two", "create table two ();"),
    ];
    let _ = migrations_merged(duped);
}
