# afs

An **agent-and-human filesystem**: content-addressed storage (with object-storage
and large-file support), a pluggable metadata database (Postgres or SQLite),
opt-in Git-style versioning that interoperates with the real `git`, and
per-actor edit attribution so a shared human+agent workspace always knows *who*
edited *what*.

Inspired by [`tursodatabase/agentfs`](https://github.com/tursodatabase/agentfs);
the full design and rationale live in [`docs/DESIGN.md`](docs/DESIGN.md), and the
milestone roadmap is tracked in [issue #11](https://github.com/danplischke/afs/issues/11).

## Status

Early — building milestone by milestone. **M0 (skeleton) is implemented:** the
two core abstractions, a SQLite metadata backend, a local content-addressed blob
store, the working-tree engine, an SDK, and a CLI.

| Milestone | What it adds | State |
|---|---|---|
| **M0** | Core traits + SQLite metadata + local CAS + engine + SDK + CLI | ✅ done |
| **M1** | Content addressing (BLAKE3 + FastCDC), large files, S3 backend, cache tier | ✅ done |
| **M2** | Postgres metadata backend (multi-writer), dual-dialect migrations | ✅ done |
| **M3** | Opt-in versioning: commit DAG, branches, checkout, log, status | ✅ done |
| **M4** | Three-way merge (diff3 text, chunk-granular binary), conflicts, locks | ✅ done |
| **M5** | Real-`git` interop: export/import genuine git objects (SHA-1 + SHA-256), git-LFS pointer bridge | ✅ done |
| **M6** | Per-actor attribution + blame (human vs agent), edit-op audit, revert | ✅ done |
| **Sandbox** | Isolated overlayfs CoW runs, imported back as attributed changes | ✅ done |
| **FUSE** | Mount the workspace as a POSIX filesystem (real read/write mount) | ✅ done |
| **MCP** | Serve the workspace to agents over MCP (JSON-RPC/stdio); writes attributed | ✅ done |
| M7–M9 | more surfaces (NFS/API), `git-remote-afs` helper, live collaboration, hardening | ⬜ |

## Layout

```
crates/
  afs-core/     # MetadataStore + ContentStore traits, SQLite/Postgres + CAS/S3 impls, the Fs engine
  afs-sdk/      # ergonomic Workspace façade
  afs-cli/      # `afs` command-line tool
  afs-sandbox/  # overlayfs copy-on-write sandbox runs, imported as attributed changes
  afs-fuse/     # mount the workspace as a POSIX filesystem via FUSE
  afs-mcp/      # serve the workspace to agents over the Model Context Protocol
  afs-git/      # export/import genuine git objects — drive afs with the real `git`
docs/DESIGN.md
```

## Quickstart

```bash
cargo build
WS=./ws
target/debug/afs --workspace "$WS" init
echo 'hello from afs' | target/debug/afs --workspace "$WS" write /notes/a.txt
target/debug/afs --workspace "$WS" ls   /notes
target/debug/afs --workspace "$WS" read /notes/a.txt
target/debug/afs --workspace "$WS" stat /notes/a.txt
```

Or from Rust:

```rust
use afs_sdk::Workspace;

let ws = Workspace::open_local("meta.db", "cas").await?;
ws.mkdir_p("/notes").await?;
ws.write("/notes/a.txt", b"hello").await?;
let bytes = ws.read("/notes/a.txt").await?;
```

### Git interop (opt-in)

afs stays BLAKE3-native internally, but its commit history can be projected to —
and imported from — genuine git objects, so you can keep using the real `git`
CLI and hosts like GitHub:

```bash
# afs history -> a real git repo the `git` binary reads directly
afs --workspace "$WS" commit -m "initial" --author "Dan <dan@example.com>"
afs --workspace "$WS" git export ./repo --format sha256   # or sha1 for GitHub
git -C ./repo log --oneline        # real git, reading afs-produced objects
git -C ./repo fsck --strict        # clean

# a real git repo -> afs history
afs --workspace "$WS2" git import ./repo --branch main
```

Large files can be exported as git-LFS pointer blobs (`--lfs-threshold <bytes>`),
backed by afs's content-addressed chunk store.

## Development

```bash
cargo test --workspace
```

## License

MIT OR Apache-2.0
