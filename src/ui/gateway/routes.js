// ---- Routes (aliases + combos in gateway.json) ----
// Combos render as a visual failover chain - request node, numbered
// provider steps in attempt order, an inline add-step form - matching
// the pipeline diagram style of gateway products like Cloudflare's.
// Steps are removable, appendable, and drag-reorderable; every edit
// commits through the same whole-table PUT.

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
    renderCombos(data.combos || {}, data.aliases || {});
    renderAliases(data.aliases || {});
    setMsg("routes-msg", "");
    markUpdated();
  } catch (e) {
    document.getElementById("combo-list").innerHTML = "";
    document.getElementById("alias-body").innerHTML = "";
    setMsg("routes-msg", `<span class="err">Routes failed to load: ${esc(e.message)}</span>`);
  }
}

// ---- Combos: one card per chain ----

function renderCombos(combos, aliases) {
  const list = document.getElementById("combo-list");
  const names = Object.keys(combos);
  list.innerHTML = "";
  if (!names.length) {
    list.innerHTML = '<div class="gw-card"><div class="gw-empty">No combos yet - add one above to chain providers for failover.</div></div>';
    return;
  }
  for (const name of names) {
    const steps = combos[name];
    // An alias shadows a combo of the same name at resolve time; say so.
    const shadowed = Object.prototype.hasOwnProperty.call(aliases, name);
    const card = document.createElement("div");
    card.className = "gw-combo-card";
    card.dataset.name = name;
    card.innerHTML = `
      <div class="gw-combo-head">
        <span class="gw-combo-name">${esc(name)}</span>
        <span class="badge gw-combo">combo</span>
        ${shadowed ? '<span class="badge gw-shadowed" title="An alias of the same name wins at resolve time">shadowed</span>' : ""}
        <button class="btn gw-revoke" type="button" data-kind="combo" data-name="${esc(name)}"
                onclick="deleteRoute(this)">Delete</button>
      </div>
      <div class="gw-flow" role="list" aria-label="Failover order for ${esc(name)}">
        <span class="gw-flow-req gw-mono" title="Inbound model name">model: ${esc(name)}</span>
        ${steps.map((s, i) => `
          <span class="ms ms-chevron_right gw-flow-arrow" aria-hidden="true"></span>
          <span class="gw-flow-step" role="listitem" draggable="true"
                data-name="${esc(name)}" data-i="${i}"
                title="Attempt ${i + 1} - drag to reorder">
            <span class="gw-flow-order">${i + 1}</span>
            ${providerMonogram(s)}
            <span class="gw-flow-sel gw-mono">${esc(s)}</span>
            <button class="gw-flow-x" type="button" aria-label="Remove ${esc(s)}"
                    onclick="removeComboStep(this)">×</button>
          </span>`).join("")}
        <span class="ms ms-chevron_right gw-flow-arrow" aria-hidden="true"></span>
        <form class="gw-flow-add" data-name="${esc(name)}" onsubmit="return addComboStep(event)">
          <input class="gw-flow-input gw-mono" type="text" list="gw-model-list"
                 autocomplete="off" placeholder="+ fallback" aria-label="Add a fallback step">
          <button class="btn" type="submit">Add</button>
        </form>
      </div>`;
    list.appendChild(card);
  }
  attachFlowDnD(list);
}

function renderAliases(aliases) {
  const body = document.getElementById("alias-body");
  const names = Object.keys(aliases);
  body.innerHTML = "";
  if (!names.length) {
    body.innerHTML = '<tr><td class="gw-empty" colspan="3">No aliases yet - add one above to give a selector a short name.</td></tr>';
    return;
  }
  for (const name of names) {
    const tr = document.createElement("tr");
    tr.innerHTML = `
      <td class="gw-primary">${esc(name)}</td>
      <td class="gw-route-target">
        <span class="gw-alias-flow">
          <span class="ms ms-chevron_right gw-flow-arrow" aria-hidden="true"></span>
          ${providerMonogram(aliases[name])}
          <span class="gw-mono">${esc(aliases[name])}</span>
        </span>
      </td>
      <td class="gw-num"><button class="btn gw-revoke" type="button"
               data-kind="alias" data-name="${esc(name)}"
               onclick="deleteRoute(this)">Delete</button></td>`;
    body.appendChild(tr);
  }
}

// ---- Step interactions ----

async function addComboStep(event) {
  event.preventDefault();
  const form = event.target;
  const input = form.querySelector(".gw-flow-input");
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
  const step = btn.closest(".gw-flow-step");
  const name = step.dataset.name;
  const i = Number(step.dataset.i);
  // The API requires at least one selector per combo, so removing the
  // last step deletes the whole combo - confirm before doing it.
  const last = step.parentElement.querySelectorAll(".gw-flow-step").length <= 1;
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
  list.querySelectorAll(".gw-flow-step").forEach(step => {
    step.addEventListener("dragstart", e => {
      dragged = step;
      step.classList.add("gw-drag");
      e.dataTransfer.effectAllowed = "move";
    });
    step.addEventListener("dragend", () => {
      step.classList.remove("gw-drag");
      list.querySelectorAll(".gw-flow-step").forEach(s => s.classList.remove("gw-drop"));
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
