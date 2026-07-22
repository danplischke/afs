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
| M2 | Postgres metadata backend (multi-writer) | ⬜ |
| M3–M5 | Opt-in versioning, three-way merge, real-`git` interop | ⬜ |
| M6 | Per-actor attribution + blame | ⬜ |
| M7–M9 | Access surfaces, live collaboration, hardening | ⬜ |

## Layout

```
crates/
  afs-core/   # MetadataStore + ContentStore traits, SQLite + local-CAS impls, the Fs engine
  afs-sdk/    # ergonomic Workspace façade
  afs-cli/    # `afs` command-line tool
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

## Development

```bash
cargo test --workspace
```

## License

MIT OR Apache-2.0
