import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type FormEvent,
  type DragEvent,
} from "react";
import { toast } from "sonner";
import { Check, Plus, Trash2, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Monogram } from "@/components/Monogram";
import { SelectorPicker, assembleSelector, parseSelector, type PickerValue } from "@/components/SelectorPicker";
import { useModels } from "@/lib/models";
import { cn } from "@/lib/utils";

// Route board: an expanded route rendered as a node-graph lane in the
// style of Cloudflare's AI Gateway "Dynamic Routing" diagram - a client
// node feeds the route node, combo lanes chain provider steps in
// attempt order ("primary" then dashed "fallback" edges) ending in a
// dashed add-step node carrying the selector picker; alias lanes hop
// once to a click-to-edit target on a "rewrite" edge. Edges are bezier
// connectors drawn in an SVG overlay after layout.

export interface RouteTable {
  aliases: Record<string, string>;
  combos: Record<string, string[]>;
}

// Splits a selector into head/model/effort for node rendering.
function selectorParts(sel: string) {
  const at = sel.lastIndexOf("@");
  const effort = at > 0 && at < sel.length - 1 ? sel.slice(at + 1) : "";
  const base = at > 0 ? sel.slice(0, at) : sel;
  const slash = base.indexOf("/");
  return {
    head: slash === -1 ? base : base.slice(0, slash),
    sub: slash === -1 ? "" : base.slice(slash + 1),
    effort,
    base,
  };
}

function SelectorMeta({ sel }: { sel: string }) {
  const p = selectorParts(sel);
  return (
    <>
      <Monogram name={p.base} />
      <span className="flex min-w-0 flex-col">
        <span className="max-w-[210px] truncate whitespace-nowrap text-[12.5px] font-semibold tracking-[-0.004em] text-ink">
          {p.head}
        </span>
        {p.sub && (
          <span className="mt-0.5 whitespace-nowrap font-mono text-[10.5px] text-ink-ter">{p.sub}</span>
        )}
        {p.effort && (
          <span className="mt-0.5 whitespace-nowrap font-mono text-[10.5px] text-accent">@{p.effort}</span>
        )}
      </span>
    </>
  );
}

function NodeX({
  onClick,
  title,
  label,
  className,
  icon,
}: {
  onClick?: (e: React.MouseEvent) => void;
  title?: string;
  label: string;
  className?: string;
  icon?: React.ReactNode;
}) {
  return (
    <button
      type={onClick ? "button" : "submit"}
      className={cn(
        "node-x inline-flex size-5 flex-none items-center justify-center rounded-[5px] text-ink-ter transition-colors hover:bg-err-tint hover:text-err",
        className,
      )}
      title={title}
      aria-label={label}
      onClick={onClick}
    >
      {icon || <X className="size-3" />}
    </button>
  );
}

function RouteNode({
  name,
  kind,
  shadowed,
  onDelete,
}: {
  name: string;
  kind: "combo" | "alias";
  shadowed?: boolean;
  onDelete: () => void;
}) {
  return (
    <div
      className="gw-node gw-node-src gw-node-dst min-w-[168px]"
      role="listitem"
      title={`Requests for model '${name}' resolve through this route`}
    >
      <div className="mb-[7px] flex items-center gap-1.5">
        <Badge variant={kind === "combo" ? "ok" : "accent"}>{kind}</Badge>
        {shadowed && (
          <Badge variant="warn" title="An alias of the same name wins at resolve time">
            shadowed
          </Badge>
        )}
        <NodeX
          className="ml-auto"
          title={`Delete ${kind}`}
          label={`Delete ${kind} ${name}`}
          icon={<Trash2 className="size-3" />}
          onClick={onDelete}
        />
      </div>
      <div className="whitespace-nowrap font-mono text-[12.5px] font-semibold tracking-[-0.004em] text-ink">
        model: {name}
      </div>
    </div>
  );
}

// ---- Edge layer ----

interface Ports { sx: number; sy: number; tx: number; ty: number }

function ports(el: Element, plane: HTMLElement, planeRect: DOMRect): Ports {
  const r = el.getBoundingClientRect();
  const y = r.top - planeRect.top + plane.scrollTop + r.height / 2;
  return {
    sx: r.right - planeRect.left + plane.scrollLeft,
    sy: y,
    tx: r.left - planeRect.left + plane.scrollLeft,
    ty: y,
  };
}

