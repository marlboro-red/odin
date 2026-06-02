//! Durable [`crate::traits::Store`] implementations.
//!
//! v1 ships [`SqliteStore`], a single-file SQLite backend. The trait's snapshot-primary
//! contract means the whole [`crate::traits::RunState`] is persisted as one JSON blob, so
//! the store needs no knowledge of the IR.

mod sqlite;

pub use sqlite::SqliteStore;
