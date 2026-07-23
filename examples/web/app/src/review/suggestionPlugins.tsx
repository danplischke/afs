// Native track-changes plugins for the inline review editor.
//
// Two leaf marks render the agent's change through Plate's own leaf pipeline —
// inserted text as <ins> (green), removed text as <del> (red, struck) — and a
// `suggestion_change` element wraps each change with a ✓ Keep / ✗ Discard
// control. The per-change decision lives in a plugin option, read reactively by
// the element via usePluginOption, so toggling re-renders just that change.

import { createPlatePlugin, PlateElement, PlateLeaf, usePluginOption, type PlateElementProps, type PlateLeafProps } from "platejs/react";

export const SuggestInsertPlugin = createPlatePlugin({
  key: "suggestion_insert",
  node: { isLeaf: true },
  render: { node: (props: PlateLeafProps) => <PlateLeaf {...props} as="ins" className="w-add" /> },
});

export const SuggestDeletePlugin = createPlatePlugin({
  key: "suggestion_delete",
  node: { isLeaf: true },
  render: { node: (props: PlateLeafProps) => <PlateLeaf {...props} as="del" className="w-del" /> },
});

interface ChangeOptions {
  decisions: Record<number, boolean>;
  onToggle?: (hunk: number, keep: boolean) => void;
}

function SuggestChangeElement(props: PlateElementProps) {
  const hunk = (props.element as { hunk?: number }).hunk ?? 0;
  const decisions = (usePluginOption(SuggestChangePlugin, "decisions") ?? {}) as Record<number, boolean>;
  const onToggle = usePluginOption(SuggestChangePlugin, "onToggle") as ChangeOptions["onToggle"];
  const kept = decisions[hunk] ?? true;
  return (
    <PlateElement {...props} as="div" className={`sug-change ${kept ? "kept" : "discarded"}`}>
      <span className="sug-controls" contentEditable={false}>
        <button
          type="button"
          className={kept ? "active" : ""}
          onClick={() => onToggle?.(hunk, true)}
          title="Keep — accept the agent's change"
        >
          ✓
        </button>
        <button
          type="button"
          className={!kept ? "active" : ""}
          onClick={() => onToggle?.(hunk, false)}
          title="Discard — keep the original"
        >
          ✗
        </button>
      </span>
      {props.children}
    </PlateElement>
  );
}

export const SuggestChangePlugin = createPlatePlugin({
  key: "suggestion_change",
  node: { isElement: true },
  options: { decisions: {}, onToggle: undefined } as ChangeOptions,
  render: { node: SuggestChangeElement },
});
