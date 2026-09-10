//! The server crate's library half: the sync machinery as a lib target,
//! so integration tests (crates/server/tests) and the thin binary share
//! one build of the apply/pull/snapshot pipeline.
pub mod sync;
