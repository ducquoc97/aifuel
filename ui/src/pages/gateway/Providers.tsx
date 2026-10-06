import { useCallback, useEffect, useState } from "react";
import { apiGet } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { Card } from "@/components/ui/card";
import { Monogram } from "@/components/Monogram";
import { Msg, usePageHeader } from "./shared";

// Providers: executable integrations grouped by the kind the API
// reports per integration (derived server-side from the id suffix).

interface ProviderInfo {
  integration: string;
  provider: string;
  kind: string;
  ready?: boolean | null;
  embeddings?: boolean;
}

const KIND_ORDER = ["cli", "api-key", "oauth", "local", "web", "other"];
const KIND_LABEL: Record<string, string> = {
  cli: "CLI integrations",
  "api-key": "API-key endpoints",
  oauth: "OAuth integrations",
  local: "Local servers",
  web: "Web sessions",
  other: "Other",
};

// Dot semantics from the API's `ready` flag: green = credential or
// presence evidence is present locally; gray = absent; amber = evidence
// could not be evaluated. Presence means "configured", not "verified
// working" - the first real request still proves the route.
function providerDot(p: ProviderInfo): [string, string] {
  if (p.ready === true) return ["var(--ok)", "credential or presence evidence found"];
  if (p.ready === false) return ["var(--ink-ter)", "no local evidence - set a key or sign in"];
  return ["var(--warn)", "evidence unavailable"];
}

function ProviderCard({ p }: { p: ProviderInfo }) {
  const [color, dotTitle] = providerDot(p);
  return (
    <Card className="min-w-0 px-4 py-3">
      <div className="flex items-center gap-2">
        <Monogram name={p.provider} />
        <span
          className="truncate font-mono text-[12.5px] font-semibold text-ink"
          title={p.integration}
        >
          {p.integration}
        </span>
        <span
          className="ml-auto size-2 flex-none rounded-full"
          style={{ background: color }}
          title={dotTitle}
        />
      </div>
      <div className="mt-1 text-xs text-ink-48">
        provider: <span className="font-mono">{p.provider}</span>
      </div>
      <div className="mt-2.5 flex flex-wrap gap-1.5">
        <Badge variant="accent">chat</Badge>
        {p.embeddings && <Badge variant="info">embeddings</Badge>}
      </div>
    </Card>
  );
}

export default function Providers() {
  const [groups, setGroups] = useState<[string, ProviderInfo[]][]>([]);
  const [empty, setEmpty] = useState(false);
  const [error, setError] = useState("");

  const load = useCallback(async () => {
    try {
      const data = await apiGet<{ providers?: ProviderInfo[] }>("/api/gateway/providers");
      const list = data.providers || [];
      setError("");
      setEmpty(!list.length);
      const byKind = new Map<string, ProviderInfo[]>();
      for (const p of list) {
        const kind = KIND_LABEL[p.kind] ? p.kind : "other";
        if (!byKind.has(kind)) byKind.set(kind, []);
        byKind.get(kind)!.push(p);
      }
      setGroups(
        KIND_ORDER.filter((k) => byKind.get(k)?.length).map((k) => [k, byKind.get(k)!]),
      );
    } catch (e) {
      setGroups([]);
      setError(`Providers failed to load: ${(e as Error).message}`);
    }
  }, []);

  usePageHeader(load);
  useEffect(() => {
    load();
  }, [load]);

  return (
    <section aria-label="Providers">
      <p className="m-0 max-w-[66ch] text-[13px] leading-[1.45] tracking-[-0.006em] text-ink-48">
        Executable integrations the gateway can route requests to, grouped by integration kind. The dot
        marks whether the integration's credential or presence evidence exists locally - "ready" means
        configured, not that a live request has been verified.
      </p>
      <div className="mt-2.5 flex flex-wrap gap-x-4 gap-y-1.5 text-xs text-ink-48" aria-label="Readiness legend">
        <span className="flex items-center gap-1.5">
          <span className="size-2 rounded-full" style={{ background: "var(--ok)" }} />
          credential or presence evidence found
        </span>
        <span className="flex items-center gap-1.5">
          <span className="size-2 rounded-full" style={{ background: "var(--ink-ter)" }} />
          no local evidence
        </span>
        <span className="flex items-center gap-1.5">
          <span className="size-2 rounded-full" style={{ background: "var(--warn)" }} />
          evidence unavailable
        </span>
      </div>
      {empty && <Card className="mt-3.5 px-[18px] py-5 text-center text-[13px] text-ink-ter">No executable integrations are registered.</Card>}
      {groups.map(([kind, items]) => (
        <div key={kind}>
          <h3 className="mb-0 mt-5 text-[11px] font-bold uppercase tracking-[0.07em] text-ink-ter">
            {KIND_LABEL[kind]}
          </h3>
          <div className="mt-2 grid grid-cols-[repeat(auto-fill,minmax(240px,1fr))] gap-2.5">
            {items.map((p) => (
              <ProviderCard key={p.integration} p={p} />
            ))}
          </div>
        </div>
      ))}
      <Msg>{error && <span className="text-err">{error}</span>}</Msg>
    </section>
  );
}
