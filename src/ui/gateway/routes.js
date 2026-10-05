// ---- Routes (aliases + combos in gateway.json) ----
// The section is a tabbed route list - "Dynamic" holds failover
// combos, "Aliases" holds name rewrites. Expanding a row reveals that
// route's graph in the style of Cloudflare's AI Gateway "Dynamic
// Routing" diagram: a client node feeds the route node, combo lanes
// chain provider steps in attempt order ("primary" then dashed
// "fallback" edges) ending in a dashed add-step node, alias lanes hop
// once to a click-to-edit target on a "rewrite" edge. Edges are
// bezier connectors drawn in an SVG overlay after layout; the add
// affordance per tab opens an inline create form. Every edit commits
// through the same whole-table PUT.

// The API replaces the whole table, so every mutation is: fetch the
// current table, apply the change client-side, PUT the merged object.
async function mutateRoutes(mutator) {
  const data = await apiGet("/api/gateway/routes");
  const table = { aliases: data.aliases || {}, combos: data.combos || {} };
  mutator(table);
  await apiPut("/api/gateway/routes", table);
}

// Last fetched route table plus list state: which tab is active and
// which row (if any) is expanded to its graph.
let routeTable = { aliases: {}, combos: {} };
let routeTab = "combos";
let expandedRoute = null; // { kind: "combo"|"alias", name }

async function loadRoutes() {
  ensureModelIds();
  try {
    const data = await apiGet("/api/gateway/routes");
    routeTable = { aliases: data.aliases || {}, combos: data.combos || {} };
    renderRouteList();
    setMsg("routes-msg", "");
    markUpdated();
  } catch (e) {
    document.getElementById("route-list").innerHTML = "";
    setMsg("routes-msg", `<span class="err">Routes failed to load: ${esc(e.message)}</span>`);
  }
}

// ---- Tabs and the create form ----

function setRouteTab(tab) {
  routeTab = tab;
  expandedRoute = null;
  document.getElementById("route-tab-combos").setAttribute("aria-selected", String(tab === "combos"));
  document.getElementById("route-tab-aliases").setAttribute("aria-selected", String(tab === "aliases"));
  toggleRouteCreate(false);
  renderRouteList();
}

// The create form's labels and parsing switch on the active tab:
// combos take comma-separated selectors, aliases take one target.
function routeFormSpec() {
  return routeTab === "combos"
    ? { kind: "combo", addLabel: "Add combo", namePh: "Route name - e.g. heavy",
        valuePh: "Selectors, comma-separated - e.g. codex, copilot, devin",
        hint: "Steps are tried in order; the chain falls through while a failure happens before provider execution." }
    : { kind: "alias", addLabel: "Add alias", namePh: "Alias name - e.g. cheap",
        valuePh: "Model selector - e.g. groq:api-key/llama-3.3-70b",
        hint: "Requests naming this alias are rewritten to the target selector before routing." };
}

function toggleRouteCreate(show) {
  const form = document.getElementById("route-create-form");
  const spec = routeFormSpec();
  const open = show === undefined ? form.hidden : show;
  form.hidden = !open;
  document.getElementById("route-add-label").textContent = spec.addLabel;
  if (open) {
    document.getElementById("route-create-name").placeholder = spec.namePh;
    document.getElementById("route-create-value").placeholder = spec.valuePh;
    document.getElementById("route-create-hint").textContent = spec.hint;
    document.getElementById("route-create-submit").textContent = spec.addLabel;
    form.elements.name.focus();
  }
}

async function addRoute(event) {
  event.preventDefault();
  const form = event.target;
  const name = form.elements.name.value.trim();
  const value = form.elements.value.value.trim();
  const combo = routeTab === "combos";
  const selectors = value.split(",").map(s => s.trim()).filter(Boolean);
  if (!name || !selectors.length) {
    setMsg("routes-msg", '<span class="err">Name and at least one selector are required.</span>');
    return false;
  }
  if (!combo && selectors.length !== 1) {
    setMsg("routes-msg", '<span class="err">An alias rewrites to exactly one selector.</span>');
    return false;
  }
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    await mutateRoutes(table => {
      if (combo) table.combos[name] = selectors;
      else table.aliases[name] = selectors[0];
    });
    form.reset();
    setMsg("routes-msg", "");
    toast(`Added ${combo ? "combo" : "alias"} ${name}.`);
    expandedRoute = { kind: combo ? "combo" : "alias", name };
    loadRoutes();
  } catch (e) {
    setMsg("routes-msg", `<span class="err">${esc(e.message)}</span>`);
  } finally {
    btn.disabled = false;
  }
  return false;
}

