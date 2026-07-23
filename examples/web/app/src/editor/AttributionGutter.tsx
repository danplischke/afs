// An authorship gutter aligned to the editor's top-level blocks.
//
// It measures each top-level `[data-slate-node="element"]` in the live editable
// DOM and draws a colored bar + author chip beside it, so you can see who wrote
// each block as you edit. Decoupled from Plate's render internals on purpose:
// it reads the DOM and re-measures on layout changes, so it survives Plate
// version churn. The exact, line-precise view lives in the Blame tab.

import { useLayoutEffect, useRef, useState, type RefObject } from "react";
import type { BlockAuthorship } from "../lib/blame";
import { actorColor, kindGlyph } from "../lib/colors";

interface Row {
  top: number;
  height: number;
  a: BlockAuthorship | undefined;
  empty: boolean;
}

export function AttributionGutter({
  containerRef,
  authorship,
  revision,
}: {
  containerRef: RefObject<HTMLDivElement | null>;
  authorship: BlockAuthorship[];
  revision: number;
}) {
  const [rows, setRows] = useState<Row[]>([]);
  const raf = useRef<number | null>(null);

  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const measure = () => {
      const editable = container.querySelector('[data-slate-editor="true"]');
      if (!editable) return;
      const blocks = Array.from(editable.children).filter(
        (el) => el.getAttribute("data-slate-node") === "element",
      ) as HTMLElement[];
      const base = container.getBoundingClientRect().top;
      setRows(
        blocks.map((el, i) => {
          const r = el.getBoundingClientRect();
          // Blank lines are real (afs blames them) but noisy in the gutter, so we
          // keep the index aligned with authorship[i] and just don't draw a chip.
          return { top: r.top - base, height: r.height, a: authorship[i], empty: !el.textContent?.trim() };
        }),
      );
    };

    const schedule = () => {
      if (raf.current != null) cancelAnimationFrame(raf.current);
      raf.current = requestAnimationFrame(measure);
    };

    schedule();
    const editable = container.querySelector('[data-slate-editor="true"]');
    const ro = new ResizeObserver(schedule);
    if (editable) ro.observe(editable);
    window.addEventListener("resize", schedule);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", schedule);
      if (raf.current != null) cancelAnimationFrame(raf.current);
    };
  }, [containerRef, authorship, revision]);

  return (
    <div className="attribution-gutter" aria-hidden>
      {rows.map((row, i) => {
        if (row.empty) return null;
        const actor = row.a?.primary ?? null;
        const color = actor ? actorColor(actor.id, actor.kind) : null;
        return (
          <div className="gutter-row" key={i} style={{ top: row.top, height: Math.max(row.height, 20) }}>
            <span className="gutter-bar" style={{ background: color?.fg ?? "var(--hairline)" }} />
            {actor ? (
              <span
                className="gutter-chip"
                style={{ color: color!.fg, background: color!.bg }}
                title={`${actor.display_name}${row.a?.mixed ? " (+ others)" : ""} · lines ${row.a?.lineStart}–${row.a?.lineEnd}`}
              >
                <span aria-hidden>{kindGlyph(actor.kind)}</span>
                <span className="gutter-name">
                  {actor.display_name}
                  {row.a?.mixed ? " +" : ""}
                </span>
              </span>
            ) : (
              <span className="gutter-chip unattributed" title="unattributed">
                ·
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}
