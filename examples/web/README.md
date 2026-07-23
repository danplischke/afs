# afs web — a React + PlateJS editor with lineage & attribution

A full-stack example: a **PlateJS** rich-text editor in **React**, backed by an
afs workspace through the Python **FastAPI** bindings, where every edit is
attributed to the human or agent that made it — and you can *see* it, per block
and per line, alongside the version history and an agent review queue.

It's the "humans and agents co-write a document, fully attributed" story, wired
end to end:

| | |
|---|---|
| **Attribution** | afs records per-line **blame** on every attributed write. The editor shows it two ways: an **authorship gutter** beside each block (who wrote it), and an exact **Blame tab** (every source line with its author, like `git blame`). |
| **Lineage** | The **History tab** is afs's commit DAG — pick a commit to see the unified diff it introduced. The **Suggestions tab** is the propose-and-review queue: an agent proposes an edit, a reviewer accepts it, and afs lands it *credited to the agent* while recording the approver. |
| **Live** | Presence (who's here now) and an SSE **activity feed** of every attributed change, straight off afs's change feed. |
| **Trust** | Identity is resolved **server-side**. The browser sends a bearer token; the server maps it to an afs actor and attributes the write. The request body never names an actor, so **attribution can't be forged**. |

```
┌─────────────── React + PlateJS (app/) ───────────────┐
│  Edit · Blame · History · Suggestions · Activity      │
└───────────────┬───────────────────────────────────────┘
                │  /fs/*  (afs router)   /api/*  (app layer)
┌───────────────▼───────────────────────────────────────┐
│  FastAPI doc-server (server/)                          │
│    afs.fastapi.build_router  +  bearer auth → actor    │
└───────────────┬───────────────────────────────────────┘
                │  afs Python bindings (write_as, blame, …)
┌───────────────▼───────────────────────────────────────┐
│  afs workspace  —  content store + metadata (blame)    │
└────────────────────────────────────────────────────────┘
```

## Run it

You need **two** processes: the Python doc-server and the Vite dev server.

### 1. The backend (`server/`)

Build the afs Python bindings once (they're a compiled pyo3 module), then run the
server:

```bash
cd ../../crates/afs-py
python -m venv .venv && . .venv/bin/activate
pip install maturin && maturin develop          # builds + installs the `afs` module
pip install fastapi "uvicorn[standard]"

cd ../../examples/web/server
uvicorn app:app --reload                         # http://127.0.0.1:8000
```

By default it opens a throwaway temp workspace. Point it at a durable one with
`AFS_WORKSPACE=/srv/ws` (local) or `AFS_DSN=postgres://…` (multi-writer).

### 2. The frontend (`app/`)

```bash
cd ../app          # examples/web/app
npm install
npm run dev        # http://localhost:5173  (proxies /fs and /api to :8000)
```

Open http://localhost:5173, **sign in** as Ada, Grace, or the `claude` agent
(the picker is seeded from the server's demo tokens), and start writing. Save is
an attributed write; the gutter and Blame tab update to credit you.

> The demo tokens (`tok-ada`, …) are **hardcoded for the demo only**. In a real
> app you'd resolve the bearer token to an actor with your own auth (JWT /
> session / verified agent token) — see `resolve_principal` in `server/app.py`.

### Try the whole loop

1. Sign in as **Ada**, write a few paragraphs, **Save**. The gutter credits Ada.
2. Sign in as **claude** (agent), edit, and **Suggest…** instead of Save.
3. Sign in as **Grace**, open the **Suggestions** tab, review the diff, **Accept**.
   The Blame tab now mixes Ada and claude per line — the agent's lines are
   credited to the agent, not to the reviewer.
4. **Commit snapshot**, then open **History** to see the diff.

## How attribution maps onto a rich-text editor

afs stores each document as **Markdown** and attributes it **by source line**.
PlateJS edits **blocks**. The example bridges the two honestly:

- **Storage** — the editor serializes to Markdown (`serializeMd`) on save and
  deserializes on load (`deserializeMd`), so afs keeps human-readable text, a
  meaningful line-based blame, real diffs, and 3-way merge. (Trade-off: content
  is whatever round-trips through Markdown.)
- **Blame tab** — renders the exact source lines with their authors. This is 1:1
  with what afs stored, so it's the ground truth (`src/editor/BlameView.tsx`).
- **Editor gutter** — maps blame onto blocks: each top-level block's source-line
  span is computed by serializing that block alone, then resolved to its
  dominant author (`src/lib/blame.ts`, `src/editor/AttributionGutter.tsx`). It's
  a best-effort projection of the exact line blame onto rendered blocks.

Unattributed content (a plain `write`, not `write_as`) has **no** blame — afs
returns an empty list, and the UI says so rather than crediting anyone.

## Layout

```
server/
  app.py            FastAPI: build_router(/fs) + bearer auth → actor,
                    plus /api/{config,me,actors,doc,feed}
  test_app.py       end-to-end tests (real workspace): attribution, forge-
                    prevention, the suggestion flow, commit/log
  requirements.txt
app/
  src/
    lib/            afsClient.ts (typed HTTP), types.ts (exact API shapes),
                    blame.ts (line↔block mapping), colors.ts, time.ts
    session.tsx     token → actor, the actor directory, per-actor color
    doc/            useDocument — load text + blame, stale-while-revalidate
    editor/         Editor (Plate), AttributionGutter, BlameView, plugins
    panels/         HistoryPanel, SuggestionsPanel, ActivityFeed, DiffText
    App.tsx, main.tsx, styles.css
```

## Verifying

```bash
# backend (needs the built `afs` module + fastapi/httpx/pytest)
cd server && python test_app.py          # or: pytest

# frontend types + production build
cd app && npm run build
```

## What afs provides vs. what the app provides

afs is a storage-and-attribution engine, not a user directory or an auth server —
those are deliberately yours. So the split is:

- **afs** (`/fs`, via `build_router`): files, per-line blame, versioning, diff,
  the suggestion queue, the change feed, presence — all attribution resolved
  server-side.
- **the app** (`/api`): mapping *your* users/agents onto afs actors, an actor
  **directory** so the id-only feeds (events, suggestions) resolve to a name, a
  combined document load (text + blame in one call), and the SSE feed. afs
  embeds the full actor in every blame range; everything else carries just an
  `actor_id`, and the app is what created those actors — so it's the right place
  to name them.
