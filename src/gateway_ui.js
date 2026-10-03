// Gateway console: connect snippets, executable providers, named routes,
// API key lifecycle, and the request log - a sidebar-driven single-page
// layout over the same-origin /api/gateway/* endpoints on this loopback
// server. Cross-origin pages are rejected with 403 and network failures
// get the same hint, since both mean the caller is not this dashboard.

const LOG_POLL_MS = 10000;
const SAME_ORIGIN_HINT = "the dashboard only accepts same-origin requests";

// Escape API-supplied strings before they land in innerHTML.
function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, c =>
    ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", '"':"&quot;", "'":"&#39;" }[c]));
}

function fmtDate(ts) {
  if (!ts) return "-";
  return new Date(ts * 1000).toLocaleString([], { month:"short", day:"numeric", hour:"2-digit", minute:"2-digit" });
}

function fmtTime(ts) {
  if (!ts) return "-";
  return new Date(ts * 1000).toLocaleString([], { month:"short", day:"numeric", hour:"2-digit", minute:"2-digit", second:"2-digit" });
}

// Compact relative time for the log view: "just now" .. "6d ago",
// then an absolute date. The cell title keeps the precise stamp.
function relTime(ts) {
  if (!ts) return "-";
  const diff = Date.now() / 1000 - ts;
  if (diff < 5) return "just now";
  if (diff < 60) return Math.floor(diff) + "s ago";
  if (diff < 3600) return Math.floor(diff / 60) + "m ago";
  if (diff < 86400) return Math.floor(diff / 3600) + "h ago";
  if (diff < 7 * 86400) return Math.floor(diff / 86400) + "d ago";
  return fmtDate(ts);
}

// Error bodies arrive either as the OpenAI envelope {"error":{message,type,..}}
// or a bare {"error":".."}; flatten both to one display string.
function apiError(data, status) {
  const err = data && data.error;
  let msg;
  if (err && typeof err === "object") msg = err.message || err.type || JSON.stringify(err);
  else if (typeof err === "string") msg = err;
  else msg = "HTTP " + status;
  if (status === 403) msg += " - " + SAME_ORIGIN_HINT;
  return msg;
}

async function api(path, opts) {
  let r;
  try {
    r = await fetch(path, opts);
  } catch (e) {
    throw new Error("Request failed - " + SAME_ORIGIN_HINT + " (" + String(e.message || e) + ")");
  }
  let data = {};
  try { data = await r.json(); } catch (_) { /* empty or non-JSON body */ }
  if (!r.ok || (data && data.error)) throw new Error(apiError(data, r.status));
  return data;
}

function apiGet(path) {
  return api(path);
}

function apiPost(path, body) {
  return api(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

function apiPut(path, body) {
  return api(path, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

function setMsg(id, html) {
  const el = document.getElementById(id);
  el.innerHTML = html || "";
  el.hidden = !html;
}

function toast(message, isErr) {
  const el = document.createElement("div");
  el.className = "gw-toast" + (isErr ? " err" : "");
  el.setAttribute("role", "status");
  el.textContent = message;
  document.getElementById("gw-toasts").appendChild(el);
  setTimeout(() => {
    el.classList.add("gone");
    setTimeout(() => el.remove(), 300);
  }, 5000);
}

function markUpdated() {
  const status = document.getElementById("updated");
  status.textContent = "Updated " + new Date().toLocaleTimeString();
  status.removeAttribute("title");
}

// ---- Clipboard ----

// navigator.clipboard with a textarea fallback; `done` flips the button
// label so the user sees the copy actually landed.
function copyText(text, btn) {
  const done = () => {
    if (!btn) return;
    if (!btn.dataset.label) btn.dataset.label = btn.textContent;
    btn.textContent = "Copied";
    setTimeout(() => { btn.textContent = btn.dataset.label; }, 1600);
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(done, () => fallbackCopy(text, done));
  } else {
    fallbackCopy(text, done);
  }
}

function fallbackCopy(text, done) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  try {
    document.execCommand("copy");
    done();
  } catch (_) {
    toast("Copy failed - select the text manually.", true);
  }
  ta.remove();
}

function copyFromEl(id, btn) {
  const el = document.getElementById(id);
  copyText(el ? el.textContent : "", btn);
}

// ---- Tag input: datalist-backed chip editor ----
// One widget = a .gw-tags wrapper holding chips and a trailing input.
// Enter or comma commits the typed value; Backspace on an empty input
// drops the last chip. values() returns chips plus any pending text.

function addTag(wrap, value) {
  for (const part of String(value).split(",")) {
    const v = part.trim();
    if (!v) continue;
    const exists = [...wrap.querySelectorAll(".gw-tag")]
      .some(t => t.dataset.value === v);
    if (exists) continue;
    const tag = document.createElement("span");
    tag.className = "gw-tag";
    tag.dataset.value = v;
    tag.innerHTML = `${esc(v)}<button type="button" aria-label="Remove ${esc(v)}">×</button>`;
    tag.querySelector("button").addEventListener("click", () => tag.remove());
    wrap.insertBefore(tag, wrap.querySelector(".gw-tag-input"));
  }
}

function tagValues(wrap) {
  const vals = [...wrap.querySelectorAll(".gw-tag")].map(t => t.dataset.value);
  const pending = wrap.querySelector(".gw-tag-input").value;
  for (const part of pending.split(",")) {
    const v = part.trim();
    if (v && !vals.includes(v)) vals.push(v);
  }
  return vals;
}

function clearTags(wrap) {
  wrap.querySelectorAll(".gw-tag").forEach(t => t.remove());
  wrap.querySelector(".gw-tag-input").value = "";
}

function attachTagInput(wrap) {
  const input = wrap.querySelector(".gw-tag-input");
  wrap.addEventListener("click", () => input.focus());
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      addTag(wrap, input.value);
      input.value = "";
    } else if (e.key === "Backspace" && !input.value) {
      const tags = wrap.querySelectorAll(".gw-tag");
      if (tags.length) tags[tags.length - 1].remove();
    }
  });
}

