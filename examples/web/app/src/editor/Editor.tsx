// The authoring surface: a PlateJS Markdown editor bound to a document in afs.
//
// - Loads by deserializing the stored Markdown into Plate nodes.
// - Saves by serializing back to Markdown and doing an **attributed** write
//   (write_as) — so afs records blame + an audit op crediting the signed-in
//   principal. A client can't forge that: the token is resolved server-side.
// - "Suggest" queues the same content as a proposal instead (the working tree is
//   untouched until a reviewer accepts it).
// - An authorship gutter shows who wrote each block, from afs's per-line blame.

import { useCallback, useMemo, useRef, useState } from "react";
import { Plate, PlateContent, usePlateEditor } from "platejs/react";
import { deserializeMd, serializeMd } from "@platejs/markdown";
import { editorPlugins } from "./plugins";
import { AttributionGutter } from "./AttributionGutter";
import { blockAuthorship, blockLineSpans, type BlockAuthorship } from "../lib/blame";
import { useSession } from "../session";
import { AfsError } from "../lib/afsClient";
import type { BlameRange } from "../lib/types";

export function EditorTab({
  path,
  initialText,
  blame,
  onSaved,
}: {
  path: string;
  initialText: string;
  blame: BlameRange[];
  onSaved: () => void;
}) {
  const { client, token } = useSession();

  const editor = usePlateEditor({
    plugins: editorPlugins,
    value: (ed) => deserializeMd(ed, initialText || ""),
  });

  const containerRef = useRef<HTMLDivElement>(null);
  const [nodes, setNodes] = useState<unknown[]>(() => editor.children as unknown[]);
  const [revision, setRevision] = useState(0);
  const [status, setStatus] = useState<{ kind: "ok" | "err"; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [showGutter, setShowGutter] = useState(true);

  const authorship = useMemo<BlockAuthorship[]>(
    () => blockAuthorship(blockLineSpans(editor, nodes), blame),
    [editor, nodes, blame],
  );

  const onChange = useCallback(() => {
    setNodes([...(editor.children as unknown[])]);
    setRevision((r) => r + 1);
  }, [editor]);

  const save = useCallback(async () => {
    setBusy(true);
    setStatus(null);
    try {
      const md = serializeMd(editor);
      const { written } = await client.writeDoc(path, md);
      setStatus({ kind: "ok", text: `saved ${written} bytes — attributed to you` });
      onSaved();
    } catch (e) {
      setStatus({ kind: "err", text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  }, [client, editor, path, onSaved]);

  const suggest = useCallback(async () => {
    const summary = window.prompt("Summary for this suggestion:", "propose an edit");
    if (summary === null) return;
    setBusy(true);
    setStatus(null);
    try {
      const md = serializeMd(editor);
      const id = await client.suggest(path, md, summary || undefined);
      setStatus({ kind: "ok", text: `suggestion #${id} queued — not applied until a reviewer accepts it` });
    } catch (e) {
      const text = e instanceof AfsError ? e.message : e instanceof Error ? e.message : String(e);
      setStatus({ kind: "err", text });
    } finally {
      setBusy(false);
    }
  }, [client, editor, path]);

  return (
    <div className="editor-tab">
      <div className="toolbar">
        <button className="primary" disabled={!token || busy} onClick={save} title="Attributed write to the working tree (write_as)">
          Save
        </button>
        <button disabled={!token || busy} onClick={suggest} title="Queue a suggestion — the working tree is untouched until a reviewer accepts">
          Suggest…
        </button>
        <label className="toggle">
          <input type="checkbox" checked={showGutter} onChange={(e) => setShowGutter(e.target.checked)} />
          attribution gutter
        </label>
        <span className="spacer" />
        {!token && <span className="hint">sign in to edit</span>}
        {status && <span className={`status ${status.kind}`}>{status.text}</span>}
      </div>
      <div className={`editor-surface ${showGutter ? "with-gutter" : ""}`} ref={containerRef}>
        {showGutter && (
          <AttributionGutter containerRef={containerRef} authorship={authorship} revision={revision} />
        )}
        <Plate editor={editor} onChange={onChange}>
          <PlateContent className="plate-content" placeholder="Write Markdown…" spellCheck />
        </Plate>
      </div>
    </div>
  );
}
