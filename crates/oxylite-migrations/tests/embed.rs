//! Pin the macro's embedding contract: the dir is scanned at expansion,
//! `NNNN_name.sql` only (`.down.sql` ignored), lexicographic order,
//! absolute-path include_str! so the content is the file's. The lib's
//! merge tests (crates/oxylite/tests/migration_merge.rs) pin what
//! happens to this list afterwards.

use oxylite_migrations::migrations;

const MIGRATIONS: &[(&str, &str)] = migrations!("tests/fixtures/migrations");

// Trailing-comma tolerance — call sites grow arguments.
const WITH_COMMA: &[(&str, &str)] = migrations!("tests/fixtures/migrations",);

#[test]
fn embeds_sorted_without_down_files() {
    let names: Vec<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["0001_one", "0002_two"]);

    // Content is the file's (include_str! at expansion time).
    assert!(MIGRATIONS[0].1.contains("CREATE TABLE one"));
    assert!(MIGRATIONS[1].1.contains("CREATE TABLE two"));
}

#[test]
fn trailing_comma_is_tolerated() {
    assert_eq!(MIGRATIONS, WITH_COMMA);
}

#[test]
fn a_down_only_dir_embeds_as_empty() {
    const EMPTY: &[(&str, &str)] = migrations!("tests/fixtures/downonly");
    assert_eq!(EMPTY, &[]);
}