// ---- List render: one row per route in the active tab ----

function renderRouteList() {
  const list = document.getElementById("route-list");
  const combos = routeTable.combos;
  const aliases = routeTable.aliases;
  document.getElementById("route-tab-combos").textContent = `Dynamic · ${Object.keys(combos).length}`;
  document.getElementById("route-tab-aliases").textContent = `Aliases · ${Object.keys(aliases).length}`;
  const spec = routeFormSpec();
  document.getElementById("route-add-label").textContent = spec.addLabel;

  const names = Object.keys(routeTab === "combos" ? combos : aliases);
  list.innerHTML = "";
  if (!names.length) {
    const empty = document.createElement("div");
    empty.className = "gw-route-empty";
    empty.innerHTML = routeTab === "combos"
      ? 'No dynamic routes yet - chain providers for failover.'
      : 'No aliases yet - give a selector a short name.';
    const btn = document.createElement("button");
    btn.className = "btn";
    btn.type = "button";
    btn.innerHTML = '<span class="ms ms-add" aria-hidden="true"></span>' + esc(spec.addLabel);
    btn.onclick = () => toggleRouteCreate(true);
    empty.appendChild(btn);
    list.appendChild(empty);
    return;
  }
  for (const name of names) {
    const shadowed = routeTab === "combos" && Object.prototype.hasOwnProperty.call(aliases, name);
    const item = document.createElement("div");
    item.className = "gw-route-item";
    item.dataset.name = name;
    const preview = routeTab === "combos"
      ? combos[name].join(" → ")
      : `→ ${aliases[name]}`;
    item.innerHTML = `
      <button class="gw-route-main" type="button" role="listitem"
              aria-expanded="false" onclick="toggleRouteDetail(this)"
              title="Show route graph">
        <span class="gw-route-name gw-mono">${esc(name)}</span>
        <span class="badge gw-${spec.kind}">${spec.kind}</span>
        ${shadowed ? '<span class="badge gw-shadowed" title="An alias of the same name wins at resolve time">shadowed</span>' : ""}
        <span class="gw-route-preview gw-mono">${esc(preview)}</span>
        <span class="ms ms-expand_more gw-route-caret" aria-hidden="true"></span>
      </button>
      <div class="gw-route-expand" hidden></div>`;
    list.appendChild(item);
  }
  // A mutation may have removed the expanded route; reopen only while it exists.
  if (expandedRoute) {
    const stillThere = [...list.querySelectorAll(".gw-route-item")]
      .find(el => el.dataset.name === expandedRoute.name);
    if (stillThere) expandRouteRow(stillThere.querySelector(".gw-route-main"));
    else expandedRoute = null;
  }
}

// ---- Row expansion: the route's own graph board ----

function toggleRouteDetail(btn) {
  const item = btn.closest(".gw-route-item");
  const open = btn.getAttribute("aria-expanded") === "true";
  // One expanded board at a time: collapse any other open row first.
  const prev = document.querySelector('.gw-route-main[aria-expanded="true"]');
  if (prev && prev !== btn) collapseRouteRow(prev);
  if (open) {
    collapseRouteRow(btn);
    expandedRoute = null;
  } else {
    expandRouteRow(btn);
    expandedRoute = { kind: routeTab === "combos" ? "combo" : "alias", name: item.dataset.name };
  }
}

function collapseRouteRow(btn) {
  btn.setAttribute("aria-expanded", "false");
  btn.closest(".gw-route-item").querySelector(".gw-route-expand").hidden = true;
}

function expandRouteRow(btn) {
  const item = btn.closest(".gw-route-item");
  const name = item.dataset.name;
  const expand = item.querySelector(".gw-route-expand");
  const kind = routeTab === "combos" ? "combo" : "alias";
  expand.innerHTML = boardHtml(kind === "combo"
    ? "Drag steps to reorder, × removes a step, the dashed node appends a fallback."
    : "Click the target node to edit the selector this alias rewrites to.");
  expand.hidden = false;
  btn.setAttribute("aria-expanded", "true");
  const lanes = expand.querySelector(".gw-lanes");
  if (kind === "combo") {
    lanes.appendChild(comboLane(name, routeTable.combos[name] || [],
      Object.prototype.hasOwnProperty.call(routeTable.aliases, name)));
  } else {
    lanes.appendChild(aliasLane(name, routeTable.aliases[name] || ""));
  }
  attachFlowDnD(lanes);
  const plane = expand.querySelector(".gw-plane");
  requestAnimationFrame(() => drawRouteEdges(plane));
  watchRoutePlane(plane);
}

