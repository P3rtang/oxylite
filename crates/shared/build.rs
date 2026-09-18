//! Build script: generate the DEMO app's migration list by scanning this
//! crate's app-table migrations (`migrations/`) at compile time — a new
//! migration file needs no hand-edit of `lib.rs` (a forgotten entry would
//! make the server apply a migration the client never runs — a silent
//! drift bug). The lib's protocol-table migrations are NOT part of this
//! list: the standard (reviewer decision, migrations standard) has the
//! lib merge its own in at the boot/migrator entry points, so the
//! consumer's list is the app's only. A one-line build.rs keeps the
//! rerun-if-changed contract the proc-macro cannot express (cargo tracks
//! dirs for build scripts only).
//!
//! This generated list drives BOTH demo sides: the client's apply-once
//! PGlite boot (tracked in the client's `meta` table), and the server's
//! migrator (`sync::migrator` — the lib's `server::migrator` merges the
//! lib's own protocol migrations back in).

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dir = Path::new(&manifest).join("migrations");
    // Recursive: a file added, removed, renamed or edited inside the
    // directory reruns the script (an edited migration must re-embed).
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut files: Vec<(String, PathBuf)> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("migrations: cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "sql")
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".down.sql"))
        })
        .map(|p| {
            let stem = p
                .file_stem()
                .unwrap_or_else(|| panic!("migrations: {}", p.display()))
                .to_string_lossy()
                .into_owned();
            (stem, p)
        })
        .collect();

    // The convention IS the identity contract (#48): applied order =
    // NUMERIC version order; the timestamps the CLI scaffolds own the
    // app's ordering (its own clock), and the versions are GLOBAL
    // across the lib's protocol migrations (a collision is a named
    // panic at the lib's merge point — migration_list.rs).
    files.sort();
    for (stem, _) in &files {
        let digits = stem.chars().take_while(|c| c.is_ascii_digit()).count();
        let rest = &stem[digits..];
        if digits == 0 || !rest.starts_with('_') || rest.len() < 2 {
            panic!(
                "migrations: {} does not follow the <version>_<name>.sql \
                 convention (integer version + underscore + name) — \
                 sqlx::migrate! and the client's apply-once tracker must \
                 agree on order and identity",
                stem
            );
        }
    }
    let mut sorted = files.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    if sorted.len() != files.len() {
        panic!("migrations: duplicate version in the app's migrations directory");
    }

    let mut code = String::from(
        "/// The DEMO app's own migration list (its tables only — the\n\
         /// lib's protocol migrations merge in at the boot/migrator entry\n\
         /// points). GENERATED — scan of `migrations/`, sorted\n\
         /// lexicographically (= applied order); adding a migration file\n\
         /// is the whole job.\n",
    );
    code.push_str("pub static APP_MIGRATIONS: &[(&str, &str)] = &[\n");
    for (stem, path) in &files {
        // include_str! with the absolute path: the generated file lives in
        // OUT_DIR, so relative resolution would miss.
        code.push_str(&format!(
            "    (\"{stem}\", include_str!(\"{}\")),\n",
            path.display()
        ));
    }
    code.push_str("];\n");

    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("migrations.rs");
    fs::write(&out, code).unwrap_or_else(|e| panic!("migrations: write {out:?}: {e}"));
}
