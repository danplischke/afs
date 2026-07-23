// Inline review of a pending agent suggestion — the VSCode "review agent edits"
// experience, rendered *in the document*: unchanged text reads normally, the
// agent's changes appear inline (old text struck through in red, new text in
// green), and you Keep or Discard each change right there.
//
// Keep-all applies through afs's native accept (atomic, credits the agent). A
// partial keep is reconstructed server-side and written *as the agent*, so the
// agent stays credited for its lines — the reviewer only ever chooses which
// changes, never authors them.

import { useCallback, useEffect, useMemo, useState } from "react";
import { diffWordsWithSpace } from "diff";
import { useSession } from "../session";
import { AfsError } from "../lib/afsClient";
import { ActorChip } from "../components/ActorBadge";
import { DiffText } from "../panels/DiffText";
import { renderProse } from "./prose";
import type { SuggestionDetail } from "../lib/types";

export function ReviewOverlay({
  id,
  onDone,
  onCancel,
}: {
  id: number;
  onDone: (msg: string) => void;
  onCancel: () => void;
}) {
  const { client, token } = useSession();
  const [detail, setDetail] = useState<SuggestionDetail | null>(null);
  const [keep, setKeep] = useState<Record<number, boolean>>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    setError(null);
    client
      .suggestionDetail(id)
      .then((d) => {
        if (!live) return;
        setDetail(d);
        const k: Record<number, boolean> = {};
        for (let i = 0; i < (d.hunks ?? 0); i++) k[i] = true; // default: keep everything
        setKeep(k);
      })
      .catch((e) => live && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      live = false;
    };
  }, [client, id]);

  const total = detail?.hunks ?? 0;
  const keptIdx = useMemo(
    () => Object.entries(keep).filter(([, v]) => v).map(([k]) => Number(k)),
    [keep],
  );
  const setAll = useCallback(
    (v: boolean) => {
      const k: Record<number, boolean> = {};
      for (let i = 0; i < total; i++) k[i] = v;
      setKeep(k);
    },
    [total],
  );

  const apply = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await client.applySuggestion(id, keptIdx);
      onDone(
        res.mode === "accept"
          ? `kept all ${res.total} changes — accepted (credited to the agent)`
          : res.kept === 0
            ? "discarded — the proposal was rejected"
            : `kept ${res.kept}/${res.total} changes — written as the agent`,
      );
    } catch (e) {
      setError(
        e instanceof AfsError && e.status === 409
          ? "The document changed since this was proposed (stale base). Ask the agent to re-propose."
          : e instanceof Error
            ? e.message
            : String(e),
      );
    } finally {
      setBusy(false);
    }
  }, [client, id, keptIdx, onDone]);

  if (error && !detail) return <div className="notice err">{error}</div>;
  if (!detail) return <div className="empty">Loading proposal…</div>;

  // Not stashed (proposed straight to /fs): show the unified diff read-only.
  if (detail.segments === null) {
    return (
      <div className="review">
        <div className="review-bar">
          <span>
            Proposal #{id} by <ActorChip name={detail.actor_name} kind={detail.actor_kind} /> —
            read-only (review it in the Suggestions tab)
          </span>
          <button onClick={onCancel}>Close</button>
        </div>
        <div className="review-body">
          <DiffText text={detail.unified ?? ""} />
        </div>
      </div>
    );
  }

  const kept = keptIdx.length;
  return (
    <div className="review">
      <div className="review-bar">
        <span className="review-title">
          Reviewing <ActorChip name={detail.actor_name} kind={detail.actor_kind} />
          {detail.summary ? <span className="muted"> — {detail.summary}</span> : null}
        </span>
        <span className="review-count">
          {kept}/{total} {total === 1 ? "change" : "changes"} kept
        </span>
        <span className="row-actions">
          <button onClick={() => setAll(true)}>Keep all</button>
          <button onClick={() => setAll(false)}>Discard all</button>
          <button className="primary" disabled={busy || !token} onClick={apply} title={token ? "" : "sign in to apply"}>
            Apply
          </button>
          <button disabled={busy} onClick={onCancel}>
            Cancel
          </button>
          {!token && <span className="hint">sign in to apply</span>}
        </span>
      </div>
      {error && <div className="notice err">{error}</div>}
      <div className="suggestion-doc plate-content">
        {detail.segments.map((seg, i) => {
          if (seg.hunk === null) {
            // Unchanged text — rendered like the document.
            return <div className="sug-equal" key={`e${i}`}>{renderProse(seg.del)}</div>;
          }
          const h = seg.hunk;
          const on = keep[h];
          // Word-level diff of the removed vs added lines, so only the changed
          // words are colored — the rest reads as normal text.
          const parts = diffWordsWithSpace(seg.del.join("\n"), seg.add.join("\n"));
          return (
            <div className={`sug-change ${on ? "kept" : "discarded"}`} key={`h${i}`}>
              <span className="sug-controls" role="group" aria-label={`change ${h + 1}`}>
                <button
                  className={on ? "active" : ""}
                  onClick={() => setKeep((k) => ({ ...k, [h]: true }))}
                  title="Keep — accept the agent's change"
                >
                  ✓
                </button>
                <button
                  className={!on ? "active" : ""}
                  onClick={() => setKeep((k) => ({ ...k, [h]: false }))}
                  title="Discard — keep the original"
                >
                  ✗
                </button>
              </span>
              <span className="sug-text">
                {parts.map((p, j) =>
                  p.removed ? (
                    <del className="w-del" key={j}>
                      {p.value}
                    </del>
                  ) : p.added ? (
                    <ins className="w-add" key={j}>
                      {p.value}
                    </ins>
                  ) : (
                    <span key={j}>{p.value}</span>
                  ),
                )}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
