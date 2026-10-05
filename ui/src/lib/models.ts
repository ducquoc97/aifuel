import { useCallback, useSyncExternalStore } from "react";
import { apiGet } from "./api";

// Shared GET /api/gateway/models cache - one response feeds the route
// pickers (integration list, per-integration catalog, reasoning lists)
// and the key allowlist suggestions.

export interface ModelEntry {
  id: string;
  reasoning?: string[];
  default_reasoning?: string;
  [k: string]: unknown;
}

let entries: ModelEntry[] = [];
let loaded = false;
let inflight: Promise<ModelEntry[]> | null = null;
const listeners = new Set<() => void>();

function notify() {
  for (const l of listeners) l();
}

export function refreshModels(force = false): Promise<ModelEntry[]> {
  if (loaded && !force) return Promise.resolve(entries);
  if (inflight) return inflight;
  inflight = apiGet<{ data?: ModelEntry[] }>("/api/gateway/models")
    .then((data) => {
      entries = data.data || [];
      loaded = true;
      return entries;
    })
    .catch(() => entries)
    .finally(() => {
      inflight = null;
      notify();
    });
  return inflight;
}

export function useModels(): { entries: ModelEntry[]; ids: string[]; refresh: (force?: boolean) => Promise<ModelEntry[]> } {
  const snapshot = useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => entries,
  );
  const refresh = useCallback((force?: boolean) => refreshModels(force), []);
  return { entries: snapshot, ids: snapshot.map((e) => e.id).filter(Boolean), refresh };
}
