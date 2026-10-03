// ---- Routes (aliases + combos in gateway.json) ----

// The API replaces the whole table, so every mutation is: fetch the
// current table, apply the change client-side, PUT the merged object.
async function mutateRoutes(mutator) {
  const data = await apiGet("/api/gateway/routes");
  const table = { aliases: data.aliases || {}, combos: data.combos || {} };
  mutator(table);
  await apiPut("/api/gateway/routes", table);
}

async function loadRoutes() {
  const body = document.getElementById("routes-body");
  ensureModelIds();
  try {
    const data = await apiGet("/api/gateway/routes");
    const aliases = data.aliases || {};
    const combos = data.combos || {};
    const rows = Object.keys(aliases).map(n => ["alias", n])
      .concat(Object.keys(combos).map(n => ["combo", n]));
    body.innerHTML = "";
    if (!rows.length) {
      body.innerHTML = '<tr><td class="gw-empty" colspan="4">No named routes yet - add an alias or combo above.</td></tr>';
    }
    for (const [kind, name] of rows) {
      const tr = document.createElement("tr");
      // Aliases show their rewrite target; combos render each selector
      // in failover order as a chip list.
      const target = kind === "alias"
        ? `<span class="gw-mono">${esc(aliases[name])}</span>`
        : combos[name].map(s => `<span class="gw-chip">${esc(s)}</span>`).join("");
      // An alias shadows a combo of the same name at resolve time; say so.
      const shadowed = kind === "combo" &&
        Object.prototype.hasOwnProperty.call(aliases, name);
      const badge = `<span class="badge gw-${kind}">${kind}</span>` +
        (shadowed ? ' <span class="badge gw-shadowed">shadowed</span>' : "");
      tr.innerHTML = `
        <td class="gw-primary">${esc(name)}</td>
        <td>${badge}</td>
        <td class="gw-route-target">${target}</td>
        <td class="gw-num"><button class="btn gw-revoke" type="button"
                 data-kind="${kind}" data-name="${esc(name)}"
                 onclick="deleteRoute(this)">Delete</button></td>`;
      body.appendChild(tr);
    }
    setMsg("routes-msg", "");
    markUpdated();
  } catch (e) {
    body.innerHTML = "";
    setMsg("routes-msg", `<span class="err">Routes failed to load: ${esc(e.message)}</span>`);
  }
}

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