// ---- Section navigation: one section visible at a time, lazy data ----

const SECTION_LOADERS = {
  connect:   loadConnect,
  providers: loadProviders,
  routes:    loadRoutes,
  keys:      loadKeys,
  logs:      loadLogs,
};
let ACTIVE = "connect";

// Opening a section (re)fetches its data - sections stay lazy in that
// nothing loads until first visited, but every visit shows current data
// and a failed load retries on the next visit. Refresh-all forces all.
function showSection(name, push) {
  if (!SECTION_LOADERS[name]) name = "connect";
  if (push === undefined) push = true;
  ACTIVE = name;
  document.querySelectorAll(".gw-section").forEach(s => {
    s.hidden = s.id !== "gw-" + name;
  });
  document.querySelectorAll(".gw-nav-item").forEach(b => {
    const on = b.dataset.section === name;
    b.classList.toggle("active", on);
    if (on) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  if (push && location.hash !== "#" + name) {
    history.pushState(null, "", "#" + name);
  }
  SECTION_LOADERS[name]();
}

window.addEventListener("hashchange", () => {
  showSection(location.hash.slice(1) || "connect", false);
});

// ---- Model selector suggestions (GET /api/gateway/models) ----
// Feeds the shared datalist on route and key forms. Served on the
// loopback admin surface so it works even once /v1 requires a key;
// degrade quietly if it fails.

let MODEL_IDS = null;

async function ensureModelIds(force) {
  if (MODEL_IDS && !force) return MODEL_IDS;
  try {
    const data = await apiGet("/api/gateway/models");
    MODEL_IDS = (data.data || []).map(m => m.id).filter(Boolean);
  } catch (_) {
    MODEL_IDS = MODEL_IDS || [];
  }
  const list = document.getElementById("gw-model-list");
  list.innerHTML = MODEL_IDS.map(id => `<option value="${esc(id)}"></option>`).join("");
  return MODEL_IDS;
}

// ---- Connect ----

const ORIGIN = window.location.origin;

// Client cards: name, one-line guidance, and a copyable snippet built
// against the real origin so the pasted config works verbatim.
const CLIENT_CARDS = [
  {
    name: "Cline / Roo Code",
    sub: "VS Code extension - choose the \"OpenAI Compatible\" provider.",
    snippet: o =>
`API Provider:   OpenAI Compatible
Base URL:       ${o}/v1
API Key:        aifuel-gw-<your key>
Model:          auto`,
  },
  {
    name: "Codex CLI",
    sub: "Environment variables for the current shell, or pin a provider in ~/.codex/config.toml.",
    snippet: o =>
`export OPENAI_BASE_URL=${o}/v1
export OPENAI_API_KEY=aifuel-gw-<your key>

# or in ~/.codex/config.toml:
# model_provider = "aifuel"
# model = "auto"
# [model_providers.aifuel]
# name     = "aifuel"
# base_url = "${o}/v1"
# wire_api = "chat"
# env_key  = "OPENAI_API_KEY"`,
  },
  {
    name: "aider",
    sub: "OpenAI-compatible flags; prefix the model with openai/.",
    snippet: o =>
`aider --openai-api-base ${o}/v1 \\
      --openai-api-key aifuel-gw-<your key> \\
      --model openai/auto`,
  },
  {
    name: "LibreChat",
    sub: "Custom endpoint block in librechat.yaml.",
    snippet: o =>
`endpoints:
  custom:
    - name: "aifuel"
      baseURL: "${o}/v1"
      apiKey: "aifuel-gw-<your key>"
      models:
        default: ["auto"]
        fetch: true
      titleConvo: true
      modelDisplayLabel: "aifuel"`,
  },
  {
    name: "OpenAI SDK",
    sub: "Python - any OpenAI-compatible SDK takes the same two settings.",
    snippet: o =>
`from openai import OpenAI

client = OpenAI(
    base_url="${o}/v1",
    api_key="aifuel-gw-<your key>",
)
resp = client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "Say hello"}],
)`,
  },
  {
    name: "curl",
    sub: "Smoke-test the gateway directly.",
    snippet: o =>
`curl ${o}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer aifuel-gw-<your key>" \\
  -d '{
    "model": "auto",
    "messages": [{"role": "user", "content": "Say hello"}]
  }'`,
  },
];

// One-time static render: endpoint URLs and the client card grid.
function initConnectStatic() {
  document.getElementById("connect-base-url").textContent = ORIGIN + "/v1";
  document.getElementById("connect-messages-url").textContent = ORIGIN + "/v1/messages";
  const grid = document.getElementById("connect-clients");
  for (const spec of CLIENT_CARDS) {
    const card = document.createElement("div");
    card.className = "gw-client-card";
    card.innerHTML = `
      <div class="gw-client-head">
        <span class="gw-client-name">${esc(spec.name)}</span>
        <button class="btn" type="button">Copy</button>
      </div>
      <div class="gw-client-sub">${esc(spec.sub)}</div>
      <pre class="gw-snippet"><code></code></pre>`;
    const code = card.querySelector("code");
    code.textContent = spec.snippet(ORIGIN);
    card.querySelector("button").addEventListener("click", e =>
      copyText(code.textContent, e.currentTarget));
    grid.appendChild(card);
  }
}

// The Connect section's only fetch: the key list decides whether the
// open-auth hint shows. Reuses the shared keys fetch so the Keys section
// cache stays coherent.
async function loadConnect() {
  try {
    const keys = await fetchKeys();
    KEYS_CACHE = keys;
    document.getElementById("connect-anon-hint").hidden = keys.length > 0;
    setMsg("connect-msg", "");
    markUpdated();
  } catch (e) {
    setMsg("connect-msg", `<span class="err">Could not check gateway keys: ${esc(e.message)}</span>`);
  }
}

// ---- Providers ----

// Group cards by the kind the API reports per integration (derived
// server-side from the id suffix). Unknown kinds land in "other".
const KIND_ORDER = ["cli", "api-key", "oauth", "local", "web", "other"];
const KIND_LABEL = {
  "cli":     "CLI integrations",
  "api-key": "API-key endpoints",
  "oauth":   "OAuth integrations",
  "local":   "Local servers",
  "web":     "Web sessions",
  "other":   "Other",
};

// Dot semantics from the API's `ready` flag: green = the integration's
// discovery evidence (credential file, env var, managed key, or endpoint
// config) is present locally; gray = absent; amber = evidence could not
// be evaluated (registry unavailable). Presence means "configured", not
// "verified working" - the first real request still proves the route.
function providerDot(p) {
  if (p.ready === true) return ["var(--green)", "credential or presence evidence found"];
  if (p.ready === false) return ["var(--label-3)", "no local evidence - set a key or sign in"];
  return ["var(--orange)", "evidence unavailable"];
}

function providerCard(p) {
  const card = document.createElement("div");
  card.className = "gw-p-card";
  const [color, dotTitle] = providerDot(p);
  const badges = ['<span class="badge gw-chat">chat</span>'];
  if (p.embeddings) badges.push('<span class="badge gw-embed">embeddings</span>');
  card.innerHTML = `
    <div class="gw-p-head">
      <span class="gw-dot" style="background:${color}" title="${dotTitle}"></span>
      <span class="gw-p-name" title="${esc(p.integration)}">${esc(p.integration)}</span>
    </div>
    <div class="gw-p-provider">provider: <span class="gw-mono">${esc(p.provider)}</span></div>
    <div class="badges">${badges.join("")}</div>`;
  return card;
}

async function loadProviders() {
  const wrap = document.getElementById("providers-groups");
  try {
    const data = await apiGet("/api/gateway/providers");
    const list = data.providers || [];
    wrap.innerHTML = "";
    if (!list.length) {
      wrap.innerHTML = '<div class="gw-card"><div class="empty">No executable integrations are registered.</div></div>';
    } else {
      const groups = new Map();
      for (const p of list) {
        const kind = KIND_LABEL[p.kind] ? p.kind : "other";
        if (!groups.has(kind)) groups.set(kind, []);
        groups.get(kind).push(p);
      }
      for (const kind of KIND_ORDER) {
        const items = groups.get(kind);
        if (!items || !items.length) continue;
        const h = document.createElement("h3");
        h.className = "gw-group-title";
        h.textContent = KIND_LABEL[kind];
        wrap.appendChild(h);
        const grid = document.createElement("div");
        grid.className = "gw-provider-grid";
        items.forEach(p => grid.appendChild(providerCard(p)));
        wrap.appendChild(grid);
      }
    }
    setMsg("providers-msg", "");
    markUpdated();
  } catch (e) {
    wrap.innerHTML = "";
    setMsg("providers-msg", `<span class="err">Providers failed to load: ${esc(e.message)}</span>`);
  }
}

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

// ---- Request log ----

function logErrorText(entry) {
  const err = entry && entry.error;
  if (!err) return "";
  return typeof err === "string" ? err : (err.message || JSON.stringify(err));
}

// 2xx green, 4xx amber, 5xx+ red; anything else (redirects, odd codes)
// stays neutral gray.
function statusClass(s) {
  if (s >= 200 && s < 300) return "ok";
  if (s >= 400 && s < 500) return "warn";
  if (s >= 500) return "fail";
  return "other";
}

function logLimit() {
  const sel = document.getElementById("logs-limit");
  const n = parseInt(sel && sel.value, 10);
  return n > 0 ? n : 200;
}

async function loadLogs() {
  const body = document.getElementById("logs-body");
  try {
    const data = await apiGet(`/api/gateway/logs?limit=${logLimit()}`);
    const entries = data.entries || [];
    body.innerHTML = "";
    if (!entries.length) {
      body.innerHTML = '<tr><td class="gw-empty" colspan="7">No requests logged yet.</td></tr>';
    }
    for (const e of entries) {
      const tr = document.createElement("tr");
      const tokens = e.usage && e.usage.total_tokens != null
        ? Number(e.usage.total_tokens).toLocaleString()
        : '<span class="gw-dim">-</span>';
      const errTxt = logErrorText(e);
      tr.innerHTML = `
        <td class="gw-mono" title="${esc(fmtTime(e.ts_unix))}">${esc(relTime(e.ts_unix))}</td>
        <td class="gw-mono">${esc(e.model)}</td>
        <td class="gw-mono">${e.integration ? esc(e.integration) : '<span class="gw-dim">-</span>'}</td>
        <td><span class="gw-status ${statusClass(e.status)}">${esc(e.status ?? "-")}</span></td>
        <td>${e.stream ? '<span class="badge gw-stream">stream</span>' : '<span class="gw-dim">-</span>'}</td>
        <td class="gw-num">${tokens}</td>
        <td class="gw-err-cell" ${errTxt ? `title="${esc(errTxt)}"` : ""}>${errTxt ? esc(errTxt) : '<span class="gw-dim">-</span>'}</td>`;
      body.appendChild(tr);
    }
    setMsg("logs-msg", "");
    markUpdated();
  } catch (e) {
    setMsg("logs-msg", `<span class="err">Request log failed: ${esc(e.message)}</span>`);
    document.getElementById("updated").innerHTML = '<span class="err">log refresh failed</span>';
  }
}

// ---- Init ----

// Refresh-all: every section's loader plus the model suggestions - the
// sidebar keeps its lazy per-section loads for normal navigation.
async function refreshAll() {
  const btn = document.getElementById("refresh-btn");
  btn.classList.add("loading");
  btn.setAttribute("aria-busy", "true");
  await Promise.allSettled([
    loadConnect(),
    loadProviders(),
    loadRoutes(),
    loadKeys(),
    loadLogs(),
    ensureModelIds(true),
  ]);
  btn.classList.remove("loading");
  btn.removeAttribute("aria-busy");
  markUpdated();
}

initConnectStatic();
attachTagInput(document.getElementById("create-key-tags"));
ensureModelIds();
showSection(location.hash.slice(1) || "connect", false);
// The log poll only fires while the Logs section is visible and the tab
// is foregrounded - other sections stay on explicit refresh.
setInterval(() => {
  if (ACTIVE === "logs" && !document.hidden) loadLogs();
}, LOG_POLL_MS);
