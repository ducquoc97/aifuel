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
