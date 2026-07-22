//! afs-core — the storage-agnostic core of the afs filesystem.
//!
//! M0 wires together two pluggable abstractions and a working-tree engine:
//!
//! - [`MetadataStore`] — names, inodes, symlinks (SQLite in M0, Postgres in M2).
//! - [`ContentStore`] — content-addressed blobs ([`LocalCasStore`] in M0; S3 in M1).
//! - [`Fs`] — POSIX-flavored operations over the two.
//!
//! See `docs/DESIGN.md` for the full architecture and the milestone roadmap.

pub mod content;
pub mod engine;
pub mod error;
pub mod metadata;
pub mod sqlite;
pub mod types;
mod util;

pub use content::{ContentStore, LocalCasStore};
pub use engine::Fs;
pub use error::{AfsError, Result};
pub use metadata::MetadataStore;
pub use sqlite::SqliteMetadataStore;
pub use types::{DirEntry, FileKind, Hash, INO_ROOT, Ino, Inode, InodeInit};
