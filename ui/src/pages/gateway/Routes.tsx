import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { toast } from "sonner";
import { ChevronDown, Plus, X } from "lucide-react";
import { apiGet, apiPut } from "@/lib/api";
import { useModels } from "@/lib/models";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { SelectorPicker, assembleSelector, type PickerValue } from "@/components/SelectorPicker";
import RouteBoard, { type RouteTable } from "./RouteBoard";
import { Msg, usePageHeader } from "./shared";
import { cn } from "@/lib/utils";

// Routes: aliases + combos from gateway.json. "Dynamic" lists failover
// combos, "Aliases" lists rewrites; expanding a row reveals that
// route's graph board. Every edit commits through the whole-table PUT.

type Tab = "combos" | "aliases";

const EMPTY_PICKER: PickerValue = { agent: "", model: "", effort: "", raw: false, rawText: "" };

interface StepRow {
  id: number;
  ghost: boolean;
  value: PickerValue;
}

// Monotonic row ids - stable across the promote/update race where one
// interaction fires two state writes.
let rowId = 1;
const nextRowId = () => rowId++;

export default function Routes() {
  const [table, setTable] = useState<RouteTable>({ aliases: {}, combos: {} });
  const [tab, setTab] = useState<Tab>("combos");
  const [expanded, setExpanded] = useState<{ kind: "combo" | "alias"; name: string } | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [createName, setCreateName] = useState("");
  const [aliasPicker, setAliasPicker] = useState<PickerValue>(EMPTY_PICKER);
  const [stepRows, setStepRows] = useState<StepRow[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const { refresh: refreshModels } = useModels();

  const load = useCallback(async () => {
    try {
      const data = await apiGet<{ aliases?: Record<string, string>; combos?: Record<string, string[]> }>(
        "/api/gateway/routes",
      );
      const t = { aliases: data.aliases || {}, combos: data.combos || {} };
      setTable(t);
      setExpanded((cur) => {
        if (!cur) return cur;
        const exists =
          cur.kind === "combo" ? cur.name in t.combos : cur.name in t.aliases;
        return exists ? cur : null;
      });
      setError("");
    } catch (e) {
      setTable({ aliases: {}, combos: {} });
      setError(`Routes failed to load: ${(e as Error).message}`);
    }
  }, []);

  usePageHeader(async () => {
    await Promise.allSettled([load(), refreshModels(true)]);
  });
  useEffect(() => {
    load();
    refreshModels();
  }, [load, refreshModels]);

  // The API replaces the whole table, so every mutation is: fetch the
  // current table, apply the change client-side, PUT the merged object.
  // Unknown top-level keys (`models`, ...) pass through untouched.
  const mutate = useCallback(async (fn: (t: RouteTable) => void) => {
    const data = await apiGet<RouteTable & Record<string, unknown>>(
      "/api/gateway/routes",
    );
    const t = { ...data, aliases: data.aliases || {}, combos: data.combos || {} };
    fn(t);
    await apiPut("/api/gateway/routes", t);
  }, []);

  const spec = useMemo(
    () =>
      tab === "combos"
        ? {
            kind: "combo" as const,
            addLabel: "Add combo",
            namePh: "Route name - e.g. heavy",
            hint: "Each row is one failover step - pick integration, model, effort. The chain runs top to bottom and falls through on failure.",
          }
        : {
            kind: "alias" as const,
            addLabel: "Add alias",
            namePh: "Alias name - e.g. cheap",
            hint: 'Pick the selector this alias rewrites to, or choose "custom selector…" to type one by hand.',
          },
    [tab],
  );

  const openCreate = (show: boolean) => {
    setCreateOpen(show);
    setExpanded(null);
    if (show) {
      setCreateName("");
      setAliasPicker(EMPTY_PICKER);
      setStepRows([{ id: nextRowId(), ghost: true, value: EMPTY_PICKER }]);
    }
  };

  const switchTab = (t: Tab) => {
    setTab(t);
    setExpanded(null);
    setCreateOpen(false);
  };

  // A ghost row invites the next step; touching any field promotes it
  // and spawns a fresh ghost.
  const promoteRow = (id: number) => {
    setStepRows((rows) => {
      const next = rows.map((r) => (r.id === id ? { ...r, ghost: false } : r));
      if (!next.some((r) => r.ghost))
        next.push({ id: nextRowId(), ghost: true, value: EMPTY_PICKER });
      return next;
    });
  };

  const setRowValue = (id: number, v: PickerValue) => {
    setStepRows((rows) => rows.map((r) => (r.id === id ? { ...r, value: v } : r)));
  };

  const removeRow = (id: number) => {
    setStepRows((rows) => rows.filter((r) => r.id !== id));
  };

  const create = async (e: FormEvent) => {
    e.preventDefault();
    const name = createName.trim();
    const combo = tab === "combos";
    const selectors = combo
      ? stepRows.filter((r) => !r.ghost).map((r) => assembleSelector(r.value)).filter(Boolean)
      : [assembleSelector(aliasPicker)].filter(Boolean);
    if (!name || !selectors.length) {
      setError(
        combo
          ? "Name and at least one step are required - fill a picker row to add one."
          : "Name and a target selector are required.",
      );
      return;
    }
    setBusy(true);
    try {
      await mutate((t) => {
        if (combo) t.combos[name] = selectors;
        else t.aliases[name] = selectors[0];
      });
      toast.success(`Added ${combo ? "combo" : "alias"} ${name}.`);
      setCreateOpen(false);
      setExpanded({ kind: combo ? "combo" : "alias", name });
      load();
    } catch (e2) {
      setError((e2 as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const names = Object.keys(tab === "combos" ? table.combos : table.aliases);
  const realSteps = stepRows.filter((r) => !r.ghost);

  return (
    <section aria-label="Routes">
      <p className="m-0 max-w-[66ch] text-[13px] leading-[1.45] tracking-[-0.006em] text-ink-48">
        Named routes stored in <code className="rounded-[5px] bg-ink/[0.04] px-1.5 py-px text-xs">gateway.json</code>.
        A <b className="text-ink">dynamic</b> route is an ordered failover chain; an <b className="text-ink">alias</b>{" "}
        rewrites a model name to one fixed selector. When a name exists as both, the alias wins. Expand a
        route to edit its graph. Changes apply to new requests immediately.
      </p>

      <div className="mt-3.5 flex flex-wrap items-center justify-between gap-2.5">
        <div role="tablist" aria-label="Route kinds" className="inline-flex gap-0.5 rounded-full bg-ink/[0.04] p-[3px]">
          {(["combos", "aliases"] as const).map((t) => (
            <button
              key={t}
              role="tab"
              type="button"
              aria-selected={tab === t}
              onClick={() => switchTab(t)}
              className={cn(
                "rounded-full px-3.5 py-1 text-[12.5px] font-medium tracking-[-0.004em] transition-colors",
                tab === t
                  ? "bg-canvas font-semibold text-ink shadow-[0_1px_3px_rgba(29,29,31,0.10),0_0_0_0.5px_rgba(29,29,31,0.04)]"
                  : "text-ink-48",
              )}
            >
              {t === "combos" ? `Dynamic · ${Object.keys(table.combos).length}` : `Aliases · ${Object.keys(table.aliases).length}`}
            </button>
          ))}
        </div>
        <Button onClick={() => openCreate(!createOpen)}>
          <Plus className="size-3.5" />
          {spec.addLabel}
        </Button>
      </div>
      <p className="mx-0 mt-2.5 text-xs leading-[1.45] tracking-[-0.004em] text-ink-ter">
        <b className="font-semibold text-ink-48">Dynamic</b> when one name should survive a provider outage or
        quota limit - steps are tried in order. <b className="font-semibold text-ink-48">Alias</b> for a stable
        short name that rewrites to one fixed selector.
      </p>

      {createOpen && (
        <form className="mt-3.5 flex flex-col items-stretch gap-2" onSubmit={create}>
          <div className="flex flex-wrap items-center gap-2">
            <Input
              name="name"
              maxLength={80}
              required
              autoFocus
              placeholder={spec.namePh}
              aria-label="Route name"
              className="w-[220px]"
              value={createName}
              onChange={(e) => setCreateName(e.target.value)}
            />
            {spec.kind === "alias" && (
              <SelectorPicker value={aliasPicker} onChange={setAliasPicker} />
            )}
            <Button type="submit" variant="primary" disabled={busy}>
              {spec.addLabel}
            </Button>
            <Button type="button" onClick={() => openCreate(false)}>
              Cancel
            </Button>
          </div>
          {spec.kind === "combo" && (
            <div className="mt-2 flex flex-col gap-2">
              {stepRows.map((row) => (
                <div key={row.id} className="flex items-center gap-2">
                  {row.ghost ? (
                    <span className="w-[38px] flex-none font-mono text-[11px] text-accent">+ step</span>
                  ) : (
                    <span className="inline-flex size-[18px] flex-none items-center justify-center rounded-full bg-ink/[0.04] text-[9px] font-semibold text-ink-48">
                      {realSteps.indexOf(row) + 1}
                    </span>
                  )}
                  <div className={cn("min-w-0 flex-1", row.ghost && "opacity-50 focus-within:opacity-100 hover:opacity-100")}>
                    <SelectorPicker
                      value={row.value}
                      onChange={(v) => setRowValue(row.id, v)}
                      onInteract={() => promoteRow(row.id)}
                    />
                  </div>
                  {!row.ghost && (
                    <button
                      type="button"
                      aria-label="Remove step"
                      className="inline-flex size-5 items-center justify-center rounded-[5px] text-ink-ter hover:bg-err-tint hover:text-err"
                      onClick={() => removeRow(row.id)}
                    >
                      <X className="size-3" />
                    </button>
                  )}
                </div>
              ))}
            </div>
          )}
          <p className="mx-0.5 mt-1 text-xs leading-[1.4] text-ink-ter">{spec.hint}</p>
        </form>
      )}

      <div className="mt-3 rounded-[11px] border-[0.5px] border-line bg-canvas px-3.5 shadow-[0_1px_2px_rgba(29,29,31,0.05)]">
        {names.length === 0 ? (
          <div className="flex flex-wrap items-center justify-center gap-3 px-3 py-6 text-[12.5px] text-ink-ter">
            {tab === "combos"
              ? "No dynamic routes yet - chain providers for failover."
              : "No aliases yet - give a selector a short name."}
            <Button type="button" onClick={() => openCreate(true)}>
              <Plus className="size-3.5" />
              {spec.addLabel}
            </Button>
          </div>
        ) : (
          names.map((name) => {
            const combo = tab === "combos";
            const shadowed = combo && Object.prototype.hasOwnProperty.call(table.aliases, name);
            const preview = combo ? table.combos[name].join(" → ") : `→ ${table.aliases[name]}`;
            const isOpen = expanded?.name === name && expanded.kind === (combo ? "combo" : "alias");
            return (
              <div key={name} className="border-b-[0.5px] border-hairline last:border-b-0">
                <button
                  type="button"
                  role="listitem"
                  aria-expanded={isOpen}
                  title="Show route graph"
                  className="group flex w-full items-center gap-2.5 bg-transparent px-1 py-[11px] text-left"
                  onClick={() =>
                    setExpanded(isOpen ? null : { kind: combo ? "combo" : "alias", name })
                  }
                >
                  <span className="font-mono text-[12.5px] font-semibold text-ink group-hover:text-accent">
                    {name}
                  </span>
                  <Badge variant={combo ? "ok" : "accent"}>{spec.kind}</Badge>
                  {shadowed && (
                    <Badge variant="warn" title="An alias of the same name wins at resolve time">
                      shadowed
                    </Badge>
                  )}
                  <span className="min-w-0 flex-1 truncate whitespace-nowrap font-mono text-[11px] text-ink-ter">
                    {preview}
                  </span>
                  <ChevronDown
                    className={cn("size-4 text-ink-ter transition-transform", isOpen && "rotate-180")}
                  />
                </button>
                {isOpen && (
                  <div className="pb-3.5">
                    <RouteBoard
                      kind={combo ? "combo" : "alias"}
                      name={name}
                      table={table}
                      mutate={mutate}
                      reload={load}
                    />
                  </div>
                )}
              </div>
            );
          })
        )}
      </div>
      <Msg>{error && <span className="text-err">{error}</span>}</Msg>
    </section>
  );
}
