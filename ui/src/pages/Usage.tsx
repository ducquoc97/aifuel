import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw, BarChart3 } from "lucide-react";
import { useHeaderActions } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Monogram } from "@/components/Monogram";
import { fmtCountdown, fmtDate } from "@/lib/format";
import { cn } from "@/lib/utils";

// Usage page: streams /api/usage/stream line-by-line and paints provider
// quota cards as they arrive - auto-refresh, countdown tick, and
// skeleton states mirror the old usage.js behavior.

interface QuotaWindow {
  label: string;
  period: string;
  remaining_percent: number | null;
  used_percent: number | null;
  resets_at?: number | null;
}

interface ResetCredit {
  title?: string;
  reset_type?: string;
  expires_at?: number;
}

interface ProviderResult {
  key: string;
  name: string;
  plan?: string;
  source?: string;
  detail?: string;
  windows?: QuotaWindow[];
  reset_credits?: { available_count?: number; credits?: ResetCredit[] };
  reset_at?: number | null;
}

const PERIOD_LABEL: Record<string, string> = {
  "5h": "5-hour", daily: "daily", weekly: "weekly", monthly: "monthly", unknown: "window",
};
const PERIOD_ORDER = ["monthly", "weekly", "daily", "5h", "unknown"];
const PERIOD_RANK: Record<string, number> = { monthly: 0, weekly: 1, daily: 2, "5h": 3, unknown: 4 };

const PROVIDER_NAME: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex CLI",
  copilot: "GitHub Copilot",
  antigravity: "Antigravity CLI",
};

const WINDOW_LIMIT = 6;
const MODEL_PREVIEW = 3;
const MODEL_PROVIDERS = new Set(["antigravity"]);

function anchorPeriod(windows: QuotaWindow[]) {
  return windows
    .slice()
    .sort((a, b) => PERIOD_ORDER.indexOf(a.period) - PERIOD_ORDER.indexOf(b.period))[0]
    ?.period;
}

// Mirror of the server's provider ranking so cards stay correctly
// ordered as they stream in. (aifuel.collect)
function effectiveRemaining(res: ProviderResult) {
  const ws = (res.windows || [])
    .slice()
    .sort((a, b) => (PERIOD_RANK[a.period] ?? 99) - (PERIOD_RANK[b.period] ?? 99));
  for (const w of ws) {
    if (w.remaining_percent !== null && w.remaining_percent !== undefined)
      return w.remaining_percent;
  }
  return -1;
}

function sortProviders(list: ProviderResult[]) {
  return list.slice().sort((a, b) => {
    const ae = effectiveRemaining(a) <= 0 ? 1 : 0;
    const be = effectiveRemaining(b) <= 0 ? 1 : 0;
    if (ae !== be) return ae - be;
    const ar = a.reset_at ?? Infinity;
    const br = b.reset_at ?? Infinity;
    return ar - br;
  });
}

function barColor(rem: number | null) {
  if (rem === null) return "var(--ink-qua)";
  if (rem <= 10) return "var(--err)";
  if (rem <= 30) return "var(--warn)";
  return "var(--ok)";
}

function modelFamily(label: string) {
  return String(label).trim().split(/[-_/.]/)[0].toLowerCase();
}

function familyPreview(models: QuotaWindow[], cap: number) {
  const seen = new Set<string>();
  const out: QuotaWindow[] = [];
  for (const m of models) {
    if (out.length >= cap) break;
    const f = modelFamily(m.label);
    if (seen.has(f)) continue;
    seen.add(f);
    out.push(m);
  }
  for (const m of models) {
    if (out.length >= cap) break;
    if (!out.includes(m)) out.push(m);
  }
  return out;
}

function ResetCredits({ rc }: { rc: NonNullable<ProviderResult["reset_credits"]> }) {
  const available = Number(rc?.available_count);
  if (!Number.isInteger(available) || available < 0) return null;
  const noun = available === 1 ? "usage limit reset" : "usage limit resets";
  const credits = (rc.credits || []).filter((c) => c && typeof c === "object");
  return (
    <section aria-label="Codex usage limit resets" className="mt-2.5 rounded-lg bg-accent-tint px-3 py-2.5">
      <div className="text-[12.5px] font-semibold tracking-[-0.004em] text-accent">Redeem usage limit reset</div>
      <div className="mt-1 text-[12.5px] leading-[1.4] tracking-[-0.004em] text-ink-48">
        You have <b>{available} {noun}</b> available.
      </div>
      {credits.map((c, i) => (
        <div key={i} className="mt-1.5 flex justify-between gap-2 text-xs text-ink-48 tabular-nums">
          <span>{c.title || c.reset_type || "Usage limit reset"}</span>
          {c.expires_at ? <span className="text-ink-ter">Expires {fmtDate(c.expires_at)}</span> : null}
        </div>
      ))}
    </section>
  );
}

