// Gateway console entry point: connect snippets, executable providers,
// named routes, API key lifecycle, and the request log. Each
// /gateway/<section> route renders the same page with
// body[data-section] picking which section is visible; the shell loads
// every module deferred in order, and this file wires the section to
// its loader over the same-origin /api/gateway/* endpoints.

const SECTION = document.body.dataset.section || "connect";
const LOG_POLL_MS = 10000;

// ---- Section display: the route picks the visible section ----

const SECTION_LOADERS = {
  connect:   loadConnect,
  providers: loadProviders,
  routes:    loadRoutes,
  keys:      loadKeys,
  logs:      loadLogs,
};

// Each /gateway/<section> route serves this same document; only the
// section named by body[data-section] is visible and fetched.
function showSection(name) {
  if (!SECTION_LOADERS[name]) name = "connect";
  document.querySelectorAll(".gw-section").forEach(s => {
    s.hidden = s.id !== "gw-" + name;
  });
  SECTION_LOADERS[name]();
}

// ---- Init ----

// The header refresh button reloads only the section this route shows,
// plus the shared model suggestions.
async function refreshSection() {
  const btn = document.getElementById("refresh-btn");
  btn.classList.add("loading");
  btn.setAttribute("aria-busy", "true");
  await Promise.allSettled([SECTION_LOADERS[SECTION](), ensureModelIds(true)]);
  btn.classList.remove("loading");
  btn.removeAttribute("aria-busy");
  markUpdated();
}

initConnectStatic();
attachTagInput(document.getElementById("create-key-tags"));
ensureModelIds();
showSection(SECTION);
// The log poll only fires on the Logs route while the tab is
// foregrounded - other sections stay on explicit refresh.
setInterval(() => {
  if (SECTION === "logs" && !document.hidden) loadLogs();
}, LOG_POLL_MS);