interface Edge { d: string; cls: string; label?: string; lx?: number; ly?: number }

function computeEdges(plane: HTMLElement): { w: number; h: number; edges: Edge[] } {
  const w = Math.max(plane.scrollWidth, plane.clientWidth);
  const h = Math.max(plane.scrollHeight, plane.clientHeight);
  const edges: Edge[] = [];
  const planeRect = plane.getBoundingClientRect();
  const app = plane.querySelector(".gw-node-app");
  if (!app) return { w, h, edges };

  const segment = (a: Ports, b: Ports, cls: string, label?: string) => {
    const c = Math.max(30, (b.tx - a.sx) * 0.45);
    edges.push({
      d: `M ${a.sx} ${a.sy} C ${a.sx + c} ${a.sy} ${b.tx - c} ${b.ty} ${b.tx} ${b.ty}`,
      cls,
      label,
      lx: (a.sx + b.tx) / 2,
      ly: (a.sy + b.ty) / 2 - 6,
    });
  };

  for (const lane of plane.querySelectorAll<HTMLElement>(".gw-lane")) {
    const nodes = [...lane.querySelectorAll<HTMLElement>(":scope > .gw-node")];
    if (!nodes.length) continue;
    segment(ports(app, plane, planeRect), ports(nodes[0], plane, planeRect), "gw-edge gw-edge-app");
    for (let i = 0; i + 1 < nodes.length; i++) {
      const last = i === nodes.length - 2;
      const a = ports(nodes[i], plane, planeRect);
      const b = ports(nodes[i + 1], plane, planeRect);
      if (lane.dataset.kind === "alias") {
        segment(a, b, "gw-edge gw-edge-primary", "rewrite");
      } else if (last) {
        segment(a, b, "gw-edge gw-edge-ghost");
      } else {
        segment(a, b, i === 0 ? "gw-edge gw-edge-primary" : "gw-edge gw-edge-alt",
          i === 0 ? "primary" : "fallback");
      }
    }
  }
  return { w, h, edges };
}

// ---- Board ----

