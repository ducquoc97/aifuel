import { useCallback, useEffect, useRef, useState } from "react";
import { apiGet } from "@/lib/api";
import { fmtTime, relTime } from "@/lib/format";
import { Badge } from "@/components/ui/badge";
import { Card } from "@/components/ui/card";
import { Select } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Msg, usePageHeader } from "./shared";
import { cn } from "@/lib/utils";

// Request log: newest-first gateway requests, auto-refreshes every 10
// seconds while this page is foregrounded.

interface LogEntry {
  ts_unix?: number;
  model?: string;
  integration?: string;
  status?: number;
  stream?: boolean;
  usage?: { total_tokens?: number };
  error?: string | { message?: string };
}

const POLL_MS = 10000;

function logErrorText(entry: LogEntry): string {
  const err = entry && entry.error;
  if (!err) return "";
  return typeof err === "string" ? err : err.message || JSON.stringify(err);
}

// 2xx green, 4xx amber, 5xx+ red; anything else stays neutral gray.
function statusVariant(s?: number) {
  if (s === undefined) return "default";
  if (s >= 200 && s < 300) return "ok";
  if (s >= 400 && s < 500) return "warn";
  if (s >= 500) return "err";
  return "default";
}

export default function Logs() {
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [error, setError] = useState("");
  const [limit, setLimit] = useState(200);
  const limitRef = useRef(limit);
  limitRef.current = limit;

  const load = useCallback(async () => {
    try {
      const data = await apiGet<{ entries?: LogEntry[] }>(`/api/gateway/logs?limit=${limitRef.current}`);
      setEntries(data.entries || []);
      setError("");
    } catch (e) {
      setError(`Request log failed: ${(e as Error).message}`);
    }
  }, []);

  usePageHeader(load);
  useEffect(() => {
    load();
    const t = setInterval(() => {
      if (!document.hidden) load();
    }, POLL_MS);
    return () => clearInterval(t);
  }, [load]);

  return (
    <section aria-label="Request Log">
      <p className="m-0 max-w-[66ch] text-[13px] leading-[1.45] tracking-[-0.006em] text-ink-48">
        Latest gateway requests, newest first. Auto-refreshes every 10 seconds while this page is open.
      </p>
      <div className="mt-3.5">
        <label className="inline-flex items-center gap-1.5 text-[12.5px] font-medium text-ink">
          Show
          <Select value={limit} onChange={(e) => { setLimit(Number(e.target.value)); setTimeout(load, 0); }} aria-label="Log entries to show">
            <option value={50}>50</option>
            <option value={200}>200</option>
            <option value={500}>500</option>
          </Select>
          entries
        </label>
      </div>
      <Card className="mt-3.5 overflow-x-auto px-[18px] pb-2.5 pt-1.5">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Time</TableHead>
              <TableHead>Model</TableHead>
              <TableHead>Integration</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Stream</TableHead>
              <TableHead className="text-right">Tokens</TableHead>
              <TableHead>Error</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {entries.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="py-5 text-center text-ink-ter">
                  No requests logged yet.
                </TableCell>
              </TableRow>
            )}
            {entries.map((e, i) => {
              const errTxt = logErrorText(e);
              return (
                <TableRow key={i}>
                  <TableCell className="whitespace-nowrap font-mono text-xs" title={fmtTime(e.ts_unix)}>
                    {relTime(e.ts_unix)}
                  </TableCell>
                  <TableCell className="font-mono text-xs">{e.model}</TableCell>
                  <TableCell className="font-mono text-xs">
                    {e.integration || <span className="text-ink-ter">-</span>}
                  </TableCell>
                  <TableCell>
                    <span
                      className={cn(
                        "inline-block min-w-[34px] rounded-full px-2 py-[3px] text-center text-xs font-semibold tabular-nums",
                        statusVariant(e.status) === "ok" && "bg-ok-tint text-ok",
                        statusVariant(e.status) === "warn" && "bg-warn-tint text-warn",
                        statusVariant(e.status) === "err" && "bg-err-tint text-err",
                        statusVariant(e.status) === "default" && "bg-ink/[0.04] text-ink-48",
                      )}
                    >
                      {e.status ?? "-"}
                    </span>
                  </TableCell>
                  <TableCell>
                    {e.stream ? <Badge variant="info">stream</Badge> : <span className="text-ink-ter">-</span>}
                  </TableCell>
                  <TableCell className="whitespace-nowrap text-right tabular-nums">
                    {e.usage && e.usage.total_tokens != null ? (
                      Number(e.usage.total_tokens).toLocaleString()
                    ) : (
                      <span className="text-ink-ter">-</span>
                    )}
                  </TableCell>
                  <TableCell
                    className="max-w-[260px] truncate text-err"
                    title={errTxt || undefined}
                  >
                    {errTxt || <span className="text-ink-ter">-</span>}
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      </Card>
      <Msg>{error && <span className="text-err">{error}</span>}</Msg>
    </section>
  );
}
