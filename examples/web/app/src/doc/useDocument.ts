// Load a document (text + per-line blame) in one round trip, with a `reload`
// to refresh blame after a write/commit/accept.

import { useCallback, useEffect, useState } from "react";
import { useSession } from "../session";
import type { DocLoad } from "../lib/types";

export interface DocState {
  doc: DocLoad | null;
  loading: boolean;
  error: string | null;
  reload: () => void;
}

export function useDocument(path: string): DocState {
  const { client } = useSession();
  const [doc, setDoc] = useState<DocLoad | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);

  const reload = useCallback(() => setNonce((n) => n + 1), []);

  // Clear on a *path* change (so the keyed editor remounts with the right text),
  // but not on a reload — a reload keeps the current doc visible while blame
  // refreshes underneath (stale-while-revalidate).
  useEffect(() => {
    setDoc(null);
  }, [path]);

  useEffect(() => {
    let live = true;
    setLoading(true);
    setError(null);
    client
      .loadDoc(path)
      .then((d) => {
        if (live) setDoc(d);
      })
      .catch((e: unknown) => {
        if (live) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (live) setLoading(false);
      });
    return () => {
      live = false;
    };
  }, [client, path, nonce]);

  return { doc, loading, error, reload };
}