// The per-route graph surface: one shared client node fans into the
// route's own lane, corner ticks mark the canvas.
function boardHtml(hint) {
  return `<div class="gw-board">
    <i class="gw-tick tl" aria-hidden="true"></i>
    <i class="gw-tick tr" aria-hidden="true"></i>
    <i class="gw-tick bl" aria-hidden="true"></i>
    <i class="gw-tick br" aria-hidden="true"></i>
    <div class="gw-plane">
      <svg class="gw-edges" aria-hidden="true"></svg>
      <div class="gw-node gw-node-app gw-node-src">
        <div class="gw-node-kicker"><span class="badge gw-kind">client</span></div>
        <div class="gw-node-title">Your app</div>
        <div class="gw-node-sub gw-mono">openai / anthropic</div>
      </div>
      <div class="gw-lanes"></div>
    </div>
  </div>
  <p class="gw-hint">${esc(hint)}</p>`;
}

// A route node heads each lane: the model name a request carries,
// the route kind, and the delete affordance.
function routeNode(name, kind, shadowed) {
  return `<div class="gw-node gw-node-route gw-node-src gw-node-dst" role="listitem"
      title="Requests for model '${esc(name)}' resolve through this route">
    <div class="gw-node-kicker">
      <span class="badge gw-${kind}">${kind}</span>
      ${shadowed ? '<span class="badge gw-shadowed" title="An alias of the same name wins at resolve time">shadowed</span>' : ""}
      <button class="gw-node-x" type="button" data-kind="${kind}" data-name="${esc(name)}"
              title="Delete ${kind}" aria-label="Delete ${kind} ${esc(name)}"
              onclick="deleteRoute(this)"><span class="ms ms-delete" aria-hidden="true"></span></button>
    </div>
    <div class="gw-node-title gw-mono">model: ${esc(name)}</div>
  </div>`;
}

// A selector renders like a Cloudflare provider node: monogram tile,
// integration part in bold, the model remainder in mono below.
function selectorMeta(sel) {
  const slash = sel.indexOf("/");
  const head = slash === -1 ? sel : sel.slice(0, slash);
  const sub = slash === -1 ? "" : sel.slice(slash + 1);
  return `${providerMonogram(sel)}
    <span class="gw-node-meta">
      <span class="gw-node-name">${esc(head)}</span>
      ${sub ? `<span class="gw-node-sub gw-mono">${esc(sub)}</span>` : ""}
    </span>`;
}

function comboLane(name, steps, shadowed) {
  const lane = document.createElement("div");
  lane.className = "gw-lane";
  lane.dataset.kind = "combo";
  lane.setAttribute("role", "list");
  lane.setAttribute("aria-label", `Failover order for ${name}`);
  lane.innerHTML = `
    ${routeNode(name, "combo", shadowed)}
    ${steps.map((s, i) => `
      <div class="gw-node gw-node-step gw-node-src gw-node-dst${i === 0 ? " gw-node-first" : ""}"
           role="listitem" draggable="true"
           data-name="${esc(name)}" data-i="${i}"
           title="Attempt ${i + 1} - drag to reorder">
        <span class="gw-node-order">${i + 1}</span>
        ${selectorMeta(s)}
        <button class="gw-node-x" type="button" aria-label="Remove ${esc(s)}"
                onclick="removeComboStep(this)"><span class="ms ms-close" aria-hidden="true"></span></button>
      </div>`).join("")}
    <div class="gw-node gw-node-add gw-node-dst">
      <form class="gw-node-addform" data-name="${esc(name)}" onsubmit="return addComboStep(event)">
        <input class="gw-node-input gw-mono" type="text" list="gw-model-list"
               autocomplete="off" placeholder="+ fallback" aria-label="Add a fallback step">
        <button class="gw-node-x" type="submit" aria-label="Add step"
                title="Add step"><span class="ms ms-add" aria-hidden="true"></span></button>
      </form>
    </div>`;
  return lane;
}

