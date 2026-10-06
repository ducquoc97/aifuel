import { useMemo, useRef, useState, useEffect } from "react";
import { Select } from "@/components/ui/select";
import { Input } from "@/components/ui/input";
import { useModels, type ModelEntry } from "@/lib/models";
import { cn } from "@/lib/utils";

// Selector picker: integration -> model -> reasoning effort.
// The cascade splits a selector into the three parts a request
// selects independently. "custom selector..." keeps raw selectors
// (route names, planner chains, unpinned efforts) enterable where
// the cascade cannot express them.

export const PICKER_RAW = "__raw";

const AUTO_NOTES: Record<string, string> = {
  auto: "auto - the gateway plans a provider per request, so the target can change; the Logs tab shows what actually ran.",
  decide: "decide - like auto, but a decision model (TypeSafe Jev) picks which ranked provider leads; falls back to auto order if it is unreachable.",
};

export interface PickerValue {
  agent: string;
  model: string;
  effort: string;
  raw: boolean;
  rawText: string;
}

export function parseSelector(sel: string, integrations: string[]): PickerValue {
  sel = (sel || "").trim();
  if (!sel) return { agent: "", model: "", effort: "", raw: false, rawText: "" };
  const at = sel.lastIndexOf("@");
  const effort = at > 0 && at < sel.length - 1 ? sel.slice(at + 1) : "";
  const base = at > 0 ? sel.slice(0, at) : sel;
  const slash = base.indexOf("/");
  const head = slash === -1 ? base : base.slice(0, slash);
  const rest = slash === -1 ? "" : base.slice(slash + 1);
  if (integrations.includes(head)) {
    return { agent: head, model: rest, effort, raw: false, rawText: "" };
  }
  return { agent: PICKER_RAW, model: "", effort: "", raw: true, rawText: sel };
}

// `agent/model@effort`, dropping empty parts.
export function assembleSelector(v: PickerValue): string {
  if (v.raw) return v.rawText.trim();
  if (!v.agent) return "";
  const m = v.model.trim();
  return v.agent + (m ? "/" + m : "") + (v.effort ? "@" + v.effort : "");
}

