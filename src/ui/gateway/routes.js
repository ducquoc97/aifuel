// ---- Routes (aliases + combos in gateway.json) ----
// The section renders as one node-graph canvas in the style of
// Cloudflare's AI Gateway "Dynamic Routing" diagram: a shared client
// node on the left fans out to one lane per named route. A combo lane
// chains its provider steps in attempt order - "primary" edge into
// step 1, dashed "fallback" edges after - and ends in a dashed
// add-step node; an alias lane hops once to its target on a "rewrite"
// edge. Edges are bezier connectors drawn in an SVG overlay after
// layout. Nodes stay interactive: steps are removable, appendable,
// and drag-reorderable; every edit commits through the same
// whole-table PUT.

// The API replaces the whole table, so every mutation is: fetch the
// current table, apply the change client-side, PUT the merged object.
async function mutateRoutes(mutator) {
  const data = await apiGet("/api/gateway/routes");
  const table = { aliases: data.aliases || {}, combos: data.combos || {} };
  mutator(table);
  await apiPut("/api/gateway/routes", table);
}

async function loadRoutes() {
  ensureModelIds();
  try {
    const data = await apiGet("/api/gateway/routes");
    renderRouteBoard(data.combos || {}, data.aliases || {});
    setMsg("routes-msg", "");
    markUpdated();
  } catch (e) {
    document.getElementById("route-lanes").innerHTML = "";
    drawRouteEdges();
    setMsg("routes-msg", `<span class="err">Routes failed to load: ${esc(e.message)}</span>`);
  }
}

// ---- Board render: one lane per route ----

function renderRouteBoard(combos, aliases) {
  const lanes = document.getElementById("route-lanes");
  lanes.innerHTML = "";
  const comboNames = Object.keys(combos);
  const aliasNames = Object.keys(aliases);
  if (!comboNames.length && !aliasNames.length) {
    lanes.innerHTML =
      '<div class="gw-board-empty">No routes yet - add an alias or combo above to draw the first path.</div>';
  }
  for (const name of comboNames) {
    // An alias shadows a combo of the same name at resolve time; say so.
    lanes.appendChild(comboLane(name, combos[name], Object.prototype.hasOwnProperty.call(aliases, name)));
  }
  for (const name of aliasNames) {
    lanes.appendChild(aliasLane(name, aliases[name]));
  }
  attachFlowDnD(lanes);
  requestAnimationFrame(drawRouteEdges);
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
    <div class="gw-node gw-node-target gw-node-dst" role="listitem"
         title="Alias rewrites to this selector">
      ${selectorMeta(target)}
    </div>`;
  return lane;
}

// ---- Edge layer: bezier connectors between adjacent nodes ----

const ROUTE_SVG_NS = "http://www.w3.org/2000/svg";

// Right-mid (source) and left-mid (target) ports of a node in plane
// coordinates - plane scrolls with the board, so client rects plus
// the scroll offset land on content coordinates.
function ports(el, plane, planeRect) {
  const r = el.getBoundingClientRect();
  const y = r.top - planeRect.top + plane.scrollTop + r.height / 2;
  return {
    sx: r.right - planeRect.left + plane.scrollLeft, sy: y,
    tx: r.left - planeRect.left + plane.scrollLeft, ty: y,
  };
}

function drawRouteEdges() {
  const plane = document.getElementById("route-plane");
  const svg = document.getElementById("route-edges");
  if (!plane || !svg) return;
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

  const app = document.getElementById("route-app");
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

// Redraw on lane/plane size changes - fonts landing and node edits can
// shift node geometry after the first paint.
let routeBoardObserved = false;
function observeRouteBoard() {
  if (routeBoardObserved || typeof ResizeObserver === "undefined") return;
  const plane = document.getElementById("route-plane");
  if (!plane) return;
  routeBoardObserved = true;
  new ResizeObserver(drawRouteEdges).observe(plane);
}
observeRouteBoard();
if (document.fonts && document.fonts.ready) {
  document.fonts.ready.then(drawRouteEdges);
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
function attachFlowDnD(list) {
  let dragged = null;
  list.querySelectorAll(".gw-node-step").forEach(step => {
    step.addEventListener("dragstart", e => {
      dragged = step;
      step.classList.add("gw-drag");
      e.dataTransfer.effectAllowed = "move";
    });
    step.addEventListener("dragend", () => {
      step.classList.remove("gw-drag");
      list.querySelectorAll(".gw-node-step").forEach(s => s.classList.remove("gw-drop"));
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

// ---- Add forms ----

async function addAlias(event) {
  event.preventDefault();
  const form = event.target;
  const name = form.elements.name.value.trim();
  const target = form.elements.target.value.trim();
  if (!name || !target) {
    setMsg("routes-msg", '<span class="err">Name and target are required.</span>');
    return false;
  }
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    await mutateRoutes(table => { table.aliases[name] = target; });
    form.reset();
    setMsg("routes-msg", "");
    toast(`Added alias ${name}.`);
    loadRoutes();
  } catch (e) {
    setMsg("routes-msg", `<span class="err">${esc(e.message)}</span>`);
  } finally {
    btn.disabled = false;
  }
  return false;
}

async function addCombo(event) {
  event.preventDefault();
  const form = event.target;
  const name = form.elements.name.value.trim();
  const selectors = form.elements.selectors.value
    .split(",").map(s => s.trim()).filter(Boolean);
  if (!name || !selectors.length) {
    setMsg("routes-msg", '<span class="err">Name and at least one selector are required.</span>');
    return false;
  }
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    await mutateRoutes(table => { table.combos[name] = selectors; });
    form.reset();
    setMsg("routes-msg", "");
    toast(`Added combo ${name}.`);
    loadRoutes();
  } catch (e) {
    setMsg("routes-msg", `<span class="err">${esc(e.message)}</span>`);
  } finally {
    btn.disabled = false;
  }
  return false;
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
