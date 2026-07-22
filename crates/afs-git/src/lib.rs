//! afs-git — drive an afs workspace with the real `git` (`docs/DESIGN.md` §4c,
//! the git-interop layer; roadmap M5).
//!
//! afs stays BLAKE3-native internally; this crate bridges its opt-in commit DAG
//! to genuine git objects in both directions:
//!
//! - [`export_git`] re-encodes an afs branch as real git objects under a `.git`
//!   directory the actual `git` binary reads (`log`, `diff`, `blame`,
//!   `checkout`, `fsck`) — in SHA-1 (GitHub-compatible) or SHA-256 (afs's native
//!   256-bit ids), with large files optionally written as git-LFS pointers.
//! - [`import_git`] reads a real git repository's history back into afs commits,
//!   trees, and blobs, then checks the branch out.
//!
//! Because git records only commit-granular authorship, the finer per-line
//! human-vs-agent attribution (`docs/DESIGN.md` §4d) stays in afs's own tables;
//! git interop neither needs nor disturbs it.
//!
//! The `afs-remote-git` crate builds on this to provide `git-remote-afs`, so the
//! real `git` can `clone`/`fetch`/`push` an afs workspace over `afs://`. Reading
//! packed (non-loose) objects on import remains a follow-up.

mod export;
mod import;
mod object;

pub use export::{export_git, ExportOptions, GitExport};
pub use import::import_git;
pub use object::ObjectFormat;
