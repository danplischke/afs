"""afs — an agent-and-human filesystem, driven from Python.

The async workspace API (``Workspace``, ``WriteCtx``, ``Mount`` …) is implemented
in Rust and imported from the compiled ``afs._afs`` extension; everything it
exports is re-exported here, so ``import afs`` is unchanged::

    import afs
    ws = await afs.Workspace.open_local("meta.db", "cas")
    ctx = afs.WriteCtx.session(actor_id, session_id)
    await ws.write_as(ctx, "/notes.txt", b"hello")

Optional integrations live in submodules you import explicitly (each pulls in
its own extra dependencies only when used):

    from afs.fastapi import build_router   # needs `pip install "afs[fastapi]"`
"""
from ._afs import (
    AfsError,
    ConflictError,
    Mount,
    Workspace,
    WriteCtx,
    fuse_mountable,
)

__all__ = [
    "AfsError",
    "ConflictError",
    "Mount",
    "Workspace",
    "WriteCtx",
    "fuse_mountable",
]
