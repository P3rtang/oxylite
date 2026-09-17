//! One line, one job: cargo tracks DIRECTORIES for build scripts only —
//! the `migrations!` proc-macro cannot express rerun-if-changed, so
//! without this a NEW file in migrations/ would not re-expand the macro
//! (edits of tracked files rebuild via include_str! dep-info; additions
//! need this). Same line every consumer ships.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
