// Only `sync` is part of the public API — `SyncOptions`, `SyncReport` and
// friends are re-exported from the crate root. The other three are how sync
// does its job, not something a consumer drives directly, so they stay
// crate-internal per design rule 5.
pub(crate) mod classify;
pub(crate) mod secrets;
pub mod sync;
pub(crate) mod walk;