function WindowRow({ w, isAnchor }: { w: QuotaWindow; isAnchor: boolean }) {
  const rem = w.remaining_percent ?? null;
  const used = w.used_percent ?? null;
  const width = rem === null ? 100 : Math.max(2, Math.min(100, rem));
  const col = barColor(rem);
  const [countdown, setCountdown] = useState("");
  useEffect(() => {
    const update = () => {
      const r = w.resets_at;
      setCountdown(r ? "renews in " + fmtCountdown(r - Date.now() / 1000) : "no reset clock");
    };
    update();
    const t = setInterval(update, 1000);
    return () => clearInterval(t);
  }, [w.resets_at]);

  return (
    <div className="mt-3.5 first:mt-0">
      <div className="flex items-baseline justify-between gap-2">
        <span className="flex min-w-0 items-center gap-1.5 text-[13px] font-medium tracking-[-0.006em] text-ink">
          <span className="truncate">{w.label}</span>
          <span
            className={cn(
              "flex-none rounded-full bg-ink/[0.04] px-[7px] py-0.5 text-[11px] font-medium tracking-[0.004em] text-ink-ter",
              isAnchor && "bg-accent-tint text-accent",
            )}
          >
            {PERIOD_LABEL[w.period] || w.period}
          </span>
        </span>
        <span className="flex-none text-[13px] font-semibold tabular-nums" style={{ color: col }}>
          {rem === null ? "n/a" : `${rem.toFixed(0)}% left`}
        </span>
      </div>
      <div
        className="mt-2 h-1.5 overflow-hidden rounded-full bg-ink/[0.06]"
        role="progressbar"
        aria-valuenow={rem ?? 0}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={`${w.label} remaining`}
      >
        <div
          className="h-full rounded-full transition-[width] duration-500"
          style={{ width: `${width}%`, background: col, opacity: rem === null ? 0.35 : 1 }}
        />
      </div>
      <div className="mt-[7px] flex justify-between gap-2 text-[11.5px] text-ink-ter tabular-nums">
        <span className="text-ink-48">{countdown}</span>
        <span>
          {used !== null && rem !== null ? `${used.toFixed(0)}% used · ` : ""}
          {fmtDate(w.resets_at)}
        </span>
      </div>
    </div>
  );
}

function ProviderCard({ p, rank }: { p: ProviderResult; rank: number }) {
  const [expanded, setExpanded] = useState(false);
  const urgent = rank === 0;
  const allW = p.windows || [];
  const isModels = MODEL_PROVIDERS.has(p.key);
  const noun = isModels ? "models" : "windows";
  const preview = isModels ? familyPreview(allW, MODEL_PREVIEW) : allW.slice(0, WINDOW_LIMIT);
  const shown = allW.length > preview.length ? (expanded ? allW : preview) : allW;
  const disclosure =
    allW.length > preview.length ? (expanded ? "Show less" : `Show all ${allW.length} ${noun}`) : null;
  const anchor = anchorPeriod(allW);

  return (
    <div
      role="listitem"
      className={cn(
        "relative overflow-hidden rounded-[18px] border-[0.5px] border-line bg-canvas px-5 py-4.5 shadow-[0_1px_2px_rgba(29,29,31,0.05)] transition-all duration-200 hover:-translate-y-0.5 hover:shadow-[0_4px_14px_rgba(29,29,31,0.10),0_0_0_0.5px_rgba(29,29,31,0.04)]",
      )}
    >
      {urgent && (
        <div className="absolute inset-x-0 top-0 h-0.5 bg-gradient-to-r from-warn to-transparent" />
      )}
      <div
        aria-label={urgent ? "resets soonest" : `#${rank + 1}`}
        className={cn(
          "absolute right-3.5 top-3.5 rounded-full px-2 py-[3px] text-[11px] font-semibold tracking-[0.004em] tabular-nums",
          urgent ? "bg-warn-tint text-warn" : "bg-ink/[0.06] text-ink-ter",
        )}
      >
        {urgent ? "resets soonest" : `#${rank + 1}`}
      </div>
      <div className="flex items-center gap-2.5 pr-[72px] text-[15px] font-semibold tracking-[-0.011em]">
        <Monogram name={p.key} />
        {p.name}
      </div>
      <div className="mt-2.5 flex flex-wrap gap-1.5">
        {p.plan && <Badge>{p.plan.charAt(0).toUpperCase() + p.plan.slice(1)}</Badge>}
        <Badge variant={p.source === "live" ? "ok" : "err"}>
          {p.source === "live" ? "Live" : "Error"}
        </Badge>
      </div>
      {p.detail && allW.length > 0 && (
        <div className="mt-2.5 text-[13px] leading-[1.4] tracking-[-0.004em] text-ink-48">{p.detail}</div>
      )}
      {p.key === "codex" && p.reset_credits && <ResetCredits rc={p.reset_credits} />}
      {allW.length > 0 && <hr className="my-3 h-px border-none bg-hairline" />}
      {allW.length ? (
        shown.map((w, i) => <WindowRow key={i} w={w} isAnchor={w.period === anchor} />)
      ) : (
        <div role="status" className="mt-2.5 px-0 py-5 text-center text-[13px] text-ink-ter">
          <BarChart3 className="mx-auto mb-1.5 size-6 opacity-55" />
          {p.detail || "No data available"}
        </div>
      )}
      {disclosure && (
        <button
          className="mt-3.5 flex w-full items-center justify-center gap-1.5 rounded-lg bg-ink/[0.04] py-2.5 text-[12.5px] font-medium tracking-[-0.004em] text-accent transition-colors hover:bg-ink/[0.06] active:scale-[0.99]"
          onClick={() => setExpanded((e) => !e)}
          aria-expanded={expanded}
        >
          <span className={cn("text-sm leading-none transition-transform", expanded && "rotate-90")}>›</span>
          {disclosure}
        </button>
      )}
    </div>
  );
}

