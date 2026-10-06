import { useCallback, useEffect, useState, type FormEvent } from "react";
import { toast } from "sonner";
import { apiGet, apiPost } from "@/lib/api";
import { fmtDate } from "@/lib/format";
import { useModels } from "@/lib/models";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { TagInput } from "@/components/TagInput";
import { CopyButton, Msg, usePageHeader } from "./shared";
import { cn } from "@/lib/utils";

// API Keys: create, list, edit model allowlist, revoke.

interface GatewayKey {
  id: string;
  name: string;
  prefix: string;
  models?: string[];
  revoked?: boolean;
  created_at?: number;
  last_used_at?: number;
}

export default function Keys() {
  const [keys, setKeys] = useState<GatewayKey[]>([]);
  const [error, setError] = useState("");
  const [editing, setEditing] = useState<string | null>(null);
  const [editModels, setEditModels] = useState<string[]>([]);
  const [newName, setNewName] = useState("");
  const [newModels, setNewModels] = useState<string[]>([]);
  const [newKey, setNewKey] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { ids: modelIds, refresh: refreshModels } = useModels();

  const load = useCallback(async () => {
    try {
      const data = await apiGet<{ keys?: GatewayKey[] }>("/api/gateway/keys");
      const list = data.keys || [];
      setKeys(list);
      setEditing((cur) => (cur && !list.some((k) => k.id === cur) ? null : cur));
      setError("");
    } catch (e) {
      setKeys([]);
      setError(`Keys failed to load: ${(e as Error).message}`);
    }
  }, []);

  usePageHeader(async () => {
    await Promise.allSettled([load(), refreshModels(true)]);
  });
  useEffect(() => {
    load();
    refreshModels();
  }, [load, refreshModels]);

  // POST /api/gateway/keys omits `models` when the allowlist is empty -
  // an empty list would store a permit-nothing allowlist instead.
  const create = async (e: FormEvent) => {
    e.preventDefault();
    const name = newName.trim();
    if (!name) {
      setError("Give the key a name first.");
      return;
    }
    setBusy(true);
    try {
      const body: { name: string; models?: string[] } = { name };
      if (newModels.length) body.models = newModels;
      const data = await apiPost<{ key?: string }>("/api/gateway/keys", body);
      setNewName("");
      setNewModels([]);
      setNewKey(data.key || "");
      load();
    } catch (e2) {
      setError((e2 as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const saveModels = async (id: string) => {
    try {
      const body: { id: string; models?: string[] } = { id };
      if (editModels.length) body.models = editModels;
      await apiPost("/api/gateway/keys/update", body);
      setEditing(null);
      toast.success("Updated model allowlist.");
      load();
    } catch (e) {
      toast.error(`Update failed: ${(e as Error).message}`);
    }
  };

  const revoke = async (k: GatewayKey) => {
    if (!window.confirm(`Revoke gateway key "${k.name || k.id}"? Apps using it stop working immediately.`))
      return;
    try {
      await apiPost("/api/gateway/keys/revoke", { id: k.id });
      toast.success(`Revoked ${k.name || k.id}.`);
      load();
    } catch (e) {
      toast.error(`Revoke failed: ${(e as Error).message}`);
    }
  };

  return (
    <section aria-label="API Keys">
      <p className="m-0 max-w-[66ch] text-[13px] leading-[1.45] tracking-[-0.006em] text-ink-48">
        Gateway keys authenticate clients as{" "}
        <code className="rounded-[5px] bg-ink/[0.04] px-1.5 py-px text-xs">Authorization: Bearer aifuel-gw-…</code>.
        Leave the model allowlist empty to permit every model.
      </p>

      {newKey !== null && (
        <div role="status" className="mt-3.5 flex flex-wrap items-center justify-between gap-4 rounded-[11px] border-[0.5px] border-line bg-canvas px-[18px] py-3.5 shadow-[0_1px_2px_rgba(29,29,31,0.05)]">
          <div className="min-w-0">
            <div className="text-[13px] font-semibold">Copy this key now - it will not be shown again.</div>
            <div className="mt-1 select-all break-all font-mono text-xs text-ink">{newKey}</div>
            <div className="mt-1 text-xs text-ink-48">Only the key prefix appears in this dashboard afterwards.</div>
          </div>
          <div className="flex gap-2">
            <CopyButton text={newKey} />
            <Button type="button" onClick={() => setNewKey(null)}>Done</Button>
          </div>
        </div>
      )}

      <form className="mt-3.5 flex flex-col gap-2" onSubmit={create}>
        <div className="flex flex-wrap items-center gap-2">
          <Input
            name="name"
            maxLength={80}
            required
            placeholder="Key name - e.g. my app"
            aria-label="Key name"
            className="w-[220px]"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
          />
          <Button type="submit" disabled={busy}>Create key</Button>
        </div>
        <TagInput
          values={newModels}
          onChange={setNewModels}
          listId="gw-model-list"
          suggestions={modelIds}
          placeholder="Model allowlist (optional) - type a selector, Enter to add"
          aria-label="Model allowlist, optional"
        />
        <p className="mt-1 text-xs leading-[1.4] text-ink-ter">
          Add model selectors the key may call - <code className="rounded bg-ink/[0.04] px-1">auto</code>,{" "}
          <code className="rounded bg-ink/[0.04] px-1">codex</code>,{" "}
          <code className="rounded bg-ink/[0.04] px-1">groq:api-key/llama-3.3-70b</code>. Empty permits every model.
        </p>
      </form>

      <Card className="mt-3.5 overflow-x-auto px-[18px] pb-2.5 pt-1.5">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Prefix</TableHead>
              <TableHead>Models</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Last used</TableHead>
              <TableHead>State</TableHead>
              <TableHead aria-label="Actions" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {keys.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="py-5 text-center text-ink-ter">
                  No gateway keys yet - create one above.
                </TableCell>
              </TableRow>
            )}
            {keys.map((k) => (
              <TableRow key={k.id} className={cn(k.revoked && "[&_td:not(:nth-last-child(-n+2))]:opacity-50")}>
                <TableCell className="font-medium text-ink">{k.name}</TableCell>
                <TableCell className="font-mono text-xs" title={k.id}>{k.prefix}</TableCell>
                <TableCell>
                  {editing === k.id ? (
                    <TagInput
                      values={editModels}
                      onChange={setEditModels}
                      listId="gw-model-list"
                      suggestions={modelIds}
                      placeholder="Type a selector, Enter to add"
                      autoFocus
                    />
                  ) : k.models && k.models.length ? (
                    k.models.map((m) => (
                      <span key={m} className="mb-0.5 mr-1 mt-0.5 inline-block break-all rounded-md bg-ink/[0.04] px-[7px] py-0.5 font-mono text-[11px] text-ink-48">
                        {m}
                      </span>
                    ))
                  ) : (
                    <span className="text-ink-ter">all models</span>
                  )}
                </TableCell>
                <TableCell>{fmtDate(k.created_at)}</TableCell>
                <TableCell>
                  {k.last_used_at ? fmtDate(k.last_used_at) : <span className="text-ink-ter">never</span>}
                </TableCell>
                <TableCell>
                  {k.revoked ? <Badge variant="err">revoked</Badge> : <Badge variant="ok">active</Badge>}
                </TableCell>
                <TableCell className="whitespace-nowrap text-right">
                  {editing === k.id ? (
                    <>
                      <Button size="sm" onClick={() => saveModels(k.id)}>Save</Button>{" "}
                      <Button size="sm" onClick={() => setEditing(null)}>Cancel</Button>
                    </>
                  ) : k.revoked ? (
                    <span className="text-ink-ter">-</span>
                  ) : (
                    <>
                      <Button size="sm" onClick={() => { setEditing(k.id); setEditModels(k.models || []); }}>
                        Edit models
                      </Button>{" "}
                      <Button size="sm" variant="danger" onClick={() => revoke(k)}>
                        Revoke
                      </Button>
                    </>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Card>
      <Msg>{error && <span className="text-err">{error}</span>}</Msg>
    </section>
  );
}
