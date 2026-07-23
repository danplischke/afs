"""Example: mount the whole afs workspace API with your own auth.

`afs.fastapi.build_router` gives you every workspace endpoint — files, blame,
versioning, diff, the suggestion review queue, the change feed, presence —
wired to a workspace, with attribution driven by an auth dependency *you*
provide. afs has no built-in auth on purpose: a blame trail is only trustworthy
if the identity behind each write is, and that's yours to own.

Run:
    pip install "afs[fastapi]"        # or: maturin develop && pip install fastapi uvicorn
    uvicorn fastapi_router:app --reload

Then, having created an actor (e.g. `id = await ws.create_human("dan", ...)`):
    curl -X PUT --data-binary 'hello' -H 'X-Actor-Id: 1' \
         http://127.0.0.1:8000/fs/files/notes.txt
    curl http://127.0.0.1:8000/fs/files/notes.txt          # -> hello
    curl http://127.0.0.1:8000/fs/blame/notes.txt          # credited to actor 1
    curl -X PUT --data-binary 'x' http://127.0.0.1:8000/fs/files/y   # 401: no identity
"""
from contextlib import asynccontextmanager
from typing import Optional

from fastapi import FastAPI, Header, HTTPException

import afs
from afs.fastapi import build_router


# --- your auth --------------------------------------------------------------
# Resolve the request's principal to the afs actor it should be attributed to.
# Swap the header check for your real auth (decode a JWT, look up a session,
# validate an agent token) and map that principal to an afs actor id. The
# request body never names an actor, so a caller cannot forge attribution.

async def authn(
    x_actor_id: Optional[int] = Header(default=None),
    x_session_id: Optional[int] = Header(default=None),
) -> afs.WriteCtx:
    if x_actor_id is None:
        raise HTTPException(status_code=401, detail="unauthenticated")
    # (authorize here too if you like: is this actor allowed to write?)
    if x_session_id is not None:
        return afs.WriteCtx.session(x_actor_id, x_session_id)
    return afs.WriteCtx.actor(x_actor_id)


@asynccontextmanager
async def lifespan(app: FastAPI):
    ws = await afs.Workspace.open_local("meta.db", "cas")   # or open_pg(dsn, "cas")
    # Mount the full workspace API under /fs. Reads are open here; pass
    # reader=<dependency> to gate them, or dependencies=[...] to gate everything.
    app.include_router(build_router(ws, authn=authn), prefix="/fs")
    app.state.ws = ws
    yield


app = FastAPI(lifespan=lifespan)


# Your own endpoints live alongside it — e.g. onboarding that creates the actor
# id your `authn` will later resolve to.
@app.post("/users")
async def create_user(name: str):
    return {"actor_id": await app.state.ws.create_human(name, None)}