// Select-style dropdown for the model field - only catalog models can
// be picked. While open the input becomes a filter query; the
// committed value changes only on click/Enter.
function ModelCombobox({
  value,
  onChange,
  suggestions,
  disabled,
  onInteract,
}: {
  value: string;
  onChange: (v: string) => void;
  suggestions: string[];
  disabled?: boolean;
  onInteract?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [highlight, setHighlight] = useState(0);
  const wrapRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return suggestions.slice(0, 300);
    const hit = suggestions.filter((s) => s.toLowerCase().includes(q));
    // Prefix matches first, substring hits after.
    hit.sort(
      (a, b) =>
        Number(b.toLowerCase().startsWith(q)) - Number(a.toLowerCase().startsWith(q)),
    );
    return hit.slice(0, 300);
  }, [query, suggestions]);

  const close = () => {
    setOpen(false);
    setQuery("");
  };

  useEffect(() => {
    const onDoc = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) close();
    };
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, []);

  useEffect(() => {
    if (open) setHighlight(0);
  }, [open, query]);

  const pick = (s: string) => {
    onChange(s);
    close();
  };

  return (
    <div ref={wrapRef} className="relative min-w-0">
      <Input
        className={cn("font-mono text-xs w-full", !open && "cursor-pointer caret-transparent")}
        placeholder="model (blank = default)"
        aria-label="Model"
        autoComplete="off"
        disabled={disabled}
        readOnly={!open}
        value={open ? query : value}
        onFocus={() => {
          setOpen(true);
          onInteract?.();
        }}
        onClick={() => {
          setOpen(true);
          onInteract?.();
        }}
        onChange={(e) => {
          setQuery(e.target.value);
          setOpen(true);
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setOpen(true);
            setHighlight((h) => Math.min(h + 1, filtered.length - 1));
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setHighlight((h) => Math.max(h - 1, 0));
          } else if (e.key === "Enter") {
            if (open && filtered[highlight]) {
              e.preventDefault();
              pick(filtered[highlight]);
            }
          } else if (e.key === "Escape") {
            close();
          }
        }}
      />
      {open && filtered.length > 0 && (
        <div
          ref={listRef}
          role="listbox"
          className="absolute z-30 mt-1 max-h-56 min-w-full w-max max-w-80 overflow-auto rounded-lg border-[0.5px] border-line bg-canvas py-1 shadow-[0_4px_16px_rgba(29,29,31,0.14)]"
        >
          {filtered.map((s, i) => (
            <button
              key={s}
              type="button"
              role="option"
              aria-selected={i === highlight}
              className={cn(
                "block w-full text-left px-2.5 py-1 font-mono text-[11.5px] text-ink-80 truncate",
                i === highlight ? "bg-accent-tint text-accent" : "hover:bg-ink/[0.04]",
              )}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => pick(s)}
              onMouseEnter={() => setHighlight(i)}
            >
              {s}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export function SelectorPicker({
  value,
  onChange,
  onInteract,
  stack,
  rawPlaceholder,
  disabled,
}: {
  value: PickerValue;
  onChange: (v: PickerValue) => void;
  onInteract?: () => void;
  stack?: boolean;
  rawPlaceholder?: string;
  disabled?: boolean;
}) {
  const { entries, ids, refresh } = useModels();
  useEffect(() => {
    refresh();
  }, [refresh]);

  const integrations = useMemo(() => ids.filter((id) => !id.includes("/")), [ids]);
  const entryMap = useMemo(() => {
    const m = new Map<string, ModelEntry>();
    for (const e of entries) if (e.id) m.set(e.id, e);
    return m;
  }, [entries]);

  const modelSuggestions = useMemo(() => {
    if (!value.agent || value.agent === PICKER_RAW) return [];
    const prefix = value.agent + "/";
    return entries
      .filter((e) => e.id && e.id.startsWith(prefix))
      .map((e) => e.id.slice(prefix.length));
  }, [entries, value.agent]);

  const modelEntry =
    value.model && value.agent !== PICKER_RAW
      ? entryMap.get(value.agent + "/" + value.model)
      : undefined;
  const efforts = Array.isArray(modelEntry?.reasoning) ? modelEntry.reasoning! : [];
  const effortDisabled = !efforts.length || disabled;
  const agentNote = AUTO_NOTES[value.agent];

  const set = (patch: Partial<PickerValue>) => {
    onInteract?.();
    onChange({ ...value, ...patch });
  };

  return (
    <div className={cn("flex items-center gap-1.5 flex-wrap min-w-0", stack && "flex-col items-stretch gap-1.5")}>
      <Select
        aria-label="Agent integration"
        className={cn("min-w-[130px] max-w-[180px]", stack && "max-w-none w-full text-[11.5px]")}
        value={value.agent || ""}
        disabled={disabled}
        onChange={(e) => {
          const agent = e.target.value;
          if (agent === PICKER_RAW) {
            set({ agent: PICKER_RAW, raw: true, model: "", effort: "", rawText: "" });
          } else {
            set({ agent, raw: false, rawText: "", model: "", effort: "" });
          }
        }}
      >
        <option value="" disabled>
          integration…
        </option>
        {integrations.map((id) => (
          <option key={id} value={id}>
            {id === "auto" ? "auto (gateway picks)" : id === "decide" ? "decide (decision model picks)" : id}
          </option>
        ))}
        <option value={PICKER_RAW}>custom selector…</option>
      </Select>

      {value.raw ? (
        <Input
          className={cn("font-mono text-xs flex-1 min-w-[240px]", stack && "min-w-0 w-full text-[11.5px]")}
          placeholder={rawPlaceholder || "selector - e.g. auto, cheap, codex/gpt-5@high"}
          aria-label="Raw selector"
          autoComplete="off"
          disabled={disabled}
          value={value.rawText}
          onChange={(e) => set({ rawText: e.target.value })}
        />
      ) : (
        <>
          <div className={cn("w-[190px]", stack && "w-full")}>
            <ModelCombobox
              value={value.model}
              onChange={(model) => set({ model, effort: "" })}
              suggestions={modelSuggestions}
              disabled={
                !value.agent ||
                disabled ||
                (!modelSuggestions.length && !value.model)
              }
              onInteract={onInteract}
            />
          </div>
          <Select
            aria-label="Reasoning effort"
            className={cn("min-w-[110px] max-w-[180px]", stack && "max-w-none w-full text-[11.5px]")}
            disabled={effortDisabled}
            title={
              efforts.length
                ? "Reasoning effort passed to the provider"
                : "No advertised reasoning levels - the provider default runs"
            }
            value={value.effort}
            onChange={(e) => set({ effort: e.target.value })}
          >
            <option value="">
              {modelEntry?.default_reasoning
                ? `default (${modelEntry.default_reasoning})`
                : "default"}
            </option>
            {efforts.map((v) => (
              <option key={v} value={v}>
                {v}
              </option>
            ))}
            {value.effort && !efforts.includes(value.effort) && (
              <option value={value.effort}>{value.effort}</option>
            )}
          </Select>
        </>
      )}

      {agentNote && (
        <div className={cn("basis-full text-[11px] leading-[1.4] text-ink-ter", stack && "basis-auto")}>
          {agentNote}
        </div>
      )}
    </div>
  );
}
