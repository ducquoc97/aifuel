# Local agent MCP registration research

Checked 2026-09-17 for Wayfinder ticket "Resolve gateway configuration and agent setup contracts". Official provider documentation was used for supported configuration behavior. Local command versions are observations from the development environment, not claimed minimum supported versions.

## Local CLI observations

The current Linux development environment has these commands installed:

| Agent | Local version | MCP management observed |
| --- | --- | --- |
| Codex CLI | 0.154.0 | `codex mcp` exposes `list`, `get`, `add`, `remove`, `login`, and `logout` |
| Claude Code | 2.1.223 | `claude mcp` exposes `add`, `add-json`, `list`, `get`, `remove`, `login`, and `logout` |
| GitHub Copilot CLI | 1.0.85 | `copilot mcp` exposes `list`, `get`, `add`, `remove`, `enable`, and `disable` |
| Gemini CLI | Not installed | No local CLI or version was available to test |

Local `mcp add --help` checks describe the advertised command surface for each observed version. Codex and Copilot default to their user-level MCP configuration; Claude exposes `--scope user` (its default scope is local/project-specific). These commands were inspected but not used to modify any agent configuration. Help output is diagnostic evidence, not a compatibility or acceptance test.

These version checks and help output establish local command presence only. Real gateway registration and tool calls still need acceptance tests.

The user selected all three locally installed agents for the initial registration set: Codex CLI, Claude Code, and GitHub Copilot CLI. Gemini CLI is excluded from this initial set because it is not installed in the current environment.

## Official configuration evidence

### Codex CLI

OpenAI documents local Codex clients as MCP hosts. Codex CLI stores user-level MCP configuration in `~/.codex/config.toml`, with server entries under `mcp_servers`; it also has a CLI management command (`codex mcp add/list`). The ChatGPT desktop app, Codex CLI, and IDE extension share this host configuration. Sources: [Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli), [Codex configuration basics](https://learn.chatgpt.com/docs/config-file/config-basic).

The repository's installation requirements page lists Windows 11 via WSL2, but its README also provides a native Windows installer. The earlier WSL-only conclusion was incomplete. AI Fuel's initial WSL2 acceptance target is a chosen scope limit; native Windows Codex support is not ruled out by upstream. Numeric OS floors from conflicting documentation require reconciliation before claiming tested combinations. Sources: [Codex README and native Windows installer](https://github.com/openai/codex/blob/main/README.md), [Codex installation requirements](https://github.com/openai/codex/blob/main/docs/install.md).

### Claude Code

Anthropic documents `claude mcp add` and `claude mcp add-json`, explicit `--scope user`, and `claude mcp list/get/remove`. User-scoped servers are available across projects and stored in `~/.claude.json`. The documented CLI operating systems include macOS 13+, Windows 10 1809+/Windows Server 2019+, Ubuntu 20.04+, Debian 10+, and Alpine 3.19+. Sources: [Claude Code MCP documentation](https://code.claude.com/docs/en/mcp), [Claude Code setup requirements](https://code.claude.com/docs/en/setup).

### GitHub Copilot CLI

GitHub documents `copilot mcp add/list/get/remove` and a user-level catalog at `~/.copilot/mcp-config.json`. Its CLI can connect to local STDIO and remote HTTP/SSE servers. Official installation options include cross-platform npm (Node.js 22+), Windows WinGet, and macOS/Linux Homebrew. Sources: [Copilot CLI MCP documentation](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers), [Copilot CLI installation](https://docs.github.com/en/copilot/get-started/cli-quickstart).

## Implications for the decision

- Codex, Claude Code, and Copilot CLI are all viable initial Agent MCP Registration adapters based on official user-level MCP configuration support and local MCP management commands.
- Their user configuration formats differ: Codex TOML, Claude's user configuration, and Copilot JSON. Keep AI Fuel's catalog canonical and let each agent adapter write only its gateway registration.
- The current local Linux binaries are candidates for end-to-end acceptance; no gateway registration, handshake, or tool call was tested in this research on any platform.
- The sources describe MCP configuration forms but do not establish a universal minimum version for every format. The table contains observed versions only. Validate each adapter's configuration contract and require actual registration, gateway handshake, and a tool call before claiming a host/version/platform combination verified. Help inspection alone must not determine compatibility.
- The user selected all three installed agents. Gemini CLI was not found locally, so it is excluded from the initial set.
- The user accepted the following initial platform scope: all three agents on Linux and macOS; Claude Code and Copilot CLI on native Windows; Codex CLI on Windows through WSL2.
- The subsequent review correction replaces the earlier tested-baseline and help-gate wording with observed versions, adapter contract validation, and real gateway acceptance evidence. MCP Host identity and registration capability are independent of Agent Integration and Agent Runs.
