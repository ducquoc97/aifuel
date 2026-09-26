# ⛽ aifuel

**The fuel gauge for your AI coding subscriptions - and one interface for spending them.**

You're paying for Claude Code, Codex, Copilot, Gemini, Antigravity, Devin... so `aifuel` answers the two questions that matter: *which limit runs out first?* and *which subscription should take this prompt?*

- **Know what's left.** `aifuel` reads each provider's own usage endpoint and shows the **quota you have left** - in one dashboard, ranked by whichever window **resets soonest**, with a live countdown to every refill.
- **Spend it deliberately.** `aifuel run` sends a prompt through any installed provider CLI - one uniform, non-interactive interface with explicit model and permission selection, resumable sessions, local approvals, and `text|json|jsonl` output for scripts.
- **Wire it into your agents.** Three MCP servers expose quota status, programmatic run management, and a gateway that fronts your external MCP servers - so every host gets the same tools from one config.

One native binary. Runs on **Windows, Linux, and macOS** in your browser, terminal, or an MCP host.

![Platforms: Windows · Linux · macOS](https://img.shields.io/badge/platform-Windows%20%C2%B7%20Linux%20%C2%B7%20macOS-8a94a6)
![Native binary](https://img.shields.io/badge/runtime-native%20binary-3ddc97)

![aifuel dashboard](docs/aifuel.png)

## Install

**Linux / macOS** (and Windows via WSL or Git Bash):

```bash
./scripts/install.sh                 # installs `aifuel` into ~/.local/bin
./scripts/install.sh --uninstall     # remove it
```

Override the target dir with `BIN_DIR=/usr/local/bin ./scripts/install.sh`.

**Windows** (PowerShell):

```powershell
.\scripts\install.ps1                # installs aifuel.exe into ~\.local\bin (+ adds it to PATH)
.\scripts\install.ps1 -Uninstall     # remove it
```

Override the target dir with `.\scripts\install.ps1 -BinDir 'C:\tools\bin'`. Open a new terminal after install.

The installers need Rust and Cargo at install time; the installed command has no runtime requirement. You can also build directly with `cargo build --release -p aifuel` and use `target/release/aifuel`.

## Commands

| Command | What you get |
|---|---|
| `aifuel` | Auto-refreshing **web dashboard** at `http://127.0.0.1:8787` (opens a browser) |
| `aifuel --no-browser` | Dashboard without opening a browser; `--host`/`--port` change the bind |
| `aifuel --text` | Compact **colored terminal** summary (great over SSH) |
| `aifuel --json` | **Normalized JSON** for scripts, status bars, and piping |
| `aifuel run --provider ID --prompt "..."` | One explicit prompt through an installed provider CLI |
| `aifuel profile list\|save\|remove` | Named defaults for `run` (provider, model, effort, access, timeout) |
| `aifuel model list\|refresh` | Cached provider model catalog, refreshed on demand |
| `aifuel approve --run ID --input ID --decision accept\|decline\|cancel` | Answer a pending permission request for a local run |
| `aifuel mcp` | Read-only MCP status server over stdio |
| `aifuel mcp execution` | MCP server that manages Agent Runs over stdio |
| `aifuel mcp gateway --agent HOST` | Selected external MCP servers, served to one agent |
| `aifuel mcp setup --agent HOST [--dry-run] [--remove]` | Apply or remove the gateway entry in an agent's own config |
| `aifuel mcp servers ...` | Manage the gateway's external server catalog |

Because `--json` is a stable, structured feed, it drops cleanly into a tmux / polybar / Sketchybar / starship status line.

## What it tracks

| Provider          | Source        | How                                                                 |
|-------------------|---------------|---------------------------------------------------------------------|
| Claude Code       | **live**      | `GET api.anthropic.com/api/oauth/usage` (token from `~/.claude/.credentials.json`) |
| Codex CLI         | **live**      | `GET chatgpt.com/backend-api/codex/usage` (token from `~/.codex/auth.json`) |
| GitHub Copilot    | **live**      | `api.github.com/copilot_internal/user` or `api.github.com/copilot_internal/v2/token` (token from `~/.copilot/config.json`) |
| Gemini CLI        | **live**      | `:loadCodeAssist` → `:retrieveUserQuota` for real per-model bars (needs working OAuth and any required project) |
| Antigravity CLI   | **live**      | Code Assist quota via its live OAuth token sources |
| Devin CLI         | **live**      | `POST {api_server_url}/exa.seat_management_pb.SeatManagementService/GetUserStatus` (key from `credentials.toml`) |

`live` = pulled from the provider API. Any provider that cannot return live usage is shown as an error. A pinned catalog keeps 69 known provider IDs with explicit capability states; catalog-only entries are reported as unsupported rather than guessed.

## Running prompts

`aifuel run` delegates one explicit prompt to a provider CLI that is installed and signed in:

```bash
aifuel run --provider codex --model gpt-5-codex --prompt "Explain Rust ownership"
```

Provider IDs: `claude`, `codex`, `copilot`, `gemini`, `antigravity`, `devin`. Options:

- `--prompt TEXT` / `--prompt-file PATH` - prompt text or a file to read it from; omit both to read the prompt from piped stdin
- `--model ID` / `--effort LEVEL` - explicit model and effort; if a provider default is rejected, pass `--model` so nothing silently falls back
- `--profile NAME` - apply a saved profile (`aifuel profile save NAME --provider ID --model ID [--effort LEVEL] [--access MODE] [--timeout SECONDS]`)
- `--account ID` - explicit account
- `--access read-only|workspace-write` - permission profile (default: read-only)
- `--working-directory PATH` (or `--cwd`) - project directory for the run
- `--resume SESSION_ID` - continue a known session; explicit `--model`/`--effort` override stored values
- `--external-tool NAME` - allow one exact gateway tool (repeatable)
- `--output text|json|jsonl` - result format (default: text)
- `--timeout 30s|10m|1h` - optional deadline; `0` means none

Exit codes: `0` succeeded, `2` request or launch error, `3` provider has no verified agent integration, `4` run failed, `5` timed out.

Permission requests are never auto-approved. A run that asks for one prints the pending request; answer it from another terminal with `aifuel approve --run RUN_ID --input INPUT_ID --decision accept|decline|cancel`. Ordinary provider questions are answered interactively during `aifuel run` (needs a terminal), or through `answer_input` for runs owned by `aifuel mcp execution`.

## MCP

`aifuel` ships three MCP interfaces over stdio.

### Status server - `aifuel mcp`

Read-only provider status for any MCP host:

```json
{
  "mcpServers": {
    "aifuel": {
      "command": "aifuel",
      "args": ["mcp"]
    }
  }
}
```

Exposes the `get_status` tool and the `aifuel://status` resource: provider status, quota, freshness, provenance, and errors. It does not execute prompts or edit files.

### Execution server - `aifuel mcp execution`

Lets one MCP host connection own and manage Agent Runs with the tools `list_agents`, `list_models`, `resolve_run`, `start_run`, `resume_session`, `answer_input`, `get_run`, `get_result`, `cancel_run`, and `read_events`. Permission approvals stay local-only through `aifuel approve`; the server cannot grant them.

### Gateway - `aifuel mcp gateway`

Exposes the tools, resources, resource templates and subscriptions, prompts, and argument completion of external MCP servers - configured once in `aifuel/mcp.json` in your user config directory - to each agent:

- Windows: `%APPDATA%/aifuel/mcp.json`
- macOS: `~/Library/Application Support/aifuel/mcp.json`
- Linux: `$XDG_CONFIG_HOME/aifuel/mcp.json`, or `~/.config/aifuel/mcp.json`

```json
{
  "servers": {
    "local-docs": {
      "transport": "stdio",
      "command": "/absolute/path/to/mcp-server",
      "args": []
    },
    "remote-docs": {
      "transport": "streamable-http",
      "url": "https://mcp.deepwiki.com/mcp"
    },
    "private-docs": {
      "transport": "streamable-http",
      "url": "https://example.com/mcp",
      "auth": { "bearerTokenEnv": "PRIVATE_MCP_TOKEN" },
      "secretHeaders": {
        "X-API-Key": { "env": "PRIVATE_MCP_API_KEY" }
      }
    }
  },
  "defaults": [],
  "agents": {
    "codex": { "servers": ["remote-docs"] }
  }
}
```

A host with no `agents` entry inherits `defaults`. Manage the catalog without editing JSON by hand:

```bash
aifuel mcp servers list
aifuel mcp servers validate
aifuel mcp servers add SERVER_ID --definition FILE
aifuel mcp servers remove SERVER_ID
aifuel mcp servers select --defaults [SERVER_ID ...]
aifuel mcp servers select --agent HOST [SERVER_ID ...]
aifuel mcp servers select --agent HOST --inherit
```

For local `stdio` servers, `env` sets literal values and `envFrom` names environment variables read from the gateway process at startup; the child gets a small platform allowlist plus those configured values. For remote servers, endpoints must use HTTPS (plain HTTP only on loopback) and support MCP 2025-11-25. `bearerTokenEnv` and `secretHeaders` reference environment variable *names* only - values are resolved once at startup, sent only to the configured endpoint, and never written to `mcp.json`, registrations, or logs. A missing or empty value fails that server. Redirects are rejected, so configure the final URL.

Then connect an agent. `aifuel mcp setup --agent HOST` applies the managed gateway entry to the host's own config file (with backup and dry-run support) and `--remove` detaches it; or register manually, e.g. in `~/.codex/config.toml`:

```toml
[mcp_servers.aifuel-gateway]
command = "aifuel"
args = ["mcp", "gateway", "--agent", "codex"]
env_vars = ["XDG_CONFIG_HOME"]
```

Host IDs: `codex`, `claude`, `copilot`, `gemini`, `antigravity`, `devin`. Restart the host after changing its registration. Repeatable `--tool NAME` on `mcp gateway` restricts which gateway tools a host sees. If a host filters child-process environment variables, forward the variables your servers reference in that host's registration.

## How it works (and what it touches)

- Credentials are read **locally only**, to authenticate each provider's own usage endpoint - the same files your CLIs already use. Tokens are never printed, and are only ever sent to the provider they belong to.
- Before each collection, `aifuel` checks for each provider's own local credential source and initializes only the providers it finds. Discovery never calls an API, refreshes a token, or writes credentials.
- If a local discovery check fails, other providers still load. JSON reports the failure in `discovery_errors`, and one-shot commands return a nonzero exit status after printing available results.
- Expired credentials are reported as unavailable; collection never refreshes or writes them.
- Results are cached in-process for 300 seconds unless a refresh is requested; the dashboard auto-refreshes every 5 minutes and countdowns tick every second client-side.
- Ordering: each provider uses its authoritative weekly/monthly window when available, otherwise the soonest reported reset, with depleted providers last.

## FAQ

**Does this send my tokens anywhere?** No. It reads the same local credential files your CLIs already use, calls each provider's *own* usage endpoint, and shows you the result. There is no server, no telemetry, no third party.

**Do I need API keys?** No. It reuses the OAuth/login your AI coding CLIs already set up. A general GitHub CLI login does not count as a GitHub Copilot login.

**It only shows some providers.** It shows providers with their own local credential source. Log in to that provider's AI coding CLI, then refresh the dashboard.

**Why not just check each dashboard?** Because six tabs don't tell you which limit you'll hit first. `aifuel` does - at a glance, on every OS.
