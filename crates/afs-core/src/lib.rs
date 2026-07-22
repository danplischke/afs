//! afs-core — the storage-agnostic core of the afs filesystem.
//!
//! M0 wires together two pluggable abstractions and a working-tree engine:
//!
//! - [`MetadataStore`] — names, inodes, symlinks (SQLite in M0, Postgres in M2).
//! - [`ContentStore`] — content-addressed blobs ([`LocalCasStore`] in M0; S3 in M1).
//! - [`Fs`] — POSIX-flavored operations over the two.
//!
//! See `docs/DESIGN.md` for the full architecture and the milestone roadmap.

pub mod attribution;
pub mod chunk;
pub mod content;
pub mod engine;
pub mod error;
pub mod interop;
pub mod merge;
pub mod metadata;
pub mod migrations;
pub mod objectgraph;
pub mod objectstore;
pub mod postgres;
pub mod sqlite;
pub mod types;
mod util;
pub mod version;
pub mod vfs;

pub use attribution::{Actor, ActorInit, ActorKind, BlameRange, EditOp, ToolCallInit, WriteCtx};
pub use chunk::{AVG_CHUNK, ChunkRef, MAX_CHUNK, MIN_CHUNK, Manifest};
pub use content::{ContentStore, LocalCasStore, MemStore, TieredStore};
pub use engine::Fs;
pub use error::{AfsError, Result};
pub use merge::{Conflict, MergeOutcome};
pub use metadata::MetadataStore;
pub use objectgraph::{
    Commit, CommitInfo, DiffEntry, DiffStatus, Tree, TreeEntry, TreeKind, VersioningMode,
};
pub use objectstore::{ObjectContentStore, S3Config};
pub use postgres::PostgresMetadataStore;
pub use sqlite::SqliteMetadataStore;
pub use types::{DirEntry, FileKind, Hash, INO_ROOT, Ino, Inode, InodeInit};
