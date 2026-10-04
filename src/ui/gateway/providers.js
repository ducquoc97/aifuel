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
  if (p.ready === true) return ["var(--ok)", "credential or presence evidence found"];
  if (p.ready === false) return ["var(--ink-ter)", "no local evidence - set a key or sign in"];
  return ["var(--warn)", "evidence unavailable"];
}

function providerCard(p) {
  const card = document.createElement("div");
  card.className = "gw-p-card";
  const [color, dotTitle] = providerDot(p);
  const badges = ['<span class="badge gw-chat">chat</span>'];
  if (p.embeddings) badges.push('<span class="badge gw-embed">embeddings</span>');
  card.innerHTML = `
    <div class="gw-p-head">
      ${providerMonogram(p.provider)}
      <span class="gw-p-name" title="${esc(p.integration)}">${esc(p.integration)}</span>
      <span class="gw-dot" style="background:${color}" title="${dotTitle}"></span>
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
