# afs

**A filesystem where humans and AI agents share the same files — and you always
know who changed what.**

afs is content-addressed storage with a real metadata database (Postgres or
SQLite), opt-in Git-style versioning, and per-actor attribution built in. Point
your agents at it and let them work: every edit is recorded against the actor
that made it, an agent's whole session can be reverted in one call, and the bytes
you read back are cryptographically guaranteed to be the bytes that were written.

```bash
# an agent works in a fast native mount; its edits stream into afs, attributed
afs --workspace ./ws overlay --actor "$AGENT" -- claude -p "refactor the parser"

# afterwards, see exactly which lines the agent wrote
afs --workspace ./ws blame /src/parser.rs
```

Don't like the result? One SDK call undoes everything that agent did in a
session — across every file it touched — and leaves human edits untouched.

---

## Why afs

When people and AI agents edit the same workspace, a plain filesystem stops being
enough. You need to answer questions a directory of files can't:

- **Who wrote this line — a person or an agent?** Every attributed write records
  the actor, session, and tool-call behind it. `afs blame` reports it per line;
  the record survives commits, branch switches, and reformatting.
- **Can I undo just the agent's work?** Revert an agent's entire session across
  every file it touched, keeping everyone else's edits intact.
- **Can I review before it lands?** Agents can *propose* edits into a review
  queue instead of applying them; a human accepts (credited to the agent) or
  rejects.
- **Will it hold up for a team?** The Postgres backend is built for many
  concurrent writers — humans and agents — sharing one workspace, with a live
  change feed and presence so every client sees edits as they happen.
- **Can I trust what I read back?** Content is BLAKE3-addressed and verified on
  every read: silent bit-rot or tampering in object storage surfaces as an error
  instead of being served as if it were real.

afs isn't a wrapper over `git` or a VFS shim — it's a storage engine with these
properties at its core, exposed through a CLI, a Rust SDK, Python bindings, an
HTTP API, and real filesystem mounts (FUSE/NFS).

## Install

afs is a Rust workspace. Build the `afs` CLI with a recent stable toolchain:

```bash
cargo install --path crates/afs-cli     # installs the `afs` binary
# or, without installing:
cargo build --release                    # ./target/release/afs
```

