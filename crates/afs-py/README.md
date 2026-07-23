# afs-py — Python bindings for afs

Async-native bindings so you can drive an afs workspace from Python — write your
own FastAPI endpoints, inject the authenticated user/agent behind each write,
and orchestrate FUSE/NFS mounts.

Every I/O method returns an **awaitable**, so it drops straight into `async def`
handlers. Structured results come back as plain `dict`/`list` (JSON-serializable).

## Build

```bash
cd crates/afs-py
python -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop            # builds the extension module + installs it
pytest tests/              # end-to-end test
```

Wheels: `maturin build --release` (abi3, one wheel works on CPython ≥ 3.9).

## Use

```python
import afs

ws = await afs.Workspace.open_local("meta.db", "cas")
# ...or multi-writer on Postgres with S3-shared content (the production combo):
#   cfg = afs.S3Config(bucket="my-bucket", region="us-east-1")   # + endpoint/keys
#   ws  = await afs.Workspace.open_pg_s3(dsn, cfg)               # or open_pg_s3_packed
# object-store constructors verify content integrity on read (a bit-rotted object
# errors instead of being served). open_object_memory(db) runs the same adapter
# with no network, for local dev/tests.

# Inject the identity you resolved in your endpoint:
ctx = afs.WriteCtx.session(actor_id, session_id)   # or afs.WriteCtx.actor(id)
await ws.write_as(ctx, "/notes.txt", b"hello")      # attributed -> blame + audit

diff = await ws.diff("main", "feature")             # [{"path","status"}, ...]
sid  = await ws.suggest(ctx, "/x", b"proposed")     # agent proposes; not applied
await ws.accept_suggestion(sid, afs.WriteCtx.actor(reviewer))  # applied, credited
```

Errors map to Python exceptions: missing path → `FileNotFoundError`, bad arg →
`ValueError`, a stale suggestion base → `afs.ConflictError`.

## FastAPI router (bring your own auth)

afs has no built-in authentication — a blame trail is only trustworthy if the
identity behind each write is, and that's yours to own. `afs.fastapi.build_router`
gives you every workspace endpoint wired up, with attribution driven by an auth
dependency **you** provide:

```python
from fastapi import FastAPI, Header, HTTPException
import afs
from afs.fastapi import build_router

async def authn(x_actor_id: int = Header(...)) -> afs.WriteCtx:
    # decode your JWT / session / agent token -> the afs actor to attribute to
    if x_actor_id is None:
        raise HTTPException(401)
    return afs.WriteCtx.actor(x_actor_id)

app = FastAPI()
app.include_router(build_router(ws, authn=authn), prefix="/fs")
```

Every mutating route depends on `authn` and passes its `WriteCtx` straight to the
workspace — the request body never names an actor, so a client can't forge
attribution. Reads are open by default; pass `reader=<dependency>` to gate them,
or `dependencies=[...]` (forwarded to `APIRouter`) to gate everything. Needs the
`fastapi` extra (`pip install "afs[fastapi]"`). See `examples/fastapi_router.py`.

## Mount orchestration

```python
mount = ws.mount("/mnt/afs")        # FUSE, in the background; returns a handle
mount.unmount()                      # or use `with ws.mount(...) as m:`

import asyncio
nfs = asyncio.create_task(ws.serve_nfs("127.0.0.1:11111"))  # runs until cancelled
nfs.cancel()
```

See `examples/fastapi_app.py` for a full FastAPI app (file CRUD, blame, diff,
the suggestion review queue, the live feed, and presence).

## API surface

`Workspace`: `open_local` · `open_local_packed` · `open_pg` · `open_s3` ·
`open_s3_packed` · `open_pg_s3` · `open_pg_s3_packed` · `open_object_memory` ·
`read` · `write` ·
`write_as` · `mkdir_p` · `ls` · `stat` · `remove` · `rename` · `commit` · `log` ·
`status` · `diff` · `diff_file` · `create_branch` · `checkout` · `branches` ·
`current_branch` · `create_human` · `create_agent` · `create_session` · `blame` ·
`watch` · `presence` · `touch` · `suggest` · `suggest_delete` · `list_suggestions` ·
`get_suggestion` · `suggestion_diff` · `accept_suggestion` · `reject_suggestion` ·
`mount` · `serve_nfs`. Plus `WriteCtx`, `S3Config`, `Mount`, `fuse_mountable()`.
