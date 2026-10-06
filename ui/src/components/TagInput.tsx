import { useRef, useState, type KeyboardEvent } from "react";
import { X } from "lucide-react";
import { cn } from "@/lib/utils";

// Tag input: chips + free input. Enter or comma commits the typed
// value; Backspace on an empty input drops the last chip.
export function TagInput({
  values,
  onChange,
  placeholder,
  listId,
  suggestions,
  autoFocus,
  "aria-label": ariaLabel,
}: {
  values: string[];
  onChange: (values: string[]) => void;
  placeholder?: string;
  listId?: string;
  suggestions?: string[];
  autoFocus?: boolean;
  "aria-label"?: string;
}) {
  const [pending, setPending] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const commit = (raw: string) => {
    const next = [...values];
    for (const part of raw.split(",")) {
      const v = part.trim();
      if (v && !next.includes(v)) next.push(v);
    }
    if (next.length !== values.length) onChange(next);
    setPending("");
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      commit(pending);
    } else if (e.key === "Backspace" && !pending && values.length) {
      onChange(values.slice(0, -1));
    }
  };

  return (
    <div
      className={cn(
        "flex flex-wrap items-center gap-1 bg-pearl border-[0.5px] border-line rounded-lg px-2 py-[5px] min-w-[220px] cursor-text focus-within:outline-2 focus-within:outline-focus",
      )}
      onClick={() => inputRef.current?.focus()}
    >
      {values.map((v) => (
        <span
          key={v}
          className="inline-flex items-center gap-1 font-mono text-[11px] text-ink bg-ink/[0.06] rounded-md pl-2 pr-1 py-0.5"
        >
          {v}
          <button
            type="button"
            aria-label={`Remove ${v}`}
            className="text-ink-ter hover:text-err rounded p-0.5"
            onClick={() => onChange(values.filter((x) => x !== v))}
          >
            <X className="size-3" />
          </button>
        </span>
      ))}
      <input
        ref={inputRef}
        className="appearance-none border-none font-mono text-xs text-ink bg-transparent px-0.5 py-1 flex-1 min-w-[120px] focus:outline-none placeholder:text-ink-ter"
        type="text"
        list={listId}
        autoComplete="off"
        placeholder={placeholder}
        aria-label={ariaLabel}
        autoFocus={autoFocus}
        value={pending}
        onChange={(e) => setPending(e.target.value)}
        onKeyDown={onKeyDown}
        onBlur={() => pending.trim() && commit(pending)}
      />
      {listId && suggestions && (
        <datalist id={listId}>
          {suggestions.map((s) => (
            <option key={s} value={s} />
          ))}
        </datalist>
      )}
    </div>
  );
}
