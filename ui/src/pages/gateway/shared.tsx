import { useCallback, useEffect, useState, type ReactNode } from "react";
import { RefreshCw } from "lucide-react";
import { useHeaderActions } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { toast } from "sonner";

// Shared topbar contract for every page: a status text plus a Refresh
// button pinned through the shell's header-actions context.
export function usePageHeader(onRefresh: () => Promise<unknown> | void) {
  const { setActions } = useHeaderActions();
  const [status, setStatus] = useState<ReactNode>("Loading...");
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      await onRefresh();
      setStatus("Updated " + new Date().toLocaleTimeString());
    } finally {
      setBusy(false);
    }
  }, [onRefresh]);

  useEffect(() => {
    setActions(
      <>
        <span className="text-xs text-ink-48 tracking-[-0.004em] tabular-nums whitespace-nowrap max-sm:hidden" aria-live="polite">
          {status}
        </span>
        <Button onClick={refresh} aria-label="Refresh this page" disabled={busy}>
          <RefreshCw className={busy ? "animate-spin" : undefined} />
          Refresh
        </Button>
      </>,
    );
    return () => setActions(null);
  }, [setActions, status, busy, refresh]);

  return { setStatus, refresh };
}

// navigator.clipboard with a textarea fallback; the button label flips
// so the user sees the copy actually landed.
export function CopyButton({ text, label = "Copy", className }: { text: string; label?: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    const done = () => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    };
    try {
      await navigator.clipboard.writeText(text);
      done();
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
        done();
      } catch {
        toast.error("Copy failed - select the text manually.");
      }
      ta.remove();
    }
  };
  return (
    <Button type="button" className={className} onClick={copy}>
      {copied ? "Copied" : label}
    </Button>
  );
}

export function Msg({ children }: { children: ReactNode }) {
  if (!children) return null;
  return (
    <div role="status" aria-live="polite" className="mt-3 text-[12.5px] text-ink-48">
      {children}
    </div>
  );
}
