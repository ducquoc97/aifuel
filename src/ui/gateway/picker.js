// ---- Selector picker: integration -> model -> reasoning effort ----
// Route forms once took one free-text selector string. This cascade
// splits it into the three parts a request selects independently:
// which integration runs it, which of that integration's models, and
// which advertised reasoning level. Options come from the same
// /api/gateway/models response cached by ensureModelIds (shared.js):
// ids without "/" are integrations, ids under "i/" are that
// integration's catalog models, and a model entry's `reasoning` list
// fills the effort select. A trailing "custom selector..." agent keeps
// raw selectors (route names, planner chains, unpinned efforts)
// enterable where the cascade cannot express them.

const PICKER_RAW = "__raw";
let pickerSeq = 0;

// Integration-level entries in the model list - ids without a slash.
function pickerIntegrations() {
  return MODEL_IDS.filter(id => !id.includes("/"));
}

// Catalog entries pinned under one integration ("devin/...").
function pickerModels(integration) {
  const prefix = integration + "/";
  return MODEL_ENTRIES.filter(e => e.id && e.id.startsWith(prefix));
}

// selectorPicker({stack, rawPlaceholder, onchange}) -> { el, value, setValue, focus }
// `stack` renders fields vertically for the narrow graph nodes;
// `onchange` fires whenever any field changes (used to promote a
// ghost step row into a real one).
function selectorPicker(opts = {}) {
  const dlId = `gw-pick-dl-${++pickerSeq}`;
  const root = document.createElement("div");
  root.className = "gw-picker" + (opts.stack ? " gw-picker-stack" : "");

  const agent = document.createElement("select");
  agent.className = "gw-select gw-picker-agent";
  agent.setAttribute("aria-label", "Agent integration");

  const model = document.createElement("input");
  model.className = "gw-input gw-mono gw-picker-model";
  model.placeholder = "model (blank = default)";
  model.autocomplete = "off";
  model.setAttribute("list", dlId);
  model.setAttribute("aria-label", "Model");

  const effort = document.createElement("select");
  effort.className = "gw-select gw-picker-effort";
  effort.setAttribute("aria-label", "Reasoning effort");

  const raw = document.createElement("input");
  raw.className = "gw-input gw-mono gw-picker-raw";
  raw.placeholder = opts.rawPlaceholder || "selector - e.g. auto, cheap, codex/gpt-5@high";
  raw.autocomplete = "off";
  raw.setAttribute("aria-label", "Raw selector");
  raw.hidden = true;

  // The planner is the only "agent" that is not a real integration -
  // explain it inline since a select option cannot carry detail.
  const autoNote = document.createElement("div");
  autoNote.className = "gw-picker-note";
  autoNote.hidden = true;
  autoNote.textContent = "auto - the gateway plans a provider per request, so the target can change; the Logs tab shows what actually ran.";

  const dl = document.createElement("datalist");
  dl.id = dlId;
  root.append(agent, model, effort, raw, autoNote, dl);

  if (opts.onchange) {
    root.addEventListener("input", opts.onchange);
    root.addEventListener("change", opts.onchange);
  }

  // Custom mode swaps the model/effort pair for one free-text input.
  function syncMode() {
    const isRaw = agent.value === PICKER_RAW;
    model.hidden = effort.hidden = isRaw;
    raw.hidden = !isRaw;
    autoNote.hidden = agent.value !== "auto";
  }

  // The model datalist and effort select both follow the agent.
  function syncModel() {
    dl.innerHTML = pickerModels(agent.value)
      .map(e => `<option value="${esc(e.id.slice(agent.value.length + 1))}"></option>`).join("");
    syncEffort();
  }

  // Effort options are exactly the model's advertised `reasoning`
  // list; without one the provider default runs, so the select
  // disables rather than inventing spellings.
  function syncEffort() {
    const mid = model.value.trim();
    const entry = mid && MODEL_ENTRIES.find(e => e.id === agent.value + "/" + mid);
    const list = entry && Array.isArray(entry.reasoning) ? entry.reasoning : [];
    const def = entry && entry.default_reasoning;
    effort.innerHTML = `<option value="">${def ? `default (${esc(def)})` : "default"}</option>`
      + list.map(v => `<option value="${esc(v)}">${esc(v)}</option>`).join("");
    effort.disabled = !list.length;
    effort.title = list.length
      ? "Reasoning effort passed to the provider"
      : "No advertised reasoning levels - the provider default runs";
  }

  agent.addEventListener("change", () => { syncMode(); syncModel(); });
  model.addEventListener("input", syncEffort);

  // Refill agent options once model data lands; preserve the current
  // choice when it still exists.
  function refresh() {
    const cur = agent.value;
    agent.innerHTML = pickerIntegrations()
      .map(id => `<option value="${esc(id)}">${id === "auto" ? "auto (gateway picks)" : esc(id)}</option>`).join("")
      + `<option value="${PICKER_RAW}">custom selector…</option>`;
    if (cur && [...agent.options].some(o => o.value === cur)) agent.value = cur;
    syncMode();
    syncModel();
  }

  function api() {
    return {
      el: root,
      refresh,
      focus: () => agent.focus(),
      // Assembled selector: `agent/model@effort`, dropping empty parts.
      value() {
        if (agent.value === PICKER_RAW) return raw.value.trim();
        const m = model.value.trim();
        const e = effort.disabled ? "" : effort.value;
        return agent.value + (m ? "/" + m : "") + (e ? "@" + e : "");
      },
      // Prefill from a stored selector: strip the @effort suffix
      // (backend splits on the last @), land the head on an agent
      // option when one matches, else fall back to raw text.
      setValue(sel) {
        sel = (sel || "").trim();
        if (!sel) {
          agent.selectedIndex = 0;
          syncMode();
          syncModel();
          return;
        }
        const at = sel.lastIndexOf("@");
        const eff = at > 0 && at < sel.length - 1 ? sel.slice(at + 1) : "";
        const base = at > 0 ? sel.slice(0, at) : sel;
        const slash = base.indexOf("/");
        const head = slash === -1 ? base : base.slice(0, slash);
        const rest = slash === -1 ? "" : base.slice(slash + 1);
        if (pickerIntegrations().includes(head)) {
          agent.value = head;
          syncMode();
          syncModel();
          model.value = rest;
          syncEffort();
          if (eff) {
            if (![...effort.options].some(o => o.value === eff)) effort.add(new Option(eff, eff));
            effort.disabled = false;
            effort.value = eff;
          }
        } else {
          agent.value = PICKER_RAW;
          syncMode();
          raw.value = sel;
        }
      },
    };
  }

  refresh();
  // Model data often lands after the picker mounts; refill once loaded.
  ensureModelIds().then(refresh);
  return api();
}
