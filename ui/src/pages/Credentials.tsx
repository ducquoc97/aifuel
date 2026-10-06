import { useCallback, useEffect, useState, type FormEvent } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

// Credentials page: one row per credential-bearing HTTP integration,
// mirroring `aifuel auth list` for source status and `aifuel auth
// set-key`/`set-session`/`remove` for mutations. Material travels only
// inside its POST body to this loopback server; it is never rendered
// back into the page.

interface AuthEntry {
  id: string;
  name: string;
  kind: "api_key" | "session";
  stored: boolean;
  env_set?: boolean;
  env_var?: string;
  accepts_key?: boolean;
  credential?: string;
  source?: string;
}

function labels(entry: AuthEntry) {
  return entry.kind === "session"
    ? { noun: "session", stored: "Session stored", placeholder: "Paste session token or Cookie header", aria: `session credential for ${entry.name}`, button: "Save session", saved: "Stored session credential" }
    : { noun: "key", stored: "Key stored", placeholder: "Paste API key", aria: `API key for ${entry.name}`, button: "Save key", saved: "Stored API key" };
}

async function post(path: string, body: unknown) {
  const r = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  let data: { error?: string; warnings?: string[] } = {};
  try {
    data = await r.json();
  } catch {
    /* non-JSON or empty body */
  }
  if (!r.ok || data.error) throw new Error(data.error || "HTTP " + r.status);
  return data;
}

function CredentialRow({ entry, onChanged }: { entry: AuthEntry; onChanged: () => void }) {
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const l = labels(entry);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const noun = entry.kind === "session" ? "session credential" : "API key";
    setBusy(true);
    try {
      await post("/api/auth/set-key", { integration: entry.id, key: value });
      setValue("");
      toast.success(`Stored ${noun} for ${entry.id}.`);
      onChanged();
    } catch (e2) {
      toast.error(String((e2 as Error).message || e2));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!entry.credential) return;
    if (!window.confirm(`Remove stored credential ${entry.credential}?`)) return;
    try {
      const data = await post("/api/auth/remove", { credential: entry.credential });
      const warnings = (data.warnings || []).join("; ");
      toast.success(`Removed ${entry.credential}.${warnings ? " " + warnings : ""}`);
      onChanged();
    } catch (e) {
      toast.error(String((e as Error).message || e));
    }
  };

  return (
    <div className="flex flex-wrap items-center justify-between gap-4 rounded-[11px] border-[0.5px] border-line bg-canvas px-[18px] py-3.5 shadow-[0_1px_2px_rgba(29,29,31,0.05)]">
      <div className="min-w-[240px] flex-1">
        <div className="text-sm font-semibold tracking-[-0.008em]">
          {entry.name} <span className="text-xs font-normal text-ink-ter">{entry.id}</span>
        </div>
        <div className="mt-2 flex flex-wrap gap-1.5">
          {entry.stored ? (
            <Badge variant="ok">{l.stored}</Badge>
          ) : entry.env_set && entry.env_var ? (
            <Badge variant="accent">env {entry.env_var} set</Badge>
          ) : (
            <Badge>No credential</Badge>
          )}
        </div>
        {entry.source && (
          <div className="mt-1.5 text-xs text-ink-48 tabular-nums">{entry.source}</div>
        )}
      </div>
      {entry.accepts_key ? (
        <form className="flex flex-none items-center gap-2" onSubmit={submit}>
          <Input
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder={l.placeholder}
            aria-label={l.aria}
            className="w-[230px]"
            value={value}
            onChange={(e) => setValue(e.target.value)}
          />
          <Button type="submit" disabled={busy}>
            {l.button}
          </Button>
          {entry.stored && (
            <Button type="button" variant="danger" onClick={remove}>
              Remove
            </Button>
          )}
        </form>
      ) : (
        <div className="text-[12.5px] text-ink-ter">
          Reads its {l.noun} from {entry.env_var} - export it in your shell.
        </div>
      )}
    </div>
  );
}

export default function Credentials() {
  const [entries, setEntries] = useState<AuthEntry[]>([]);
  const [error, setError] = useState("");

  const load = useCallback(async () => {
    try {
      const r = await fetch("/api/auth");
      const data: { integrations?: AuthEntry[]; error?: string } = await r
        .json()
        .catch(() => ({}));
      if (!r.ok || data.error) throw new Error(data.error || "HTTP " + r.status);
      setEntries(data.integrations || []);
      setError("");
    } catch (e) {
      setEntries([]);
      setError(`Connect list failed: ${String((e as Error).message || e)}`);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <section aria-label="Provider credentials">
      <p className="m-0 max-w-[62ch] text-[13px] leading-[1.45] tracking-[-0.006em] text-ink-48">
        Store an API key or a browser-session credential for an integration - the same store{" "}
        <code className="rounded-[5px] bg-ink/[0.04] px-1.5 py-px text-xs">aifuel auth set-key</code> and{" "}
        <code className="rounded-[5px] bg-ink/[0.04] px-1.5 py-px text-xs">aifuel auth set-session</code>{" "}
        write. Credentials are saved locally, never displayed.
      </p>
      <div className="mt-4 flex flex-col gap-2.5">
        {entries.length === 0 && !error && (
          <div className="text-[12.5px] text-ink-ter">
            No credential-bearing integrations are registered.
          </div>
        )}
        {entries.map((e) => (
          <CredentialRow key={e.id} entry={e} onChanged={load} />
        ))}
      </div>
      {error && (
        <div role="status" aria-live="polite" className="mt-3 text-[12.5px] text-err">
          {error}
        </div>
      )}
    </section>
  );
}
