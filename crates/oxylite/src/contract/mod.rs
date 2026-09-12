//! The plug-in contracts an app fulfills to consume the lib — the
//! lib-boundary's app side (see spec/lib-boundary.md): the table enum's
//! identity ([`table`]), the row write contract whose ONE declaration
//! feeds both appliers with the same generated SQL ([`sync_row`]), and
//! the read-side decode ([`from_row`], feature `client` — decoding is
//! side-specific, the write contract is shared).

pub mod sync_row;
pub mod table;

#[cfg(feature = "client")]
pub mod from_row;
