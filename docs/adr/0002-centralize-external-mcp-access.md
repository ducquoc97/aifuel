# Centralize external MCP access through an AI Fuel gateway

Configure external MCP servers once in a per-user AI Fuel catalog and expose them to selected local agents through an MCP Gateway, instead of copying external server definitions into every agent. Keep the gateway separate from AI Fuel's read-only monitoring server because external tools may have effects beyond monitoring. Project-scoped catalogs are outside the initial scope. Transport coverage and authentication remain to be decided.

Store the central catalog as JSON, matching the common configuration format used by MCP hosts and AI Fuel's existing JSON support. It contains external server definitions, shared defaults, and exact per-agent selections.

Represent the catalog with a `servers` map keyed by server id, a shared `defaults` list of server ids, and an `agents` map keyed by Agent Integration id. An absent agent entry inherits the defaults; an explicit agent entry's `servers` list replaces them, including an empty list that selects no servers. Transport-specific fields in server definitions follow the gateway protocol contract.

Validate that every server id referenced from defaults or an agent selection exists in the server map. A dangling reference invalidates the catalog with an actionable error; the affected gateway does not start with a silently reduced server set.

The catalog management command refuses to remove a server still referenced by defaults or any agent selection. The user must first update the selections, avoiding a hidden change in which tools an agent receives.

Place the catalog at `aifuel/mcp.json` within the operating system's standard per-user application configuration directory. This gives each user one catalog shared across selected agents and keeps it separate from provider credential sources.

Allow users to edit the JSON catalog directly and provide `aifuel` commands to validate and manage its server entries. The file remains the canonical shared configuration for all selected agents.

Keep `aifuel mcp` as the existing read-only monitoring server. Add `aifuel mcp gateway --agent <id>` for an agent's gateway process; `aifuel mcp servers list|add|remove|validate` for catalog management; and `aifuel mcp setup --agent <id>` for Agent MCP Registration. Setup applies by default, `--remove` removes the registration, and `--dry-run` previews either operation without writing. Use the managed registration name `aifuel-gateway`.

Each gateway process loads a configuration snapshot at startup. Catalog edits apply to the next gateway process; after applying an agent registration, the user restarts that agent so it reads the new configuration.

The first Agent MCP Registration adapters cover the locally installed Codex CLI, Claude Code, and GitHub Copilot CLI. Gemini CLI is not part of this initial registration set. Platform and version support must be gated by documented provider support and tested configuration behavior.

Initial platform scope follows provider support: all three agents on Linux and macOS; Claude Code and Copilot CLI on native Windows; Codex CLI on Windows through WSL2. Treat the locally observed versions as tested baselines, and require both documented platform support and a successful registration capability check before claiming a combination supported.

Record the locally tested baselines as Codex CLI 0.154.0, Claude Code 2.1.223, and GitHub Copilot CLI 1.0.85. At setup time, require a successful `mcp add --help` capability probe; for Claude Code, require the explicit user-scope option. Codex and Copilot's add commands default to user-level configuration. Do not infer compatibility from a version number alone. Report versions without tested acceptance as unverified.

Setup re-reads and validates current agent configuration before each operation. Applying setup recomputes the change from the current contents. If the configuration changes during the write, abort instead of overwriting the concurrent edit. `--dry-run` reports the plan without writing; a previous preview is informational and is not a saved transaction.

Registration is idempotent when the existing gateway entry matches the AI Fuel-managed definition. If the gateway name already has different settings, report a conflict and preserve the file. Removal targets only an unchanged entry matching the AI Fuel-managed definition; edited or user-owned entries are preserved and reported as conflicts.

Each agent starts its own gateway process using the central configuration. This avoids background service management and keeps upstream connections and protocol sessions separate between agents. Local external servers may consequently run once per gateway; sharing their configuration does not imply sharing their running processes.

AI Fuel manages external server configuration only: remote endpoints and existing local commands. Users supply any required executables and runtimes; installing or updating external server packages is outside this design.

Use a shared default set of external MCP servers with optional per-agent selections. Server definitions remain centralized even when agents select different sets. An explicit agent selection replaces the default set completely: no override inherits defaults, while an empty override selects no servers. This makes an overridden agent's server access independent of later additions to the default set.

The setup command applies Agent MCP Registration changes for selected agents by default; `--dry-run` previews the same operation without writing. Agent-specific configuration adapters own file formats and registration rules; ordinary discovery and monitoring retain their existing policy against credential writes. This decision authorizes the design of that command, not changes to local agent configurations during this interview.
