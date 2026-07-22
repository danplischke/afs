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
| **M9 · GC** | Mark-and-sweep garbage collection: reclaim content no ref or live file references | ✅ done |
| **M9 · Import** | Import a `tursodatabase/agentfs` SQLite DB (tree + audit) with agent attribution | ✅ done |
| **M9 · Encrypt** | Encryption at rest (XChaCha20-Poly1305), dedup-preserving, transparent to the engine | ✅ done |
| **M9 · Bench** | Criterion benchmarks over the chunk/write/read/commit hot paths + encryption overhead | ✅ done |
| **M5 · Remote** | `git-remote-afs` helper: real `git clone` / `fetch` / `push` over `afs://` | ✅ done |
| M7, M8 | more surfaces (NFS/API), live collaboration | ⬜ |

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
  afs-remote-git/ # `git-remote-afs` helper: clone/fetch/push over afs:// URLs
  afs-agentfs/  # import a tursodatabase/agentfs SQLite database into a workspace
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

Or go over the wire: with `git-remote-afs` on `PATH`, the real `git` clones,
fetches, and pushes an afs workspace through `afs://` URLs — no export step:

```bash
git clone afs://"$WS" checkout      # clone an afs workspace with real git
cd checkout && echo hi >> readme.md && git commit -am edit
git push origin main                # the push lands back in the afs workspace
```

### Reclaiming space

Content is addressed and never overwritten, so churn (overwrites, deleted files,
abandoned branches) leaves orphaned chunks behind. Garbage collection is a
mark-and-sweep from the refs and the live working tree:

```bash
afs --workspace "$WS" gc     # kept N object(s), deleted M (… bytes freed)
```

Run it when the workspace is idle — it is not safe to run concurrently with
writers.

### Coming from agentfs

An existing [`tursodatabase/agentfs`](https://github.com/tursodatabase/agentfs)
database imports directly — its files, directories, and symlinks become an afs
working tree, and its `tool_calls` audit log folds into afs's own audit. By
default the imported tree is attributed to a synthetic `agentfs` agent actor, so
`afs blame` shows it as agent-authored:

```bash
afs --workspace "$WS" import-agentfs ./agent.db
afs --workspace "$WS" blame /some/file     # agent:agentfs
```

### Encryption at rest

Content can be encrypted before it ever touches disk or object storage
(XChaCha20-Poly1305), transparently to the engine — the address stays the
plaintext hash, so metadata, versioning, and GC are unchanged, and **dedup still
works** (convergent encryption). Opt in by setting `AFS_ENCRYPTION_KEY`:

```bash
export AFS_ENCRYPTION_KEY="correct horse battery staple"
echo 'secret' | afs --workspace "$WS" write /notes.txt   # ciphertext on disk
afs --workspace "$WS" read /notes.txt                     # plaintext back
```

The same key must be used every time; the wrong key fails loudly rather than
returning garbage. From Rust, use `Workspace::open_local_encrypted` or wrap any
`ContentStore` in an `EncryptedStore`.

## Development

```bash
cargo test --workspace
```

### Benchmarks

Criterion micro-benchmarks over the hot paths (chunk + BLAKE3 write, whole-file
read, commit/tree building, and the cost of encryption) run over the in-memory
store, so they reflect afs's own CPU cost rather than disk or network:

```bash
cargo bench -p afs-core
```

Indicative single-threaded numbers (release build, in-memory store): writes chunk
+ hash at ~1.3 GiB/s and reads reassemble at ~10 GiB/s; encryption at rest costs
roughly 2× on write and is decrypt-bound on read.

## License

MIT OR Apache-2.0
