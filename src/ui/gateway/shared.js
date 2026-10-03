// Gateway console shared helpers: same-origin /api/gateway/* client,
// HTML escaping, clipboard, tag-input chips, and the model selector
// datalist. Cross-origin pages are rejected with 403 and network
// failures get the same hint, since both mean the caller is not this
// dashboard.

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
