// Build a Plate value from the server's diff segments, for the native review
// editor. Unchanged text is deserialized from Markdown (so it renders richly);
// each change becomes a `suggestion_change` element whose children are
// word-level text runs marked `suggestion_insert` / `suggestion_delete`.

import { createSlateEditor, type Value } from "platejs";
import { deserializeMd } from "@platejs/markdown";
import { diffWordsWithSpace } from "diff";
import { editorPlugins } from "../editor/plugins";
import type { DiffSegment } from "../lib/types";

// A headless editor just for deserializing the unchanged Markdown blocks.
let helper: ReturnType<typeof createSlateEditor> | null = null;
function helperEditor() {
  if (!helper) helper = createSlateEditor({ plugins: editorPlugins });
  return helper;
}

export function buildSuggestionValue(segments: DiffSegment[]): Value {
  const value: Value = [];
  for (const seg of segments) {
    if (seg.hunk === null) {
      const md = seg.del.join("\n");
      if (md.trim() === "") continue;
      value.push(...(deserializeMd(helperEditor(), md) as Value));
    } else {
      const parts = diffWordsWithSpace(seg.del.join(" "), seg.add.join(" "));
      const children = parts.map((p) =>
        p.added
          ? { text: p.value, suggestion_insert: true }
          : p.removed
            ? { text: p.value, suggestion_delete: true }
            : { text: p.value },
      );
      value.push({
        type: "suggestion_change",
        hunk: seg.hunk,
        children: children.length ? children : [{ text: "" }],
      });
    }
  }
  return value.length ? value : [{ type: "p", children: [{ text: "" }] }];
}
