// Connect panel: one row per AuthBinding::ApiKey HTTP integration, mirroring
// `aifuel auth list` for source status and `aifuel auth set-key`/`remove`
// for mutations. A key travels only inside its POST body to this loopback
// server; it is never rendered back into the page.

function connectBadges(entry) {
  if (entry.stored) return '<span class="badge live">Key stored</span>';
  if (entry.env_set && entry.env_var) {
    return `<span class="badge connect-env">env ${esc(entry.env_var)} set</span>`;
  }
  return '<span class="badge">No credential</span>';
}

function connectControls(entry) {
  if (!entry.accepts_key) {
    return `<div class="connect-hint">Reads its key from ${esc(entry.env_var)} - export it in your shell.</div>`;
  }
  return `<form class="connect-form" data-id="${esc(entry.id)}" onsubmit="connectSubmit(event); return false">
    <input class="connect-input" type="password" name="key" autocomplete="off" spellcheck="false"
           placeholder="Paste API key" aria-label="API key for ${esc(entry.name)}">
    <button class="btn" type="submit">Save key</button>
    ${entry.stored
      ? `<button class="btn connect-remove" type="button" data-credential="${esc(entry.credential)}"
                 onclick="connectRemove(this)">Remove</button>`
      : ""}
  </form>`;
}

function connectRow(entry) {
  const row = document.createElement("div");
  row.className = "connect-row";
  row.innerHTML = `<div class="connect-info">
      <div class="connect-name">${esc(entry.name)} <span class="connect-id">${esc(entry.id)}</span></div>
      <div class="badges">${connectBadges(entry)}</div>
      <div class="connect-source">${esc(entry.source)}</div>
    </div>
    ${connectControls(entry)}`;
  return row;
}

function connectMessage(html) {
  const msg = document.getElementById("connect-msg");
  msg.innerHTML = html;
  msg.hidden = !html;
}

async function connectPost(path, body) {
  const r = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  let data = {};
  try { data = await r.json(); } catch (_) { /* non-JSON or empty body */ }
  if (!r.ok || data.error) throw new Error(data.error || "HTTP " + r.status);
  return data;
}

async function loadConnect() {
  const section = document.getElementById("connect");
  const list = document.getElementById("connect-list");
  try {
    const r = await fetch("/api/auth");
    let data = {};
    try { data = await r.json(); } catch (_) { /* handled below */ }
    if (!r.ok || data.error) throw new Error(data.error || "HTTP " + r.status);
    const entries = data.integrations || [];
    list.innerHTML = "";
    if (entries.length === 0) {
      list.innerHTML = '<div class="connect-hint">No API-key integrations are registered.</div>';
    } else {
      entries.forEach(entry => list.appendChild(connectRow(entry)));
    }
  } catch (e) {
    list.innerHTML = "";
    connectMessage(`<span class="err">Connect list failed: ${esc(String(e.message || e))}</span>`);
  }
  section.hidden = false;
}

async function connectSubmit(event) {
  event.preventDefault();
  const form = event.target;
  const input = form.elements.key;
  try {
    await connectPost("/api/auth/set-key", { integration: form.dataset.id, key: input.value });
    input.value = "";
    connectMessage(`Stored API key for ${esc(form.dataset.id)}.`);
    loadConnect();
  } catch (e) {
    connectMessage(`<span class="err">${esc(String(e.message || e))}</span>`);
  }
  return false;
}

async function connectRemove(button) {
  const credential = button.dataset.credential;
  if (!confirm(`Remove stored credential ${credential}?`)) return;
  try {
    const data = await connectPost("/api/auth/remove", { credential });
    const warnings = (data.warnings || [])
      .map(warning => `<div class="warn">${esc(warning)}</div>`)
      .join("");
    connectMessage(`Removed ${esc(credential)}.${warnings}`);
    loadConnect();
  } catch (e) {
    connectMessage(`<span class="err">${esc(String(e.message || e))}</span>`);
  }
}

loadConnect();
