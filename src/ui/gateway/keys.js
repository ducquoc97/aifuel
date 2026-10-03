// ---- API Keys ----

let KEYS_CACHE = [];
let EDITING_KEY = null;   // id of the key row with an open allowlist editor

async function fetchKeys() {
  const data = await apiGet("/api/gateway/keys");
  return data.keys || [];
}

function keyModelsCell(k) {
  if (k.models && k.models.length) {
    return k.models.map(m => `<span class="gw-chip">${esc(m)}</span>`).join("");
  }
  return '<span class="gw-dim">all models</span>';
}

async function loadKeys() {
  const body = document.getElementById("keys-body");
  ensureModelIds();
  try {
    const list = await fetchKeys();
    KEYS_CACHE = list;
    document.getElementById("connect-anon-hint").hidden = list.length > 0;
    renderKeys(list);
    setMsg("keys-msg", "");
    markUpdated();
  } catch (e) {
    body.innerHTML = "";
    setMsg("keys-msg", `<span class="err">Keys failed to load: ${esc(e.message)}</span>`);
  }
}

function renderKeys(list) {
  const body = document.getElementById("keys-body");
  body.innerHTML = "";
  if (!list.length) {
    body.innerHTML = '<tr><td class="gw-empty" colspan="7">No gateway keys yet - create one above.</td></tr>';
    EDITING_KEY = null;
    return;
  }
  if (EDITING_KEY && !list.some(k => k.id === EDITING_KEY)) EDITING_KEY = null;
  for (const k of list) {
    const tr = document.createElement("tr");
    if (k.revoked) tr.className = "gw-revoked";
    const state = k.revoked
      ? '<span class="badge gw-revoked">revoked</span>'
      : '<span class="badge gw-active">active</span>';
    let modelsCell, actions;
    if (EDITING_KEY === k.id) {
      modelsCell = `<td class="gw-models-cell" data-edit-id="${esc(k.id)}"></td>`;
      actions = `
        <button class="btn" type="button" data-id="${esc(k.id)}"
                onclick="saveKeyModels(this)">Save</button>
        <button class="btn" type="button" onclick="cancelKeyEdit()">Cancel</button>`;
    } else {
      modelsCell = `<td class="gw-models-cell">${keyModelsCell(k)}</td>`;
      actions = k.revoked
        ? '<span class="gw-dim">-</span>'
        : `<button class="btn" type="button" data-id="${esc(k.id)}"
                   onclick="editKeyModels(this)">Edit models</button>
           <button class="btn gw-revoke" type="button" data-id="${esc(k.id)}" data-name="${esc(k.name)}"
                   onclick="revokeKey(this)">Revoke</button>`;
    }
    tr.innerHTML = `
      <td class="gw-primary">${esc(k.name)}</td>
      <td class="gw-mono" title="${esc(k.id)}">${esc(k.prefix)}</td>
      ${modelsCell}
      <td>${fmtDate(k.created_at)}</td>
      <td>${k.last_used_at ? fmtDate(k.last_used_at) : '<span class="gw-dim">never</span>'}</td>
      <td>${state}</td>
      <td class="gw-actions">${actions}</td>`;
    body.appendChild(tr);
  }
  // Seed the open allowlist editor with the key's current models.
  if (EDITING_KEY) {
    const key = list.find(k => k.id === EDITING_KEY);
    const cell = body.querySelector(".gw-models-cell[data-edit-id]");
    if (key && cell) {
      const wrap = document.createElement("div");
      wrap.className = "gw-tags";
      wrap.innerHTML = '<input class="gw-tag-input" type="text" list="gw-model-list" autocomplete="off" placeholder="Type a selector, Enter to add">';
      attachTagInput(wrap);
      (key.models || []).forEach(m => addTag(wrap, m));
      cell.appendChild(wrap);
      wrap.querySelector(".gw-tag-input").focus();
    }
  }
}

function editKeyModels(btn) {
  EDITING_KEY = btn.dataset.id;
  renderKeys(KEYS_CACHE);
}

function cancelKeyEdit() {
  EDITING_KEY = null;
  renderKeys(KEYS_CACHE);
}

// POST /api/gateway/keys/update: {"id","models":[...]}; omitting models
// clears the allowlist back to "every model" - never send an empty list,
// which would store a permit-nothing allowlist instead.
async function saveKeyModels(btn) {
  const id = btn.dataset.id;
  const wrap = btn.closest("tr").querySelector(".gw-tags");
  const models = wrap ? tagValues(wrap) : [];
  btn.disabled = true;
  try {
    const body = { id };
    if (models.length) body.models = models;
    await apiPost("/api/gateway/keys/update", body);
    EDITING_KEY = null;
    toast("Updated model allowlist.");
    loadKeys();
  } catch (e) {
    toast(`Update failed: ${e.message}`, true);
    btn.disabled = false;
  }
}

async function createKey(event) {
  event.preventDefault();
  const form = event.target;
  const name = form.elements.name.value.trim();
  if (!name) {
    setMsg("keys-msg", '<span class="err">Give the key a name first.</span>');
    return false;
  }
  const wrap = document.getElementById("create-key-tags");
  const models = tagValues(wrap);
  const payload = { name };
  if (models.length) payload.models = models;

  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    const data = await apiPost("/api/gateway/keys", payload);
    form.reset();
    clearTags(wrap);
    setMsg("keys-msg", "");
    showNewKey(data.key);
    loadKeys();
  } catch (e) {
    setMsg("keys-msg", `<span class="err">${esc(e.message)}</span>`);
  } finally {
    btn.disabled = false;
  }
  return false;
}

async function revokeKey(btn) {
  const label = btn.dataset.name || btn.dataset.id;
  if (!confirm(`Revoke gateway key "${label}"? Apps using it stop working immediately.`)) return;
  btn.disabled = true;
  try {
    await apiPost("/api/gateway/keys/revoke", { id: btn.dataset.id });
    toast(`Revoked ${label}.`);
    loadKeys();
  } catch (e) {
    toast(`Revoke failed: ${e.message}`, true);
    btn.disabled = false;
  }
}

// ---- One-shot raw key callout ----

function showNewKey(rawKey) {
  document.getElementById("new-key-value").textContent = rawKey || "";
  const callout = document.getElementById("new-key-callout");
  callout.hidden = false;
  callout.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function dismissNewKey() {
  document.getElementById("new-key-callout").hidden = true;
  // Clear the raw key from the DOM so it does not linger after dismissal.
  document.getElementById("new-key-value").textContent = "";
}

function copyNewKey(btn) {
  copyText(document.getElementById("new-key-value").textContent, btn);
}