function aliasLane(name, target) {
  const lane = document.createElement("div");
  lane.className = "gw-lane";
  lane.dataset.kind = "alias";
  lane.setAttribute("role", "list");
  lane.setAttribute("aria-label", `Rewrite target for ${name}`);
  lane.innerHTML = `
    ${routeNode(name, "alias", false)}
    <div class="gw-node gw-node-target gw-node-dst gw-node-edit" role="listitem"
         data-name="${esc(name)}"
         title="Click to edit the rewrite target"
         onclick="editAliasTarget(this)">
      ${selectorMeta(target)}
    </div>`;
  return lane;
}

// Clicking an alias target swaps its label for an inline input;
// Enter or blur commits through the table PUT, Escape reverts.
function editAliasTarget(node) {
  if (node.querySelector(".gw-node-edit-input")) return;
  const name = node.dataset.name;
  const meta = node.querySelector(".gw-node-meta");
  meta.hidden = true;
  const input = document.createElement("input");
  input.className = "gw-node-edit-input gw-mono";
  input.value = routeTable.aliases[name] || "";
  input.setAttribute("list", "gw-model-list");
  input.setAttribute("aria-label", `Rewrite target for ${name}`);
  meta.parentElement.appendChild(input);
  input.focus();
  input.select();
  // Enter and blur both resolve the edit; settle once so the DOM
  // teardown on re-render cannot commit a second time.
  let settled = false;
  const commit = async () => {
    if (settled) return;
    settled = true;
    const value = input.value.trim();
    if (value && value !== routeTable.aliases[name]) {
      try {
        await mutateRoutes(table => { table.aliases[name] = value; });
        toast(`Alias ${name} now rewrites to ${value}.`);
      } catch (e) {
        toast(`Update failed: ${e.message}`, true);
      }
    }
    loadRoutes();
  };
  input.addEventListener("keydown", e => {
    if (e.key === "Enter") { e.preventDefault(); commit(); }
    if (e.key === "Escape") { settled = true; loadRoutes(); }
  });
  input.addEventListener("blur", commit, { once: true });
  input.addEventListener("click", e => e.stopPropagation());
}

// ---- Edge layer: bezier connectors between adjacent nodes ----

const ROUTE_SVG_NS = "http://www.w3.org/2000/svg";

// Right-mid (source) and left-mid (target) ports of a node in plane
// coordinates - the plane scrolls inside its board, so client rects
// plus the scroll offset land on content coordinates.
function ports(el, plane, planeRect) {
  const r = el.getBoundingClientRect();
  const y = r.top - planeRect.top + plane.scrollTop + r.height / 2;
  return {
    sx: r.right - planeRect.left + plane.scrollLeft, sy: y,
    tx: r.left - planeRect.left + plane.scrollLeft, ty: y,
  };
}

function drawRouteEdges(plane) {
  if (!plane || !plane.isConnected) return;
  const svg = plane.querySelector(".gw-edges");
  if (!svg) return;
  // Reset before measuring so a previous size never inflates the plane.
  svg.setAttribute("width", "0");
  svg.setAttribute("height", "0");
  const w = Math.max(plane.scrollWidth, plane.clientWidth);
  const h = Math.max(plane.scrollHeight, plane.clientHeight);
  svg.setAttribute("width", w);
  svg.setAttribute("height", h);
  svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
  svg.innerHTML = "";
  const planeRect = plane.getBoundingClientRect();

  const segment = (a, b, cls, label) => {
    const path = document.createElementNS(ROUTE_SVG_NS, "path");
    const c = Math.max(30, (b.tx - a.sx) * 0.45);
    path.setAttribute("d", `M ${a.sx} ${a.sy} C ${a.sx + c} ${a.sy} ${b.tx - c} ${b.ty} ${b.tx} ${b.ty}`);
    path.setAttribute("class", cls);
    svg.appendChild(path);
    if (label) {
      const text = document.createElementNS(ROUTE_SVG_NS, "text");
      text.setAttribute("class", "gw-edge-label");
      text.setAttribute("x", (a.sx + b.tx) / 2);
      text.setAttribute("y", (a.sy + b.ty) / 2 - 6);
      text.setAttribute("text-anchor", "middle");
      text.textContent = label;
      svg.appendChild(text);
    }
  };

  const app = plane.querySelector(".gw-node-app");
  for (const lane of plane.querySelectorAll(".gw-lane")) {
    const nodes = [...lane.querySelectorAll(":scope > .gw-node")];
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
}

// Redraw on plane size changes - fonts landing and node edits can
// shift node geometry after the first paint. Only one board is
// expanded at a time, so a single observer hops between planes.
let routePlaneObserver = null;
function watchRoutePlane(plane) {
  if (typeof ResizeObserver === "undefined" || !plane) return;
  if (!routePlaneObserver) {
    routePlaneObserver = new ResizeObserver(entries => {
      for (const entry of entries) drawRouteEdges(entry.target);
    });
  }
  routePlaneObserver.disconnect();
  routePlaneObserver.observe(plane);
}
if (document.fonts && document.fonts.ready) {
  document.fonts.ready.then(() => drawRouteEdges(document.querySelector(".gw-plane")));
}

// ---- Step interactions ----

async function addComboStep(event) {
  event.preventDefault();
  const form = event.target;
  const input = form.querySelector(".gw-node-input");
  const value = input.value.trim();
  if (!value) {
    input.focus();
    return false;
  }
  const name = form.dataset.name;
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    await mutateRoutes(table => {
      (table.combos[name] = table.combos[name] || []).push(value);
    });
    toast(`Added ${value} to ${name}.`);
    loadRoutes();
  } catch (e) {
    toast(`Add failed: ${e.message}`, true);
    btn.disabled = false;
  }
  return false;
}