A workspace is just a directory afs manages (metadata DB + content store). For a
team deployment, point it at Postgres and object storage instead — see
[Running for a team](#running-for-a-team) and [Storage backends](#storage-backends).

## Quickstart

```bash
WS=./ws
afs --workspace "$WS" init
echo 'hello from afs' | afs --workspace "$WS" write /notes/a.txt
afs --workspace "$WS" ls   /notes
afs --workspace "$WS" read /notes/a.txt
afs --workspace "$WS" stat /notes/a.txt
```

From Rust:

```rust
use afs_sdk::Workspace;

let ws = Workspace::open_local("meta.db", "cas").await?;   // or open_pg(dsn, cas)
ws.mkdir_p("/notes").await?;
ws.write("/notes/a.txt", b"hello").await?;
let bytes = ws.read("/notes/a.txt").await?;
```

## Working with agents

### A native mount agents edit in place

The fastest way to put an agent to work is a **live overlay mount**. afs sets up
an unprivileged kernel overlay over the workspace, runs your agent inside it, and
streams the agent's changes back into afs — *attributed, as they happen*, not
only when the process exits:

```bash
afs --workspace "$WS" overlay --actor "$AGENT" --sync-ms 500 -- \
    some-agent --do-the-thing
```

The agent sees an ordinary directory and reads/writes at native speed; afs
captures each change (create, modify, delete) into the content store and records
it against `--actor`. When it finishes, `afs blame` and the change feed already
reflect everything it did. This is how agents are meant to interact with afs day
to day.

Prefer a protocol integration? afs also speaks **MCP** (Model Context Protocol)
over stdio, so an agent can call filesystem tools directly — and every write is
attributed to the agent:

```bash
afs --workspace "$WS" mcp --agent-name claude --model claude-opus-4-8
```

### Propose-and-review, not just apply

An agent can submit an edit for human review instead of applying it. The proposed
bytes go straight into the content store (deduplicated, diffable); the working
tree doesn't change until someone accepts:

```bash
echo "patched" | afs --workspace "$WS" suggest /main.rs --actor "$AGENT" --summary "fix bug"
afs --workspace "$WS" suggestions --status pending
afs --workspace "$WS" suggestion-diff 1              # base → proposed, unified diff
afs --workspace "$WS" accept 1 --actor "$HUMAN"      # applies it, credited to the agent
```

`accept` lands the edit **attributed to the authoring agent** (so blame stays
honest) and records the approver; it refuses if the file moved since the proposal
(a stale base). `reject` discards it.

## Know who did what

Every attributed write (`write_as`) records an append-only edit-op — actor,
session, tool-call, before/after content — and updates a per-line authorship map.
`afs blame` then reports, per line range, whether a **human** or an **agent** wrote
it:

```bash
afs --workspace "$WS" blame /src/parser.rs
#    1-40   human:dan
#   41-58   agent:claude
#   59-72   human:dan
```

Blame is keyed by **content**, so it stays correct where naïve line-tracking
breaks: it survives commits and branch checkouts (the map travels with the bytes,
never desyncing from the file), a re-indent or a moved block keeps its original
author rather than being credited to whoever reformatted, and content produced
outside the attributed path simply blames to nothing instead of showing stale
authorship.

Undo an agent's work without touching anyone else's — `revert_session` walks
every file the agent touched in that session and removes exactly the lines it
authored, leaving surrounding human edits in place:

```rust
let files_changed = ws.revert_session(agent_id, session_id).await?;
```

## Versioning

Versioning is opt-in and Git-shaped — a real commit DAG, branches, checkout, log,
status, three-way merge, and locks — but backed by afs's content-addressed store,
so snapshots are incremental (only changed chunks are stored) and identical trees
are shared across commits for free.

```bash
afs --workspace "$WS" commit -m "initial" --author "Dan <dan@example.com>"
afs --workspace "$WS" branch feature
afs --workspace "$WS" checkout feature
afs --workspace "$WS" diff main feature                # changed-path list
afs --workspace "$WS" diff main feature --path /x.rs   # one file's line diff
```

Branch comparison works on content addresses, not file reads: equal hashes mean
an identical file (a 32-byte compare), so a diff only ever reads the paths that
actually changed — the metadata trees *are* the index.

### Real-`git` interop

afs stays BLAKE3-native internally, but its history projects to — and imports
from — genuine git objects, so you can keep using the `git` CLI and hosts like
GitHub:

```bash
# afs history → a real git repo the `git` binary reads directly
afs --workspace "$WS" git export ./repo --format sha256   # or sha1 for GitHub
git -C ./repo log --oneline
git -C ./repo fsck --strict                                # clean

# a real git repo → afs history
afs --workspace "$WS2" git import ./repo --branch main
```

With `git-remote-afs` on your `PATH`, the real `git` can even clone, fetch, and
push an afs workspace over `afs://` URLs — no export step:

```bash
git clone afs://"$WS" checkout
cd checkout && echo hi >> readme.md && git commit -am edit && git push origin main
```

Large files can be exported as git-LFS pointer blobs (`--lfs-threshold <bytes>`),
backed by afs's chunk store.

## Running for a team

For a shared human+agent workspace, run afs on **Postgres** — the backend built
for many concurrent writers. Atomic-create is serialized so racing writers never
leave orphaned inodes, and the whole write path is transactional: content is made
durable first, then metadata, blame, and the audit log commit together, so a
crash can never leave a half-recorded edit.

```rust
let ws = Workspace::open_pg("host=db port=5432 user=afs dbname=afs", content).await?;
```

### Live collaboration

Every operation lands on an append-only **change feed** (who touched what, who
committed), and each session heartbeats its **presence** (which actor, which
path). Tail the feed by cursor, or — on Postgres — let `LISTEN/NOTIFY` push new
events so clients never poll:

```bash
afs --workspace "$WS" watch --follow    # live feed: seq  kind  actor  path
afs --workspace "$WS" presence          # who's active right now
```

The feed is **exactly-once and in commit order** even under concurrent writers,
and every event is **branch-scoped**, so a UI showing `main` filters to one
branch. From Rust, `PostgresMetadataStore::subscribe(after_seq, branch)` returns
a blocking `LISTEN`-backed subscription whose `recv()` wakes on every committed
change — a real push, not a poll.

### HTTP API

Every operation is available over HTTP/JSON — files as raw bytes, everything else
as JSON — so any client or service can drive a workspace. Writes go through the
same path as every other surface, so they land on the change feed and carry
attribution:

```bash
afs --workspace "$WS" serve --addr 127.0.0.1:8080 &
curl -X PUT --data-binary 'hello' http://127.0.0.1:8080/files/notes/a.txt
curl 'http://127.0.0.1:8080/files/notes/a.txt'                   # → hello
curl -X POST -d '{"author":"dan","message":"first"}' http://127.0.0.1:8080/commit
curl 'http://127.0.0.1:8080/events?since=0'                      # the change feed
```

An attributed write is `PUT /files/x?actor=<id>&session=<id>`. Full routes cover
files, dirs, stat, blame, rename, commit/log, branches/checkout, events,
presence, actors, sessions, diff, and suggestions.

## Built to not lose or corrupt data

Because agents can generate a lot of churn against shared storage, correctness
under failure is a first-class concern:

- **Corruption never passes as real.** Content is BLAKE3-addressed and re-hashed
  on read against the address it was fetched by. A flipped bit, a truncated
  object, or tampering in object storage surfaces as a precise error — down to the
  offending chunk — instead of being handed back as authentic. Compaction
  re-verifies every surviving chunk before dropping the old copy.
- **Writes are atomic and durable.** Content is flushed before the metadata that
  references it commits, and an edit's inode, content, blame, and audit entry all
  land in one transaction — or none of them do.
- **Blame can't lie.** Authorship is tied to content, so it can never drift out
  of sync with the file it annotates (see [Know who did what](#know-who-did-what)).

## Storage backends

Content addressing means a chunk's identity is its BLAKE3 hash, so dedup,
versioning, and integrity hold no matter where bytes live.

- **Local** — a sharded content-addressed directory. `Workspace::open_local`.
- **Object storage (S3/R2/GCS)** — `Workspace::open_s3`. Content-defined chunking
  keeps edits cheap (only changed chunks re-upload), and a **pack layer**
  (`open_s3_packed`) batches chunks into large pack objects so you make a few big
  PUTs instead of thousands of tiny ones, with a small local index for single
  ranged-GET reads. `repack()` reclaims space from deleted chunks.
- **Encryption at rest** — wrap any backend so content is encrypted
  (XChaCha20-Poly1305) before it touches disk or the network, transparently to
  the engine. The address stays the plaintext hash, so **dedup still works**
  (convergent encryption). Set `AFS_ENCRYPTION_KEY` or use
  `Workspace::open_local_encrypted`.

Content is addressed and never overwritten, so churn leaves orphaned chunks
behind; mark-and-sweep garbage collection reclaims them:

```bash
afs --workspace "$WS" gc     # run when idle — not safe alongside active writers
```

## Interfaces

| Surface | Use it for |
|---|---|
| **`afs` CLI** | Scripting and day-to-day workspace operations |
| **Rust SDK** (`afs-sdk`) | Embedding afs in a Rust service |
| **Python** (`afs-py`) | Async-native PyO3 bindings — FastAPI-ready, resolve identity yourself |
| **HTTP API** (`afs-api`) | Any language / any client over JSON |
| **MCP** (`afs-mcp`) | Agents calling filesystem tools directly, attributed |
| **Overlay mount** | Running an agent live in a fast native mount |
| **FUSE / NFS** | Mounting the workspace as a POSIX filesystem |

Python, for example, keeps every I/O method awaitable so it composes with
FastAPI, and lets you inject the user/agent behind each write:

```python
import afs
ws  = await afs.Workspace.open_local("meta.db", "cas")   # or open_pg(dsn, cas)
ctx = afs.WriteCtx.session(actor_id, session_id)          # your resolved identity
await ws.write_as(ctx, "/notes.txt", b"hello")            # attributed → blame + audit
```

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```

The Postgres backend tests self-skip unless `AFS_PG_TEST_URL` points at a
reachable database:

```bash
AFS_PG_TEST_URL="host=127.0.0.1 port=5432 user=postgres dbname=afs" cargo test --workspace
```

### Performance

Criterion micro-benchmarks cover the hot paths (chunk + BLAKE3 write, whole-file
read, commit/tree building, encryption overhead) over an in-memory store, so they
reflect afs's own CPU cost rather than disk or network:

```bash
cargo bench -p afs-core
```

Indicative single-threaded numbers (release, in-memory store): writes chunk + hash
at ~1.3 GiB/s and reads reassemble at ~10 GiB/s; encryption at rest costs roughly
2× on write and is decrypt-bound on read.

## Design

The full design and rationale — the metadata/content split, the versioning model,
attribution, and the failure-surface work — live in
[`docs/DESIGN.md`](docs/DESIGN.md). afs was inspired by
[`tursodatabase/agentfs`](https://github.com/tursodatabase/agentfs).

## License

MIT OR Apache-2.0
