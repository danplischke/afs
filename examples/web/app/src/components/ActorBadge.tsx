// A small, colored actor chip: kind glyph + name, in the actor's stable color.

import { actorColor, kindGlyph, type ActorColor } from "../lib/colors";
import type { Actor, ActorKind } from "../lib/types";

export function ActorChip({
  name,
  kind,
  color,
  title,
  faded,
}: {
  name: string;
  kind: ActorKind;
  color?: ActorColor;
  title?: string;
  faded?: boolean;
}) {
  const c = color ?? actorColor(0, kind);
  return (
    <span
      className="actor-chip"
      title={title ?? name}
      style={{
        color: c.fg,
        background: c.bg,
        borderColor: c.fg,
        opacity: faded ? 0.55 : 1,
      }}
    >
      <span aria-hidden>{kindGlyph(kind)}</span>
      <span className="actor-chip-name">{name}</span>
    </span>
  );
}

/** Chip for a full blame actor (carries kind + model). */
export function ActorChipFor({ actor }: { actor: Actor }) {
  const title =
    actor.kind === "agent" && actor.agent_model
      ? `${actor.display_name} · ${actor.agent_model}`
      : actor.display_name;
  return (
    <ActorChip
      name={actor.display_name}
      kind={actor.kind}
      color={actorColor(actor.id, actor.kind)}
      title={title}
    />
  );
}
