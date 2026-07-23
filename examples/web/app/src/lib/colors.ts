// Deterministic per-actor color, so the same person/agent gets the same hue
// everywhere (gutter, badges, presence, feed) without a server round trip.
//
// Humans get cool hues, agents warm ones, so you can tell a human edit from an
// agent edit at a glance even before reading the name.

import type { ActorKind } from "./types";

function hashString(s: string): number {
  let h = 2166136261 >>> 0; // FNV-1a
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export interface ActorColor {
  /** Solid color for text/badges. */
  fg: string;
  /** Translucent fill for the gutter / line highlight. */
  bg: string;
  /** The base hue, for callers that want to derive their own shades. */
  hue: number;
}

/**
 * A stable color for an actor. Keyed by id so it's consistent even if a display
 * name changes. Humans land in the blue→violet arc, agents in the amber→red arc.
 */
export function actorColor(actorId: number, kind: ActorKind): ActorColor {
  const seed = hashString(`${kind}:${actorId}`);
  let hue: number;
  let sat: number;
  let light: number;
  if (kind === "agent") {
    hue = 20 + (seed % 45); // 20–65: amber/orange
    sat = 85;
    light = 55;
  } else if (kind === "system") {
    hue = 0;
    sat = 0; // grey
    light = 55;
  } else {
    hue = 205 + (seed % 80); // 205–285: blue/indigo/violet
    sat = 70;
    light = 55;
  }
  return {
    fg: `hsl(${hue} ${sat}% ${light}%)`,
    bg: `hsl(${hue} ${sat}% ${light}% / 0.14)`,
    hue,
  };
}

/** A short label for an actor kind, for badges. */
export function kindGlyph(kind: ActorKind): string {
  return kind === "agent" ? "🤖" : kind === "system" ? "⚙️" : "🧑";
}
