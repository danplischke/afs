// GitLens-style inline attribution: a faint annotation at the caret's line
// showing who authored it, in the author's color. Rendered inside <Plate> so it
// can read the live selection; positioned against the editor surface.

import { useLayoutEffect, useState, type RefObject } from "react";
import { useEditorSelection } from "platejs/react";
import type { BlockAuthorship } from "../lib/blame";
import { actorColor, kindGlyph } from "../lib/colors";
import type { Actor } from "../lib/types";

interface Anno {
  top: number;
  actor: Actor;
  mixed: boolean;
  lineStart: number;
  lineEnd: number;
}

export function InlineBlameAnnotation({
  containerRef,
  authorship,
}: {
  containerRef: RefObject<HTMLDivElement | null>;
  authorship: BlockAuthorship[];
}) {
  const selection = useEditorSelection();
  const [anno, setAnno] = useState<Anno | null>(null);

  useLayoutEffect(() => {
    const container = containerRef.current;
    const blockIndex = selection?.anchor?.path?.[0];
    if (!container || blockIndex == null) {
      setAnno(null);
      return;
    }
    const editable = container.querySelector('[data-slate-editor="true"]');
    if (!editable) {
      setAnno(null);
      return;
    }
    const blocks = Array.from(editable.children).filter(
      (el) => el.getAttribute("data-slate-node") === "element",
    ) as HTMLElement[];
    const el = blocks[blockIndex];
    const info = authorship[blockIndex];
    if (!el || !info?.primary) {
      setAnno(null);
      return;
    }
    const base = container.getBoundingClientRect().top;
    setAnno({
      top: el.getBoundingClientRect().top - base,
      actor: info.primary,
      mixed: info.mixed,
      lineStart: info.lineStart,
      lineEnd: info.lineEnd,
    });
  }, [selection, authorship, containerRef]);

  if (!anno) return null;
  const c = actorColor(anno.actor.id, anno.actor.kind);
  const lines =
    anno.lineStart === anno.lineEnd ? `line ${anno.lineStart}` : `lines ${anno.lineStart}–${anno.lineEnd}`;
  return (
    <div className="inline-blame" style={{ top: anno.top, color: c.fg }} aria-hidden>
      <span>{kindGlyph(anno.actor.kind)}</span>
      <span className="inline-blame-name">
        {anno.actor.display_name}
        {anno.mixed ? " (+ others)" : ""}
      </span>
      <span className="inline-blame-meta">· {lines}</span>
    </div>
  );
}
