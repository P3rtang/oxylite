//! Build script: generate the DEMO app's migration list by scanning the
//! lib's protocol-table migrations (`../sync/migrations`) and this
//! crate's app-table migrations (`migrations/`) at compile time, merged
//! by version — a new migration file needs no hand-edit of `lib.rs` (a
//! forgotten entry would make the server apply a migration the client
//! never runs — a silent drift bug).
//!
//! The lib owns its tables' migrations and the app owns its own (#31);
//! this merged list is the demo's one-list convenience that drives BOTH
//! sides: the client's apply-once PGlite boot (tracked in the client's
//! `meta` table), and the server's `sqlx::migrate!` over the app's
//! directory (the lib runs its own via `sync::server::migrate`).

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dirs = [
        Path::new(&manifest).join("migrations"),
        // The lib's own protocol-table migrations — part of the union
        // because the demo client boots PGlite with ONE list.
        Path::new(&manifest).join("../sync/migrations"),
    ];
    for dir in &dirs {
        // Recursive: a file added, removed, renamed or edited inside the
        // directory reruns the script (an edited migration must re-embed).
        println!("cargo:rerun-if-changed={}", dir.display());
    }

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for dir in &dirs {
        files.extend(
            fs::read_dir(dir)
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
                }),
        );
    }

    // The convention IS the contract with the server's `sqlx::migrate!`
    // and the client's apply-once tracker: `NNNN_name.sql`, zero-padded,
    // so lexicographic order == applied order on BOTH sides, and the
    // versions are globally unique across BOTH directories (the lib's
    // migrator and the app's share one `_sqlx_migrations` table). Five
    // digits are rejected on purpose — they would sort before four
    // ("10000" < "9999"), so the width is enforced.
    files.sort();
    for (stem, _) in &files {
        let digits = stem.chars().take_while(|c| c.is_ascii_digit()).count();
        let rest = &stem[digits..];
        if digits != 4 || !rest.starts_with('_') || rest.len() < 2 {
            panic!(
                "migrations: {} does not follow the NNNN_name.sql convention \
                 (four zero-padded digits + underscore) — sqlx::migrate! and the \
                 client's apply-once tracker must agree on order and identity",
                stem
            );
        }
    }
    let mut sorted = files.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    if sorted.len() != files.len() {
        panic!(
            "migrations: duplicate version across the lib and app directories — \
                versions must be globally unique (one _sqlx_migrations table)"
        );
    }

    let mut code = String::from(
        "/// The demo app's full migration list: the UNION of the lib's\n\
         /// protocol-table migrations and the app's own (see build.rs).\n\
         /// GENERATED — scan of both directories, sorted lexicographically\n\
         /// (= applied order); adding a migration file is the whole job.\n",
    );
    code.push_str("pub static MIGRATIONS: &[(&str, &str)] = &[\n");
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
