// App shell: sidebar chrome shared by every page - active nav state,
// search filter, collapse, mobile drawer, Quit, and the shared toast
// helper pages call after mutations.

const app = document.querySelector(".app");
const sidebar = document.getElementById("sidebar");
const scrim = document.getElementById("scrim");

// ---- Active nav item (server marks body[data-active]) ----

const activeKey = document.body.dataset.active;
if (activeKey) {
  const item = document.querySelector(`.nav-item[data-nav="${activeKey}"]`);
  if (item) {
    item.classList.add("active");
    item.setAttribute("aria-current", "page");
  }
}

// ---- Collapse (desktop) ----

const collapseBtn = document.getElementById("collapse-btn");
const COLLAPSED_KEY = "aifuel.sidebar.collapsed";

function setCollapsed(collapsed) {
  app.classList.toggle("sidebar-collapsed", collapsed);
  collapseBtn.setAttribute("aria-expanded", String(!collapsed));
  collapseBtn.setAttribute("aria-label",
    collapsed ? "Expand sidebar" : "Collapse sidebar");
  localStorage.setItem(COLLAPSED_KEY, collapsed ? "1" : "0");
}

setCollapsed(localStorage.getItem(COLLAPSED_KEY) === "1");
collapseBtn.addEventListener("click", () =>
  setCollapsed(!app.classList.contains("sidebar-collapsed")));

// ---- Mobile drawer ----

function setDrawer(open) {
  app.classList.toggle("sidebar-open", open);
  scrim.hidden = !open;
}

document.getElementById("menu-btn").addEventListener("click", () => setDrawer(true));
scrim.addEventListener("click", () => setDrawer(false));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") setDrawer(false);
});
// A nav tap on mobile should close the drawer before following the link.
document.getElementById("nav").addEventListener("click", (e) => {
  if (e.target.closest(".nav-item")) setDrawer(false);
});

// ---- Nav search filter ----

const searchInput = document.getElementById("nav-search");

function filterNav(query) {
  const q = query.trim().toLowerCase();
  const items = [...document.querySelectorAll(".nav-item")];
  const groups = [...document.querySelectorAll(".nav-group")];
  for (const item of items) {
    const hit = !q || item.textContent.toLowerCase().includes(q);
    item.classList.toggle("nav-hidden", !hit);
  }
  for (const group of groups) {
    // Hide a group label when nothing below it (until the next group or
    // the end of the nav) is still visible.
    let el = group.nextElementSibling;
    let anyVisible = false;
    while (el && !el.classList.contains("nav-group")) {
      if (el.classList.contains("nav-item") && !el.classList.contains("nav-hidden")) {
        anyVisible = true;
        break;
      }
      el = el.nextElementSibling;
    }
    group.classList.toggle("nav-hidden", !anyVisible);
  }
}

searchInput.addEventListener("input", () => filterNav(searchInput.value));
document.addEventListener("keydown", (e) => {
  // "/" focuses the nav filter, OmniRoute-style. Skip while typing in a field.
  if (e.key === "/" && !/^(input|textarea|select)$/i.test(document.activeElement.tagName)) {
    e.preventDefault();
    searchInput.focus();
  }
});

// ---- Quit: stop the dashboard server ----

document.getElementById("quit-btn").addEventListener("click", async () => {
  if (!confirm("Stop the aifuel server? The dashboard and /v1 gateway will shut down.")) return;
  try {
    await fetch("/api/shutdown", { method: "POST" });
  } catch (_) { /* the socket dies with the server - that is the point */ }
  document.querySelector(".content").innerHTML =
    '<div class="shutdown-note">aifuel has stopped. You can close this tab.</div>';
});

// ---- Shared toast helper (pages call toast(message, isErr)) ----

function toast(message, isErr) {
  const el = document.createElement("div");
  el.className = "toast" + (isErr ? " err" : "");
  el.setAttribute("role", "status");
  el.textContent = message;
  document.getElementById("toasts").appendChild(el);
  setTimeout(() => {
    el.classList.add("gone");
    setTimeout(() => el.remove(), 300);
  }, 5000);
}