export default function RouteBoard({
  kind,
  name,
  table,
  mutate,
  reload,
}: {
  kind: "combo" | "alias";
  name: string;
  table: RouteTable;
  // Every edit commits through the whole-table PUT.
  mutate: (fn: (t: RouteTable) => void) => Promise<void>;
  reload: () => void;
}) {
  const planeRef = useRef<HTMLDivElement>(null);
  const [edges, setEdges] = useState<{ w: number; h: number; edges: Edge[] }>({ w: 0, h: 0, edges: [] });
  const { ids } = useModels();
  const integrations = ids.filter((id) => !id.includes("/"));

  // Alias target editing state.
  const [editingTarget, setEditingTarget] = useState(false);
  const [targetValue, setTargetValue] = useState<PickerValue>({
    agent: "", model: "", effort: "", raw: false, rawText: "",
  });
  const settledRef = useRef(false);

  // Add-step node picker.
  const [addValue, setAddValue] = useState<PickerValue>({
    agent: "", model: "", effort: "", raw: false, rawText: "",
  });
  const [addBusy, setAddBusy] = useState(false);

  // Drag state for step reorder.
  const dragIndex = useRef<number | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
  const [dragging, setDragging] = useState<number | null>(null);

  const redraw = useCallback(() => {
    const plane = planeRef.current;
    if (!plane || !plane.isConnected) return;
    setEdges(computeEdges(plane));
  }, []);

  useLayoutEffect(() => {
    redraw();
  }, [redraw, table, editingTarget]);

  useEffect(() => {
    const plane = planeRef.current;
    if (!plane || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => redraw());
    ro.observe(plane);
    if (document.fonts?.ready) document.fonts.ready.then(redraw);
    return () => ro.disconnect();
  }, [redraw]);

  const steps = table.combos[name] || [];
  const target = table.aliases[name] || "";
  const shadowed = kind === "combo" && Object.prototype.hasOwnProperty.call(table.aliases, name);

  const deleteRoute = async () => {
    if (!window.confirm(`Delete ${kind} "${name}"?`)) return;
    try {
      await mutate((t) => {
        delete t[kind === "alias" ? "aliases" : "combos"][name];
      });
      toast.success(`Deleted ${kind} ${name}.`);
      reload();
    } catch (e) {
      toast.error(`Delete failed: ${(e as Error).message}`);
    }
  };

  const addStep = async (e: FormEvent) => {
    e.preventDefault();
    const value = assembleSelector(addValue);
    if (!value) return;
    setAddBusy(true);
    try {
      await mutate((t) => {
        (t.combos[name] = t.combos[name] || []).push(value);
      });
      toast.success(`Added ${value} to ${name}.`);
      setAddValue({ agent: "", model: "", effort: "", raw: false, rawText: "" });
      reload();
    } catch (e2) {
      toast.error(`Add failed: ${(e2 as Error).message}`);
    } finally {
      setAddBusy(false);
    }
  };

  const removeStep = async (i: number) => {
    const last = steps.length <= 1;
    // The API requires at least one selector per combo, so removing the
    // last step deletes the whole combo - confirm before doing it.
    if (last && !window.confirm(`Removing the last step deletes combo "${name}". Continue?`)) return;
    try {
      await mutate((t) => {
        const s = t.combos[name] || [];
        if (s.length <= 1) delete t.combos[name];
        else s.splice(i, 1);
      });
      toast.success(last ? `Deleted combo ${name}.` : `Removed a step from ${name}.`);
      reload();
    } catch (e) {
      toast.error(`Update failed: ${(e as Error).message}`);
    }
  };

  const reorder = async (from: number, to: number) => {
    try {
      await mutate((t) => {
        const s = t.combos[name] || [];
        const [moved] = s.splice(from, 1);
        if (moved !== undefined) s.splice(to, 0, moved);
      });
      toast.success("Reordered the failover chain.");
      reload();
    } catch (e) {
      toast.error(`Reorder failed: ${(e as Error).message}`);
    }
  };

  const commitTarget = async () => {
    if (settledRef.current) return;
    settledRef.current = true;
    const value = assembleSelector(targetValue);
    if (value && value !== target) {
      try {
        await mutate((t) => {
          t.aliases[name] = value;
        });
        toast.success(`Alias ${name} now rewrites to ${value}.`);
      } catch (e) {
        toast.error(`Update failed: ${(e as Error).message}`);
      }
    }
    setEditingTarget(false);
    reload();
  };

  const onStepDragStart = (e: DragEvent, i: number) => {
    dragIndex.current = i;
    setDragging(i);
    e.dataTransfer.effectAllowed = "move";
  };
  const onStepDragEnd = () => {
    dragIndex.current = null;
    setDragging(null);
    setDropIndex(null);
  };
  const onStepDragOver = (e: DragEvent, i: number) => {
    e.preventDefault();
    if (dragIndex.current !== null && dragIndex.current !== i) setDropIndex(i);
  };
  const onStepDrop = (e: DragEvent, i: number) => {
    e.preventDefault();
    const from = dragIndex.current;
    if (from !== null && from !== i) reorder(from, i);
  };

  return (
    <div>
      <div className="gw-board mt-1">
        <i className="gw-tick tl" aria-hidden="true" />
        <i className="gw-tick tr" aria-hidden="true" />
        <i className="gw-tick bl" aria-hidden="true" />
        <i className="gw-tick br" aria-hidden="true" />
        <div className="gw-plane" ref={planeRef}>
          <svg className="gw-edges" aria-hidden="true" width={edges.w} height={edges.h} viewBox={`0 0 ${edges.w} ${edges.h}`}>
            {edges.edges.map((e, i) => (
              <g key={i}>
                <path d={e.d} className={e.cls} />
                {e.label && (
                  <text className="gw-edge-label" x={e.lx} y={e.ly} textAnchor="middle">
                    {e.label}
                  </text>
                )}
              </g>
            ))}
          </svg>

          <div className="gw-node gw-node-src sticky left-0 z-[2] min-w-[150px] self-center gw-node-app">
            <div className="mb-[7px] flex items-center gap-1.5">
              <Badge>client</Badge>
            </div>
            <div className="whitespace-nowrap text-[12.5px] font-semibold tracking-[-0.004em] text-ink">
              Your app
            </div>
            <div className="mt-0.5 whitespace-nowrap font-mono text-[10.5px] text-ink-ter">
              openai / anthropic
            </div>
          </div>

          <div className="relative z-[1] flex min-w-0 flex-col gap-[22px]">
            <div
              className="gw-lane flex w-max min-w-full items-center gap-[72px]"
              data-kind={kind}
              role="list"
              aria-label={kind === "combo" ? `Failover order for ${name}` : `Rewrite target for ${name}`}
            >
              <RouteNode name={name} kind={kind} shadowed={shadowed} onDelete={deleteRoute} />

              {kind === "combo" &&
                steps.map((s, i) => (
                  <div
                    key={`${i}-${s}`}
                    role="listitem"
                    draggable
                    data-i={i}
                    title={`Attempt ${i + 1} - drag to reorder`}
                    className={cn(
                      "gw-node gw-node-src gw-node-dst gw-node-step flex items-center gap-2.5 px-[13px] py-[9px]",
                      i === 0 && "gw-node-first",
                      dragging === i && "gw-drag",
                      dropIndex === i && "gw-drop",
                    )}
                    onDragStart={(e) => onStepDragStart(e, i)}
                    onDragEnd={onStepDragEnd}
                    onDragOver={(e) => onStepDragOver(e, i)}
                    onDragLeave={() => setDropIndex((cur) => (cur === i ? null : cur))}
                    onDrop={(e) => onStepDrop(e, i)}
                  >
                    <span className="inline-flex size-4 flex-none items-center justify-center rounded-full bg-ink/[0.08] text-[10px] font-semibold text-ink-48">
                      {i + 1}
                    </span>
                    <SelectorMeta sel={s} />
                    <NodeX
                      title={`Remove ${s}`}
                      label={`Remove ${s}`}
                      onClick={() => removeStep(i)}
                    />
                  </div>
                ))}

              {kind === "combo" && (
                <div className="gw-node gw-node-dst gw-node-add">
                  <form className="flex flex-col items-stretch gap-[7px]" onSubmit={addStep}>
                    <span className="font-mono text-[11px] text-ink-ter">+ fallback</span>
                    <SelectorPicker
                      stack
                      value={addValue}
                      onChange={setAddValue}
                      rawPlaceholder="selector"
                    />
                    <NodeX
                      title="Add step"
                      label="Add step"
                      className={cn("self-end text-accent hover:bg-accent-tint hover:text-accent", addBusy && "opacity-50")}
                      icon={<Plus className="size-3.5" />}
                    />
                  </form>
                </div>
              )}

              {kind === "alias" &&
                (editingTarget ? (
                  <div
                    className="gw-node gw-node-dst cursor-default"
                    role="listitem"
                    onKeyDown={(e) => {
                      if (e.key === "Escape") {
                        settledRef.current = true;
                        setEditingTarget(false);
                      }
                      if (e.key === "Enter" && (e.target as HTMLElement).tagName !== "SELECT") {
                        e.preventDefault();
                        commitTarget();
                      }
                    }}
                  >
                    <SelectorPicker
                      stack
                      value={targetValue}
                      onChange={setTargetValue}
                      rawPlaceholder="selector"
                    />
                    <div className="mt-[7px] flex justify-end gap-1.5">
                      <button
                        type="button"
                        title="Save target"
                        aria-label="Save target"
                        className="inline-flex size-5 items-center justify-center rounded-[5px] text-ink-ter hover:bg-accent-tint hover:text-accent"
                        onClick={(e) => {
                          e.stopPropagation();
                          commitTarget();
                        }}
                      >
                        <Check className="size-3.5" />
                      </button>
                      <button
                        type="button"
                        title="Cancel"
                        aria-label="Cancel edit"
                        className="inline-flex size-5 items-center justify-center rounded-[5px] text-ink-ter hover:bg-err-tint hover:text-err"
                        onClick={(e) => {
                          e.stopPropagation();
                          settledRef.current = true;
                          setEditingTarget(false);
                        }}
                      >
                        <X className="size-3" />
                      </button>
                    </div>
                  </div>
                ) : (
                  <div
                    className="gw-node gw-node-dst flex cursor-pointer items-center gap-2.5 px-[13px] py-[9px]"
                    role="listitem"
                    title="Click to edit the rewrite target"
                    onClick={() => {
                      settledRef.current = false;
                      setTargetValue(parseSelector(target, integrations));
                      setEditingTarget(true);
                    }}
                  >
                    <SelectorMeta sel={target} />
                  </div>
                ))}
            </div>
          </div>
        </div>
      </div>
      <p className="mx-0.5 mt-1.5 text-xs leading-[1.4] text-ink-ter">
        {kind === "combo"
          ? "Drag steps to reorder, × removes a step, the dashed node appends a fallback."
          : "Click the target node to edit the selector this alias rewrites to."}
      </p>
    </div>
  );
}
