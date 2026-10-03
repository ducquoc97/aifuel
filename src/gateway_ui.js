// Gateway console: executable providers, gateway API key lifecycle, and the
// request log. All calls go to same-origin /api/gateway/* endpoints on this
// loopback server; cross-origin pages are rejected with 403 and network
// failures get the same hint, since both mean the caller is not this dashboard.

const LOG_LIMIT = 200;
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

function apiPost(path, body) {
  return api(path, {
    method: "POST",
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

// ---- Providers ----

function yesNo(v) {
  return v ? '<span class="badge gw-yes">yes</span>'
           : '<span class="badge">no</span>';
}

async function loadProviders() {
  const body = document.getElementById("providers-body");
  try {
    const data = await api("/api/gateway/providers");
    const list = data.providers || [];
    body.innerHTML = "";
    if (!list.length) {
      body.innerHTML = '<tr><td class="gw-empty" colspan="4">No executable integrations are registered.</td></tr>';
    }
    for (const p of list) {
      const tr = document.createElement("tr");
      tr.innerHTML = `
        <td class="gw-primary">${esc(p.integration)}</td>
        <td>${esc(p.provider)}</td>
        <td>${yesNo(p.streaming)}</td>
        <td>${yesNo(p.read_only)}</td>`;
      body.appendChild(tr);
    }
    setMsg("providers-msg", "");
  } catch (e) {
    body.innerHTML = "";
    setMsg("providers-msg", `<span class="err">Providers failed to load: ${esc(e.message)}</span>`);
  }
}

// ---- API Keys ----

async function loadKeys() {
  const body = document.getElementById("keys-body");
  try {
    const data = await api("/api/gateway/keys");
    const list = data.keys || [];
    body.innerHTML = "";
    if (!list.length) {
      body.innerHTML = '<tr><td class="gw-empty" colspan="6">No gateway keys yet - create one above.</td></tr>';
    }
    for (const k of list) {
      const tr = document.createElement("tr");
      if (k.revoked) tr.className = "gw-revoked";
      const models = k.models && k.models.length
        ? esc(k.models.join(", "))
        : '<span class="gw-dim">all</span>';
      const action = k.revoked
        ? '<span class="badge gw-revoked">revoked</span>'
        : `<button class="btn gw-revoke" type="button" data-id="${esc(k.id)}" data-name="${esc(k.name)}"
                   onclick="revokeKey(this)">Revoke</button>`;
      tr.innerHTML = `
        <td class="gw-primary">${esc(k.name)}</td>
        <td class="gw-mono">${esc(k.prefix)}</td>
        <td>${fmtDate(k.created_at)}</td>
        <td>${k.last_used_at ? fmtDate(k.last_used_at) : '<span class="gw-dim">never</span>'}</td>
        <td>${models}</td>
        <td class="gw-num">${action}</td>`;
      body.appendChild(tr);
    }
    setMsg("keys-msg", "");
  } catch (e) {
    body.innerHTML = "";
    setMsg("keys-msg", `<span class="err">Keys failed to load: ${esc(e.message)}</span>`);
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
  // Comma text -> models array; empty input omits the field (all models).
  const models = form.elements.models.value
    .split(",").map(s => s.trim()).filter(Boolean);
  const payload = { name };
  if (models.length) payload.models = models;

  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    const data = await apiPost("/api/gateway/keys", payload);
    form.reset();
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
  const key = document.getElementById("new-key-value").textContent;
  const done = () => {
    btn.textContent = "Copied";
    setTimeout(() => { btn.textContent = "Copy"; }, 1600);
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(key).then(done, () => fallbackCopy(key, done));
  } else {
    fallbackCopy(key, done);
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
    toast("Copy failed - select the key text manually.", true);
  }
  ta.remove();
}

// ---- Request log ----

function logErrorText(entry) {
  const err = entry && entry.error;
  if (!err) return "";
  return typeof err === "string" ? err : (err.message || JSON.stringify(err));
}

async function loadLogs() {
  const body = document.getElementById("logs-body");
  const status = document.getElementById("updated");
  try {
    const data = await api(`/api/gateway/logs?limit=${LOG_LIMIT}`);
    const entries = data.entries || [];
    body.innerHTML = "";
    if (!entries.length) {
      body.innerHTML = '<tr><td class="gw-empty" colspan="7">No requests logged yet.</td></tr>';
    }
    for (const e of entries) {
      const tr = document.createElement("tr");
      const ok = e.status >= 200 && e.status < 300;
      const tokens = e.usage && e.usage.total_tokens != null
        ? Number(e.usage.total_tokens).toLocaleString()
        : '<span class="gw-dim">-</span>';
      const errTxt = logErrorText(e);
      tr.innerHTML = `
        <td class="gw-mono">${fmtTime(e.ts_unix)}</td>
        <td class="gw-primary">${esc(e.model)}</td>
        <td>${esc(e.integration)}</td>
        <td><span class="gw-status ${ok ? "ok" : "fail"}">${esc(e.status ?? "-")}</span></td>
        <td>${e.stream ? '<span class="badge gw-stream">stream</span>' : '<span class="gw-dim">-</span>'}</td>
        <td class="gw-num">${tokens}</td>
        <td class="gw-err-cell" ${errTxt ? `title="${esc(errTxt)}"` : ""}>${errTxt ? esc(errTxt) : '<span class="gw-dim">-</span>'}</td>`;
      body.appendChild(tr);
    }
    setMsg("logs-msg", "");
    status.textContent = "Updated " + new Date().toLocaleTimeString();
    status.removeAttribute("title");
  } catch (e) {
    setMsg("logs-msg", `<span class="err">Request log failed: ${esc(e.message)}</span>`);
    status.innerHTML = '<span class="err">log refresh failed</span>';
  }
}

// ---- Init ----

async function refreshAll() {
  const btn = document.getElementById("refresh-btn");
  btn.classList.add("loading");
  btn.setAttribute("aria-busy", "true");
  await Promise.allSettled([loadProviders(), loadKeys(), loadLogs()]);
  btn.classList.remove("loading");
  btn.removeAttribute("aria-busy");
}

refreshAll();
setInterval(loadLogs, LOG_POLL_MS);
