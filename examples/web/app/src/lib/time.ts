// Compact relative-time formatting for unix-seconds timestamps.

export function relativeTime(unixSecs: number): string {
  const deltaMs = Date.now() - unixSecs * 1000;
  const s = Math.round(deltaMs / 1000);
  if (s < 0) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.round(h / 24);
  return `${d}d ago`;
}

export function shortHash(hash: string): string {
  return hash.slice(0, 10);
}
