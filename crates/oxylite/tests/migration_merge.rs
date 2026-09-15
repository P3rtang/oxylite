//! The migration standard's merge contract (host tests — the merge is
//! pure): lib protocol migrations + the app's list ride as ONE list,
//! lexicographic order == applied order, and a version collision is a
//! named panic. The client's `Pglite::init` and the server's
//! `server::migrator` both funnel through `migrations_merged` — pinning
//! it here pins both engines' applied shape.

use oxylite::migrations_merged;

// The app's list as the macro would embed it: the app's own tables only,
// NNNN-ordered.
const APP: &[(&str, &str)] = &[
    ("0001_app_notes", "create table app_notes ();"),
    ("0008_app_extra", "create table app_extra ();"),
    ("0012_app_last", "create table app_last ();"),
];

#[test]
fn merged_is_the_global_lexicographic_union() {
    let merged = migrations_merged(APP);
    let names: Vec<&str> = merged.iter().map(|(n, _)| *n).collect();

    // The app's entries interleave with the lib's by version, not by
    // origin — the demo's layout (0001 app, 0002-0011 lib, 0008 app)
    // is exactly this shape.
    assert_eq!(names.first(), Some(&"0001_app_notes"));
    assert!(names.contains(&"0002_sync_log"));
    assert!(names.contains(&"0011_sync_log_notify"));
    assert!(names.contains(&"0008_app_extra"));
    assert_eq!(names.last(), Some(&"0012_app_last"));

    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "merged must already be applied-ordered");
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
    assert_eq!(merged[0].0, "0002_sync_log");
}

#[test]
#[should_panic(expected = "version collision")]
fn a_version_collision_panics_with_both_sides_named() {
    // The app picked a number the lib already owns — the exact mistake
    // the demo's build.rs used to catch at compile time when the union
    // was scanned in one place; the merge point is the one place both
    // lists meet, so it is the one place this can be named.
    let colliding: &[(&str, &str)] = &[("0002_sync_log", "create table sneak ();")];
    let _ = migrations_merged(colliding);
}
