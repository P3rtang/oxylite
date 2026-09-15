//! `migrations!("migrations")` — embed the consumer's migration directory
//! at compile time. The compile-time half of the migration standard (the
//! runtime half — merging the lib's protocol migrations with this list —
//! lives in `oxylite::{migrations_merged, migrations_total}` and runs
//! inside `Pglite::init` / `oxylite::server::migrator`).
//!
//! Contract (the same one the demo's build.rs pinned before the macro
//! existed): plain `*.sql` files named `NNNN_name.sql` — four zero-padded
//! digits, underscore, then a name; `.down.sql` files are ignored (up
//! only); lexicographic stem order IS the applied order on both engines
//! (sqlx's migrator and the client's apply-once tracker); the NNNN
//! versions must be globally unique against the lib's protocol
//! migrations (checked at the merge point with a named panic).
//!
//! Path resolution: the literal is relative to the CALLING crate's
//! manifest (`CARGO_MANIFEST_DIR` at expansion time). The
//! `OXYLITE_MIGRATIONS_DIR` env var overrides it — read at expansion, so
//! unlike the literal it is NOT re-expanded when only the env changes
//! (cargo tracks build-script env, not proc-macro env): the literal path
//! is the primary contract, the env var a deployment convenience.

use proc_macro::{Spacing, TokenStream, TokenTree};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Embed the calling crate's migration directory as
/// `&'static [(&'static str, &'static str)]` — `(NNNN_name, sql)` pairs,
/// sorted for application. The lib merges its own protocol migrations in
/// at boot; the consumer never names them.
#[proc_macro]
pub fn migrations(input: TokenStream) -> TokenStream {
    match expand(input) {
        Ok(tokens) => tokens,
        Err(message) => compile_error(&message),
    }
}

fn expand(input: TokenStream) -> Result<TokenStream, String> {
    let literal = dir_literal(input)?;

    let manifest = env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        "migrations!: CARGO_MANIFEST_DIR unset — not running under cargo?".to_string()
    })?;
    let raw = env::var("OXYLITE_MIGRATIONS_DIR").ok().unwrap_or(literal);
    let dir = PathBuf::from(raw);
    let dir = if dir.is_absolute() {
        dir
    } else {
        Path::new(&manifest).join(dir)
    };

    let entries = fs::read_dir(&dir).map_err(|e| {
        format!(
            "migrations!: cannot read the migrations directory {} \
             — create it (NNNN_name.sql files) or point OXYLITE_MIGRATIONS_DIR \
             at it: {e}",
            dir.display()
        )
    })?;

    let mut files: Vec<(String, PathBuf)> = entries
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
                .ok_or_else(|| format!("migrations!: {}", p.display()))?
                .to_string_lossy()
                .into_owned();
            Ok((stem, p))
        })
        .collect::<Result<_, String>>()?;

    // The convention IS the identity contract on both engines: NNNN_name,
    // zero-padded, lexicographic == applied order. Five digits are
    // rejected on purpose — they would sort before four ("10000" < "9999").
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (stem, _) in &files {
        let digits = stem.chars().take_while(|c| c.is_ascii_digit()).count();
        let rest = &stem[digits..];
        if digits != 4 || !rest.starts_with('_') || rest.len() < 2 {
            return Err(format!(
                "migrations!: {stem} does not follow the NNNN_name.sql convention \
                 (four zero-padded digits + underscore) — sqlx::migrate! and the \
                 client's apply-once tracker must agree on order and identity"
            ));
        }
    }
    for w in files.windows(2) {
        if w[0].0 == w[1].0 {
            return Err(format!(
                "migrations!: duplicate version in {} — two files claim {}",
                dir.display(),
                w[0].0
            ));
        }
    }

    let mut code = String::from("&[");
    for (stem, path) in &files {
        // include_str! with the ABSOLUTE path: the tokens compile in the
        // caller's crate, so relative resolution would start at the
        // caller's source file — unknowable from here (the demo's
        // generated OUT_DIR file needed the same trick).
        code.push_str(&format!(
            "({} , include_str!({})),\n",
            quoted(stem),
            quoted(&path.display().to_string())
        ));
    }
    code.push(']');

    Ok(code.parse().expect("migrations!: generated tokens"))
}

/// Exactly one string literal — the directory. No syn dependency: the
/// grammar is one literal, everything else is a usage error.
fn dir_literal(input: TokenStream) -> Result<String, String> {
    let mut trees = input.into_iter();
    let path = match trees.next() {
        Some(TokenTree::Literal(lit)) => {
            let s = lit.to_string();
            let trimmed = s
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .ok_or_else(|| format!("migrations!: expected a string literal, got {s}"))?
                .to_string();
            if trimmed.is_empty() {
                return Err("migrations!: empty directory path".into());
            }
            trimmed
        }
        other => {
            return Err(format!(
                "migrations!: expected one string literal (the migrations directory), got {other:?}"
            ));
        }
    };
    // A trailing comma is tolerated (macro call sites grow args); anything
    // else after the literal is a mistake.
    match trees.next() {
        None => Ok(path),
        Some(TokenTree::Punct(p)) if p.as_char() == ',' && p.spacing() == Spacing::Alone => {
            match trees.next() {
                None => Ok(path),
                Some(t) => Err(format!(
                    "migrations!: unexpected token after the directory path: {t}"
                )),
            }
        }
        Some(t) => Err(format!(
            "migrations!: unexpected token after the directory path: {t}"
        )),
    }
}

fn quoted(s: &str) -> String {
    // Escape for a Rust string literal — Windows paths and names with
    // quotes survive (the repo is unix, the macro is not trusted to be).
    format!("{:?}", s)
}

fn compile_error(message: &str) -> TokenStream {
    format!("compile_error!({});", quoted(message))
        .parse()
        .expect("migrations!: compile_error tokens")
}
