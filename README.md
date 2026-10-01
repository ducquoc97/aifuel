# ⛽ aifuel

**The fuel gauge for your AI coding subscriptions - and one interface for spending them.**

You pay for Claude Code, Codex, Copilot, Gemini, Antigravity, Devin... `aifuel` answers the two questions that matter: *which limit runs out first?* and *which subscription should take this prompt?*

- **Know what's left.** Reads each provider's own usage endpoint and shows the quota you have left in one dashboard, ranked by soonest reset, with a live countdown to every refill.
- **Spend it deliberately.** `aifuel run` sends a prompt through any installed provider CLI - explicit model and permission selection, resumable sessions, local approvals, `text|json|jsonl` output for scripts.
- **Wire it into your agents.** Three MCP servers expose quota status, run management, and a gateway that fronts your external MCP servers - so every host gets the same tools from one config.

One native binary. **Windows, Linux, macOS** - browser, terminal, or MCP host.

![Platforms: Windows · Linux · macOS](https://img.shields.io/badge/platform-Windows%20%C2%B7%20Linux%20%C2%B7%20macOS-8a94a6)
![Native binary](https://img.shields.io/badge/runtime-native%20binary-3ddc97)

![aifuel dashboard](docs/aifuel.png)

## Install

**Linux / macOS** (Windows via WSL or Git Bash):

```bash
./scripts/install.sh                 # installs `aifuel` into ~/.local/bin
./scripts/install.sh --uninstall     # remove it
```

**Windows** (PowerShell):

```powershell
.\scripts\install.ps1                # installs aifuel.exe into ~\.local\bin (+ PATH)
.\scripts\install.ps1 -Uninstall     # remove it
```

Override the target dir with `BIN_DIR=/usr/local/bin` (or `-BinDir` on Windows). The installers need Rust and Cargo; the installed command has no runtime requirement. Or build directly: `cargo build --release -p aifuel`.

## Commands

| Command | What you get |
|---|---|
| `aifuel` | **Web dashboard** at `http://127.0.0.1:8787` (opens a browser; optional auto-refresh) |
| `aifuel --no-browser` | Dashboard without opening a browser; `--host`/`--port` change the bind (a non-loopback `--host` also exposes the Connect form's key submission to clients that can reach the port) |
| `aifuel --text` | Compact **colored terminal** summary (great over SSH) |
| `aifuel --json` | **Normalized JSON** for scripts, status bars, and piping |
| `aifuel run --provider ID --prompt "..."` | One explicit prompt through an installed provider CLI |
| `aifuel profile list\|save\|remove` | Named defaults for `run` (provider, model, effort, access, timeout) |
| `aifuel model list\|refresh` | Cached provider model catalog, refreshed on demand |
| `aifuel auth list\|set-key\|remove` | Stored API keys for API-key integrations (also via the dashboard's Connect section) |
| `aifuel approve --run ID --input ID --decision accept\|decline\|cancel` | Answer a pending permission request for a local run |
| `aifuel mcp` | Read-only MCP status server over stdio |
| `aifuel mcp execution` | MCP server that manages Agent Runs over stdio |
| `aifuel mcp gateway --agent HOST` | Selected external MCP servers, served to one agent |
| `aifuel mcp setup --agent HOST [--dry-run] [--remove]` | Apply or remove the gateway entry in an agent's own config |
| `aifuel mcp servers ...` | Manage the gateway's external server catalog |
| `aifuel runtime` | JSON-RPC stdio bridge for embedding agent runs in a host process |

`--json` is a stable structured feed - it drops cleanly into a tmux / polybar / Sketchybar / starship status line.

## What it tracks

| Provider | Source | Reads credentials from |
|---|---|---|
| Claude Code | **live** | `~/.claude/.credentials.json` |
| Codex CLI | **live** | `~/.codex/auth.json` |
| GitHub Copilot | **live** | `~/.copilot/config.json` |
| Gemini CLI | **live** | Code Assist OAuth token |
| Antigravity CLI | **live** | Code Assist OAuth token |
| Devin CLI | **live** | `credentials.toml` key |

`live` = pulled from the provider's own API; a provider that cannot return live usage shows as an error, never a guess. A pinned catalog covers 69 provider IDs - catalog-only entries report as unsupported.

## Running prompts

```bash
aifuel run --provider codex --model gpt-5-codex --prompt "Explain Rust ownership"
```

Integration IDs (`--provider` is an alias for `--integration`): `claude`, `codex`, `copilot`, `gemini`, `antigravity`, `devin`, `opencode`, `cursor`, `ollama:local`, `lmstudio:local`, `openai:api-key`, `openrouter:api-key`. Options:

- `--prompt TEXT` / `--prompt-file PATH` - or omit both to pipe the prompt on stdin
- `--model ID` / `--effort LEVEL` - explicit model and effort
- `--profile NAME` - apply a saved profile (`aifuel profile save NAME --provider ID --model ID [--effort LEVEL] [--access MODE] [--timeout SECONDS]`)
- `--account ID` - explicit account
- `--access read-only|workspace-write|full` - permission profile (default: read-only)
- `--working-directory PATH` (or `--cwd`) - project directory for the run
- `--resume SESSION_ID` - continue a known session (`session_id` or `local_session_id` from a run's output); explicit `--model`/`--effort` override stored values
- `--external-tool NAME` - allow one exact gateway tool (repeatable)
- `--output text|json|jsonl` - result format (default: text)
- `--timeout 30s|10m|1h` - optional deadline; `0` means none

Exit codes: `0` succeeded, `2` request or launch error, `3` provider has no verified agent integration, `4` run failed, `5` timed out.

Permission requests are never auto-approved: a run that asks for one prints the pending request - answer it with `aifuel approve --run RUN_ID --input INPUT_ID --decision accept|decline|cancel`. Ordinary provider questions answer interactively during `aifuel run`, or through `answer_input` for runs owned by `aifuel mcp execution`.

## MCP

Three stdio servers:

| Server | Purpose |
|---|---|
| `aifuel mcp` | Read-only status: `get_status` tool and `aifuel://status` resource |
| `aifuel mcp execution` | Owns Agent Runs: `list_agents`, `list_models`, `resolve_run`, `start_run`, `resume_session`, `answer_input`, `get_run`, `get_result`, `cancel_run`, `read_events` |
| `aifuel mcp gateway --agent HOST` | Serves your external MCP servers' tools, resources, and prompts to one agent |

Register the status server on any host:

```json
{ "mcpServers": { "aifuel": { "command": "aifuel", "args": ["mcp"] } } }
```

Permission approvals stay local-only through `aifuel approve` - the execution server cannot grant them.

### Gateway config

Servers are configured once in `aifuel/mcp.json` in your user config directory (`%APPDATA%/aifuel` on Windows, `~/Library/Application Support/aifuel` on macOS, `$XDG_CONFIG_HOME/aifuel` or `~/.config/aifuel` on Linux):

```json
{
  "servers": {
    "local-docs": { "transport": "stdio", "command": "/absolute/path/to/mcp-server", "args": [] },
    "remote-docs": { "transport": "streamable-http", "url": "https://mcp.deepwiki.com/mcp" }
  },
  "defaults": [],
  "agents": {
    "codex": { "servers": ["remote-docs"] }
  }
}
```

A host with no `agents` entry inherits `defaults`. Manage the catalog without editing JSON:

```bash
aifuel mcp servers list|validate
aifuel mcp servers add SERVER_ID --definition FILE | remove SERVER_ID
aifuel mcp servers select --defaults [SERVER_ID ...] | --agent HOST [SERVER_ID ...] | --agent HOST --inherit
```

Trust rules: remote endpoints must use HTTPS (plain HTTP only on loopback) and speak MCP 2025-11-25. `bearerTokenEnv` and `secretHeaders` name environment variables only - values resolve once at startup (a missing or empty value fails that server), are sent only to the configured endpoint, and are never written to `mcp.json` or logs. Redirects are rejected, so configure the final URL. For stdio servers, `env` sets literals and `envFrom` names variables read at startup; children get a small platform allowlist plus what you configure.

Then connect an agent: `aifuel mcp setup --agent HOST` writes the managed entry into the host's config (backup and `--dry-run` supported, `--remove` detaches). Host IDs: `codex`, `claude`, `copilot`, `gemini`, `antigravity`, `devin`. Restart the host after changing its registration. Repeatable `--tool NAME` on `mcp gateway` restricts which tools a host sees.

## How it works

- Credentials are read **locally only** - the same files your CLIs already use. Tokens are never printed and are sent only to their own provider's usage endpoint.
- Discovery is filesystem-only: it never calls an API, refreshes a token, or writes credentials. One provider's failure doesn't block the rest - JSON reports it in `discovery_errors`.
- Results cache in-process for 300s unless a refresh is requested; the dashboard's 5-minute auto-refresh is opt-in and countdowns tick every second.
- Providers rank by their authoritative weekly/monthly window, else soonest reset; depleted providers sort last.

## FAQ

**Does this send my tokens anywhere?** No. It reads the same local credential files your CLIs use, calls each provider's *own* usage endpoint, and shows the result. No server, no telemetry, no third party.

**Do I need API keys?** Not for monitoring - it reuses the OAuth/logins your AI coding CLIs already set up. `aifuel run` through an API-key integration (`openai:api-key`, `openrouter:api-key`) does need one via `aifuel auth set-key`, the dashboard's Connect section, or the env var. A general GitHub CLI login does not count as a GitHub Copilot login.

**It only shows some providers.** Those are the ones with local credentials. Log in to that provider's AI coding CLI, then refresh.

**Why not just check each dashboard?** Six tabs don't tell you which limit you'll hit first - `aifuel` does, at a glance.