function SkeletonCard({ name }: { name: string }) {
  return (
    <div role="listitem" aria-busy="true" className="pointer-events-none rounded-[18px] border-[0.5px] border-line bg-canvas px-5 py-4.5">
      <div className="flex items-center gap-2.5 text-[15px] font-semibold">
        <Monogram name={name} />
        <span>{PROVIDER_NAME[name] || name.charAt(0).toUpperCase() + name.slice(1)}</span>
        <span className="ml-auto size-[15px] animate-spin rounded-full border-2 border-ink/[0.08] border-t-accent" role="status" aria-label="Loading" />
      </div>
      <div className="skel mt-2.5 h-5 w-[72px] rounded-full" />
      <hr className="my-3 h-px border-none bg-hairline" />
      <div className="skel mt-3.5 h-3 rounded-md" />
      <div className="skel mt-3.5 h-3 rounded-md" />
      <div className="skel mt-3.5 h-3 w-[55%] rounded-md" />
    </div>
  );
}

export default function Usage() {
  const { setActions } = useHeaderActions();
  const [received, setReceived] = useState<Map<string, ProviderResult>>(new Map());
  const [expected, setExpected] = useState<string[] | null>(null);
  const [status, setStatus] = useState<{ text: string; err?: boolean; title?: string }>({
    text: "Loading...",
  });
  const [loading, setLoading] = useState(true);
  const [autoRefresh, setAutoRefresh] = useState(
    () => localStorage.getItem("autoRefresh") === "1",
  );
  const abortRef = useRef<AbortController | null>(null);
  const expectedRef = useRef<string[] | null>(null);
  expectedRef.current = expected;

  const load = useCallback(async (force: boolean, initial: boolean) => {
    abortRef.current?.abort();
    const ac = new AbortController();
    abortRef.current = ac;
    setLoading(true);
    if (initial) {
      setReceived(new Map());
      setExpected(null);
      setStatus({ text: "Loading..." });
    }
    const next = new Map<string, ProviderResult>();
    let discoveryErrors: { provider?: { name?: string; key?: string } | string; detail?: string }[] = [];
    try {
      const r = await fetch("/api/usage/stream" + (force ? "?force=1" : ""), {
        signal: ac.signal,
      });
      if (!r.ok || !r.body) throw new Error("HTTP " + r.status);
      const reader = r.body.getReader();
      const dec = new TextDecoder();
      let buf = "";
      let got = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        let nl: number;
        while ((nl = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, nl).trim();
          buf = buf.slice(nl + 1);
          if (!line) continue;
          const msg = JSON.parse(line);
          if (msg.providers_expected) {
            discoveryErrors = msg.discovery_errors || [];
            setExpected(msg.providers_expected);
            for (const key of next.keys()) {
              if (!msg.providers_expected.includes(key)) next.delete(key);
            }
            setReceived(new Map(next));
          } else if (msg.provider) {
            next.set(msg.provider.key, msg.provider);
            setReceived(new Map(next));
            got++;
            setStatus({ text: `Loading ${got}/${(msg.providers_expected ?? expectedRef.current ?? []).length || got}...` });
          } else if (msg.done) {
            if (discoveryErrors.length) {
              const names = discoveryErrors
                .map((e) => (typeof e.provider === "object" ? e.provider?.name || e.provider?.key : e.provider))
                .join(", ");
              setStatus({
                text: `Discovery failed: ${names}`,
                err: true,
                title: discoveryErrors
                  .map((e) => {
                    const p = typeof e.provider === "object" ? e.provider?.name || e.provider?.key : e.provider;
                    return `${p}: ${e.detail}`;
                  })
                  .join("\n"),
              });
            } else {
              setStatus({ text: "Updated " + new Date(msg.generated_at * 1000).toLocaleTimeString() });
            }
          }
        }
      }
    } catch (e) {
      if ((e as Error).name === "AbortError") return;
      setStatus({ text: "fetch failed: " + String(e), err: true });
    } finally {
      if (abortRef.current === ac) {
        abortRef.current = null;
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    load(false, true);
    return () => abortRef.current?.abort();
  }, [load]);

  useEffect(() => {
    if (!autoRefresh) return;
    const t = setInterval(() => load(false, false), 300000);
    return () => clearInterval(t);
  }, [autoRefresh, load]);

  useEffect(() => {
    setActions(
      <>
        <span
          className="text-xs text-ink-48 tracking-[-0.004em] tabular-nums whitespace-nowrap max-sm:hidden"
          aria-live="polite"
          title={status.title}
        >
          {status.err ? <span className="text-err">{status.text}</span> : status.text}
        </span>
        <label className="inline-flex items-center gap-[7px] text-[12.5px] font-medium text-ink cursor-pointer select-none whitespace-nowrap">
          <Switch
            aria-label="Auto-refresh"
            checked={autoRefresh}
            onCheckedChange={(v) => {
              setAutoRefresh(v);
              localStorage.setItem("autoRefresh", v ? "1" : "0");
            }}
          />
          Auto-refresh
        </label>
        <Button onClick={() => load(true, false)} aria-label="Refresh data now">
          <RefreshCw className={cn(loading && "animate-spin")} />
          Refresh
        </Button>
      </>,
    );
    return () => setActions(null);
  }, [setActions, status, autoRefresh, loading, load]);

  const arrived = sortProviders([...received.values()]);
  const pending = (expected || []).filter((k) => !received.has(k));

  return (
    <>
      <div role="list" aria-label="AI provider usage cards" className="grid grid-cols-[repeat(auto-fill,minmax(272px,1fr))] gap-3">
        {expected === null && (
          <div role="status" aria-busy="true" className="pointer-events-none col-span-full rounded-[18px] border-[0.5px] border-line bg-canvas px-5 py-4.5">
            <div className="flex items-center gap-2.5 font-semibold">
              <span>Finding configured providers</span>
              <span className="ml-auto size-[15px] animate-spin rounded-full border-2 border-ink/[0.08] border-t-accent" />
            </div>
            <div className="skel mt-2.5 h-5 w-[72px] rounded-full" />
            <hr className="my-3 h-px border-none bg-hairline" />
            <div className="skel mt-3.5 h-3 rounded-md" />
            <div className="skel mt-3.5 h-3 w-[55%] rounded-md" />
          </div>
        )}
        {expected !== null && expected.length === 0 && received.size === 0 && (
          <div role="listitem" className="col-span-full grid min-h-[220px] place-items-center rounded-[18px] border-[0.5px] border-line bg-canvas text-center">
            <div className="max-w-[360px]">
              <BarChart3 className="mx-auto mb-3.5 size-9 text-ink-ter" />
              <h2 className="m-0 mb-2 text-[15px] font-semibold tracking-[-0.011em]">No configured providers found</h2>
              <p className="m-0 text-[13px] leading-[1.45] text-ink-48">
                Sign in with an AI coding provider, then refresh this dashboard.
              </p>
            </div>
          </div>
        )}
        {arrived.map((p, i) => (
          <ProviderCard key={p.key} p={p} rank={i} />
        ))}
        {pending.map((k) => (
          <SkeletonCard key={k} name={k} />
        ))}
      </div>

      <footer className="px-0.5 pb-10 pt-5 text-xs leading-[1.45] tracking-[-0.004em] text-ink-ter">
        {autoRefresh && <span>Auto-refreshes every 5 minutes.</span>}
        <div className="mt-1.5 flex flex-wrap gap-x-4 gap-y-1.5" aria-label="Source legend">
          <span className="flex items-center gap-1.5">
            <span className="size-2 rounded-full" style={{ background: "var(--ok)" }} />
            <b className="text-ink-48">Live</b> - pulled from provider API
          </span>
          <span className="flex items-center gap-1.5">
            <span className="size-2 rounded-full" style={{ background: "var(--err)" }} />
            <b className="text-ink-48">Error</b> - live usage unavailable or unusable
          </span>
        </div>
      </footer>
    </>
  );
}
