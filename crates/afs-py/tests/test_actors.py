"""find_or_create_actor: map your app's user id to an afs actor, idempotently.

Build + run (from crates/afs-py, in a venv):
    maturin develop
    python tests/test_actors.py       # or: pytest tests/
"""
import asyncio
import os
import tempfile

import afs


def test_find_or_create_is_idempotent_by_subject():
    async def _exercise():
        d = tempfile.mkdtemp()
        ws = await afs.Workspace.open_local(
            os.path.join(d, "meta.db"), os.path.join(d, "cas")
        )

        # unknown subject -> None
        assert await ws.actor_by_subject("user_42") is None

        # first call creates; second call with the same subject returns the SAME id
        a1 = await ws.find_or_create_human("user_42", "Dan")
        a2 = await ws.find_or_create_human("user_42", "Dan (renamed)")
        assert a1 == a2

        # a different subject is a different actor
        b = await ws.find_or_create_human("user_99", "Sam")
        assert b != a1

        # the lookup now resolves and carries the identity
        found = await ws.actor_by_subject("user_42")
        assert found is not None
        assert found["id"] == a1
        assert found["auth_subject"] == "user_42"
        assert found["kind"] == "human"

        # agents key on the subject the same way
        g1 = await ws.find_or_create_agent("agent_token_7", "claude", "opus", a1)
        g2 = await ws.find_or_create_agent("agent_token_7", "claude", "opus", a1)
        assert g1 == g2 and g1 != a1
        assert (await ws.actor_by_subject("agent_token_7"))["kind"] == "agent"

    asyncio.run(_exercise())


def _run_all():
    import inspect

    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and inspect.isfunction(fn):
            fn()
            print("ok  ", name)
    print("ALL OK")


if __name__ == "__main__":
    _run_all()