async function removeComboStep(btn) {
  const step = btn.closest(".gw-node-step");
  const name = step.dataset.name;
  const i = Number(step.dataset.i);
  // The API requires at least one selector per combo, so removing the
  // last step deletes the whole combo - confirm before doing it.
  const last = step.parentElement.querySelectorAll(".gw-node-step").length <= 1;
  if (last && !confirm(`Removing the last step deletes combo "${name}". Continue?`)) return;
  btn.disabled = true;
  try {
    await mutateRoutes(table => {
      const steps = table.combos[name] || [];
      if (steps.length <= 1) delete table.combos[name];
      else steps.splice(i, 1);
    });
    toast(last ? `Deleted combo ${name}.` : `Removed a step from ${name}.`);
    loadRoutes();
  } catch (e) {
    toast(`Update failed: ${e.message}`, true);
    btn.disabled = false;
  }
}

// Drag a step onto a sibling to reorder the failover chain; the drop
// commits the new order through the same table PUT.
function attachFlowDnD(lanes) {
  let dragged = null;
  lanes.querySelectorAll(".gw-node-step").forEach(step => {
    step.addEventListener("dragstart", e => {
      dragged = step;
      step.classList.add("gw-drag");
      e.dataTransfer.effectAllowed = "move";
    });
    step.addEventListener("dragend", () => {
      step.classList.remove("gw-drag");
      lanes.querySelectorAll(".gw-node-step").forEach(s => s.classList.remove("gw-drop"));
      dragged = null;
    });
    step.addEventListener("dragover", e => {
      e.preventDefault();
      if (dragged && step !== dragged && step.dataset.name === dragged.dataset.name) {
        step.classList.add("gw-drop");
      }
    });
    step.addEventListener("dragleave", () => step.classList.remove("gw-drop"));
    step.addEventListener("drop", e => {
      e.preventDefault();
      if (!dragged || dragged === step || step.dataset.name !== dragged.dataset.name) return;
      reorderComboStep(step.dataset.name, Number(dragged.dataset.i), Number(step.dataset.i));
    });
  });
}

async function reorderComboStep(name, from, to) {
  try {
    await mutateRoutes(table => {
      const steps = table.combos[name] || [];
      const [moved] = steps.splice(from, 1);
      if (moved !== undefined) steps.splice(to, 0, moved);
    });
    toast("Reordered the failover chain.");
    loadRoutes();
  } catch (e) {
    toast(`Reorder failed: ${e.message}`, true);
  }
}

async function deleteRoute(btn) {
  const kind = btn.dataset.kind;
  const name = btn.dataset.name;
  if (!confirm(`Delete ${kind} "${name}"?`)) return;
  btn.disabled = true;
  try {
    await mutateRoutes(table => {
      delete table[kind === "alias" ? "aliases" : "combos"][name];
    });
    toast(`Deleted ${kind} ${name}.`);
    loadRoutes();
  } catch (e) {
    toast(`Delete failed: ${e.message}`, true);
    btn.disabled = false;
  }
}
