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
