//! The synced-table contract. The lib never names a table (boundary
//! rule 1): the implementing repo's table enum derives
//! `enum_iterator::Sequence` (so a new variant joins every loop
//! automatically) and implements this for its wire names. Lives in the
//! ungated core (#31): the generic wire DTOs' bound needs it, so it
//! cannot sit behind the `server` feature.

use enum_iterator::Sequence;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use std::hash::Hash;

/// The wire-identity + machinery supertraits. Deliberately NOT serde:
/// the DTO derives add `T: Serialize` / `T: Deserialize<'de>` themselves
/// — inheriting the HRTB supertrait here would give the compiler two
/// competing proofs of the same bound (E0283/E0521). The app's enum
/// derives serde on its own.
pub trait SyncTable: Copy + Clone + Sequence + Eq + Hash + Debug + Send + Sync + 'static {
    /// The wire name stored in `sync_log.table_name` and
    /// `snapshots.table_name`; must stay stable across releases.
    fn as_str(self) -> &'static str;

    /// Inverse of [`SyncTable::as_str`]. Returning `None` means "unknown
    /// here" (a table from a newer client): replaying sites skip it —
    /// this side is the compat boundary and can't apply what it doesn't
    /// know.
    fn from_name(name: &str) -> Option<Self>;
}

/// The serde the ENGINE's message types need on top of [`SyncTable`] —
/// wire serialization for socket and BroadcastChannel traffic. A blanket
/// helper rather than supertraits, for the same single-proof-path reason
/// as above. The app's table enum derives serde anyway (the DTOs demand
/// it), so for every real app this is satisfied for free.
pub trait SyncTableWire: SyncTable + Serialize + DeserializeOwned {}
impl<T: SyncTable + Serialize + DeserializeOwned> SyncTableWire for T {}
