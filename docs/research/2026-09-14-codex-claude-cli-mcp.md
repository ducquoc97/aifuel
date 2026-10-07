# Codex CLI and Claude Code contracts for read-only delegation

Date: 2026-09-14  
Issue: [#10](https://github.com/ducquoc97/aifuel/issues/10)  
Scope: research only. MCP is limited to read-only AI Fuel status, model, and quota monitoring. Starting provider prompts or agent runs through MCP is out of scope. This note records evidence and failure conditions; it does not select AI Fuel's final architecture or permission policy.

## Evidence boundary

- Local CLI checks were help/version checks only. No inference was launched and no credential contents were read.
- Local versions observed on 2026-09-14: `codex-cli 0.154.0` and `Claude Code 2.1.223`.
- Codex source was checked at the official `rust-v0.154.0` tag. The matching release is [rust-v0.154.0](https://github.com/openai/codex/releases/tag/rust-v0.154.0).
- Claude Code's implementation is distributed as the official CLI binary. The official CLI, headless, permissions, MCP, authentication, cost, and error documentation are the primary sources used here.

## Short findings

1. Both CLIs expose documented noninteractive entry points and machine-readable output.
2. Codex has the clearer provider-enforced read-only execution contract: `codex exec --sandbox read-only --ask-for-approval never`. Its current documentation says `codex exec` is read-only by default, and its source exposes the sandbox, approval, JSONL, output-schema, and ephemeral flags.
3. Claude Code `--permission-mode plan` blocks source edits but intentionally permits read-only exploration and shell commands. A stronger no-write invocation must also restrict built-in tools, for example with `--tools "Read,Glob,Grep"`; `--tools` does not restrict MCP tools, so `--disallowedTools "mcp__*"` is a separate control. Plan mode alone is not a proof that every shell command is harmless.
4. Neither main CLI exposes a general wall-clock timeout flag in the current contracts. Claude documents process cancellation: SIGTERM exits 143 and SIGINT ends the turn. Codex handles Ctrl-C by sending `turn/interrupt`; its app-server protocol also exposes `turn/interrupt`. A caller that needs a wall-clock deadline must use an external supervisor and classify forced termination separately from provider failure.
5. Auth mode changes the meaning of quota and cost. ChatGPT-managed Codex usage is not the same account surface as Codex API-key usage. Claude subscription OAuth usage is not the same billing surface as `ANTHROPIC_API_KEY` Console usage. A per-run token/cost estimate is not an account quota or invoice.
6. Provider CLIs are MCP clients. A read-only `aifuel mcp` would be a server exposing status/model/quota data only. MCP must not be used here to launch Codex or Claude prompts. The client cannot prove that an arbitrary server is read-only; the server's exposed operations must enforce that boundary.

## Contract matrix

| Concern | Codex CLI | Claude Code CLI |
| --- | --- | --- |
| Noninteractive form | `codex exec [PROMPT]`; prompt can come from stdin, with piped input appended when a prompt is also supplied. | `claude -p "query"`; prompt content can be piped on stdin. |
| Model | `-m, --model <MODEL>`. | `--model <alias-or-full-id>`, including aliases such as `sonnet`, `opus`, and `haiku`. |
| Structured final output | `--output-schema <FILE>` requests a JSON Schema-shaped final message; `-o, --output-last-message <FILE>` writes the final message. | `--output-format json` plus `--json-schema`; structured data is in `structured_output`, with session and usage metadata. |
| Streaming | `--json` emits JSONL events to stdout. Current event types include `thread.started`, `turn.started`, `turn.completed`, `turn.failed`, `item.*`, and `error`. | `--output-format stream-json --verbose --include-partial-messages`; each line is an event and the final line is a `result` message. |
| Sessions | `codex exec resume --last [PROMPT]` or `codex exec resume <SESSION_ID> [PROMPT]`; `--ephemeral` disables session-file persistence. | `--continue`, `--resume <ID-or-name>`, `--session-id <UUID>`, `--fork-session`, and `--no-session-persistence`. |
| Read-only execution | Provider sandbox: `--sandbox read-only`; unattended variant: `--sandbox read-only --ask-for-approval never`. Do not use `workspace-write`, `danger-full-access`, or the bypass flag for a read-only run. | `--permission-mode plan` blocks edits but allows exploration. Restrict built-ins with `--tools`; deny MCP separately with `--disallowedTools "mcp__*"`. |
| Turn budget | No documented `codex exec` max-turn or wall-clock flag found. | `--max-turns`; `--max-budget-usd` also applies spend caps and counts subagents. |
| Cancellation | Ctrl-C is handled by the exec process as a `turn/interrupt`; app-server has JSON-RPC `turn/interrupt`. No general CLI timeout flag was found. | SIGINT ends the turn; SIGTERM exits with code 143 and leaves the turn unfinished. No general `-p` wall-clock timeout flag was found. |
| Failure signal | Official source exits 1 when the relevant turn/server error is observed. Signals otherwise follow the host process semantics. | Official headless docs specify code 0 for success and nonzero for failure. SIGTERM is specifically documented as 143. |

Sources: [Codex noninteractive mode](https://learn.chatgpt.com/docs/non-interactive-mode), [Codex CLI source](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/cli.rs), [Codex JSON event source](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/exec_events.rs), [Codex exec loop and interrupt source](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/lib.rs), [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), and [Claude headless mode](https://code.claude.com/docs/en/headless).

## Read-only capability and fail-closed checks

### Codex

OpenAI documents `read-only` as a sandbox in which the agent can inspect files but cannot edit them. The noninteractive guide describes it as the default for `codex exec`; the approval guide gives `--sandbox read-only --ask-for-approval never` as the read-only CI combination. The CLI source makes `--sandbox`, `--ask-for-approval`, `--output-schema`, `--json`, and `--ephemeral` real parsed options, rather than prompt conventions.

- The read-only guarantee is the Codex execution sandbox, not a promise made by the prompt.
- `--ask-for-approval never` prevents an unattended run from waiting for a person. It does not turn a write-capable sandbox into a read-only one.
- `--dangerously-bypass-approvals-and-sandbox` explicitly removes the boundary and must be treated as incompatible with a read-only request.
- If a required flag is missing from `codex exec --help`, the requested read-only contract is unsupported and the run should fail before inference. Do not silently substitute an older compatibility flag.

Sources: [Codex sandbox](https://learn.chatgpt.com/docs/sandboxing), [Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security), [Codex noninteractive mode](https://learn.chatgpt.com/docs/non-interactive-mode), [shared CLI option definitions](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/utils/cli/src/shared_options.rs), and [Codex exec tests for nonzero server failure](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/tests/suite/server_error_exit.rs).

### Claude Code

Anthropic documents plan mode as research and proposal without source edits. It still reads files and runs shell commands to explore. The CLI separately supports `--tools` for the built-in tool set and `--disallowedTools` for deny rules. The CLI reference explicitly says `--tools` does not affect MCP tools and that `mcp__*` must be denied separately.

For a read-only status or inspection run, the provider-supported controls can therefore be checked as two layers:

1. `--permission-mode plan` for the provider's no-edit planning mode.
2. A built-in tool allowlist containing only the read tools needed by the task, plus `--disallowedTools "mcp__*"` when MCP must not be called.

The exact tool set is a caller choice, not resolved by this research. The important negative finding is that plan mode alone still allows shell exploration, and `--allowedTools` is an auto-approval list, not a tool-removal list. If the required tool restrictions are unavailable, fail closed instead of using `--dangerously-skip-permissions`, `acceptEdits`, or an unrestricted `Bash` allow rule.

Claude Code has newer restrictions that are not available in the locally observed version: `--restricted` requires v2.1.248 or later, and `--permission-prompts none` requires v2.1.259 or later. Local Claude Code is v2.1.223, so neither flag can be treated as present for this environment. The documented `--permission-prompts none` behavior denies unresolved prompts rather than waiting for a host, which is useful for unattended runs but is not a substitute for removing write-capable tools.

Sources: [Claude permission modes](https://code.claude.com/docs/en/permission-modes), [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), and [Claude headless mode](https://code.claude.com/docs/en/headless).

## Output, sessions, cancellation, and time limits

### Codex output and sessions

`codex exec --json` changes stdout to JSONL, while progress in the normal mode is sent to stderr and only the final message is sent to stdout. The official event source defines a first `thread.started` event with a resumable `thread_id`, turn lifecycle events, item lifecycle events, MCP tool-call items, collaboration-agent items, and terminal error events. `--output-schema` shapes the final agent message, not the entire event stream. `--ephemeral` removes session-file persistence and therefore changes whether a later resume is possible.

Codex's exec source listens for Ctrl-C and sends `turn/interrupt` for the active turn. The app-server reference documents the same operation and reports a terminal `interrupted` status. There is no documented CLI wall-clock timeout option in the current `codex exec` reference or the locally observed help, so a timeout imposed by a caller is an external process policy, not a Codex result type.

Sources: [Codex noninteractive mode](https://learn.chatgpt.com/docs/non-interactive-mode), [Codex exec CLI source](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/cli.rs), [Codex event definitions](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/exec_events.rs), [Codex exec implementation](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/lib.rs), and [Codex app-server interruption](https://learn.chatgpt.com/docs/app-server).

### Claude output and sessions

`claude -p` supports `text`, `json`, and `stream-json`. JSON contains the final result, session ID, and metadata. `--json-schema` places the validated structured result in `structured_output`; invalid schemas fail instead of silently becoming unstructured output on current versions. Streaming emits newline-delimited events, and the final `result` event includes response text, cost, and session metadata. `--include-partial-messages` requires `stream-json` and emits token-level partial events.

The JSON session ID can be captured and passed to `--resume`. `--no-session-persistence` explicitly prevents resume. `--continue` and `--resume` are session controls, not a guarantee that the session ran locally or under the same account unless the caller checks the returned metadata and auth context.

Claude documents SIGTERM behavior for `-p`: exit code 143, unfinished turn, no new model request while exiting, and cleanup of the running Bash process tree. SIGINT ends the turn. The CLI has `--max-turns` and `--max-budget-usd`, but neither is a wall-clock timeout. A caller timeout remains an external termination and should be recorded as such.

Sources: [Claude headless mode](https://code.claude.com/docs/en/headless), [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), and [Claude error reference](https://code.claude.com/docs/en/errors).

## Authentication, account, and billing distinctions

### Codex

Codex automation can reuse saved CLI authentication or receive `CODEX_API_KEY` for a single invocation. The app-server account surface distinguishes `apikey` from ChatGPT-managed auth, reports `planType` when available, and exposes `account/rateLimits/read` for ChatGPT rate limits. OpenAI's account guidance says Codex is included across ChatGPT plans, that Codex, ChatGPT Work, and Workspace Agents can share a plan allowance and credit pool, and that the displayed dollar values are estimates rather than invoices.

Therefore, a Codex observation must retain the auth/account context. A ChatGPT plan limit, an API-key organization limit, a model availability error, and a local token count are different facts. A successful CLI process does not prove that a ChatGPT allowance or API billing account has remaining quota, and a token count does not establish an invoice amount.

Sources: [Codex automation authentication](https://learn.chatgpt.com/docs/non-interactive-mode), [Codex app-server account and rate-limit methods](https://learn.chatgpt.com/docs/app-server), [Using Codex with a ChatGPT plan](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan), and [Codex CLI source](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/cli/src/login.rs).

### Claude Code

Claude Code distinguishes subscription authentication from Console/API authentication. The official authentication documentation says that `ANTHROPIC_API_KEY` is used for direct API access and, in noninteractive `-p` mode, is always used when present. Subscription OAuth credentials are a separate path. The CLI reference exposes `claude auth login --console` for Console API usage billing instead of a Claude subscription.

The cost guide says subscription users see plan usage bars and that the session dollar figure is not the subscription bill. For API users, the CLI's session cost is a local estimate; authoritative billing is in Claude Console. Anthropic's legal guidance says developers building products should use API-key or supported cloud-provider authentication and may not collect or intermediate Claude.ai credentials or session tokens. This rules out treating aifuel as a broker for a user's Claude subscription credential.

Sources: [Claude authentication](https://code.claude.com/docs/en/team), [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), [Claude costs](https://code.claude.com/docs/en/costs), and [Claude legal and compliance](https://code.claude.com/docs/en/legal-and-compliance).

## MCP boundary for read-only monitoring

MCP is a client/server tool protocol in these CLI surfaces:

- Codex CLI loads MCP servers from `~/.codex/config.toml` or a trusted project config. It supports local stdio and Streamable HTTP server entries, server startup/tool timeouts, enabled and disabled tool lists, and tool approval modes. The current Codex CLI's `mcp` subcommands manage server entries and OAuth credentials. The official docs state that the old `codex mcp-server` command and standalone `codex-mcp-server` binary were removed; that command is not a supported way to expose AI Fuel.
- Claude Code loads servers from `.mcp.json`, user/project settings, or `--mcp-config`. It supports stdio and HTTP transports, and `-p` reports loaded servers and `mcp_server_errors` in `system/init` when the relevant versions support those fields. Invalid MCP config entries can be skipped while the run exits cleanly, so a machine caller must inspect the reported server list and errors.
- An MCP server can expose tools with side effects. Client approval modes and annotations are not a complete read-only proof for an arbitrary server. The server implementation and its credential scope must enforce that monitoring operations do not write credentials, mutate provider state, or start inference.

For this issue, `aifuel mcp` can only mean a read-only MCP server for AI Fuel status/model/quota observations. It must not mean a provider prompt launcher, an agent scheduler, or a proxy that accepts arbitrary model instructions. Those launch behaviors are outside the map.

Sources: [Codex MCP documentation](https://developers.openai.com/codex/mcp/), [Codex MCP CLI source](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/cli/src/mcp_cmd.rs), [Claude MCP documentation](https://code.claude.com/docs/en/mcp), and [Claude headless MCP startup/error fields](https://code.claude.com/docs/en/headless).

## Many-agent and session reporting

The provider surfaces can report more than one agent, but they do not expose one shared cross-provider agent registry:

- Codex's `codex agents` command is documented as a view of all agent sessions on the shared local app-server daemon. The current CLI help exposes this as an interactive overview. Codex JSONL events also carry collaboration-agent calls and states such as pending, running, interrupted, completed, errored, shutdown, and not found.
- Claude Code's `--bg` starts a background agent and returns a session ID. `claude agents --json` reports active sessions, and `--json --all` includes completed background sessions. `stream-json` can include subagent messages with `parent_tool_use_id`; current docs specify version gates for foreground and nested subagent forwarding.
- A status report can therefore distinguish provider, session ID, lifecycle state, model, and whether the provider supplied a terminal result. It cannot infer hidden, remote, expired, or provider-external agents from a local CLI count. A missing provider status is not evidence that zero agents exist.

Sources: [Codex CLI reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli), [Codex event definitions](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/exec/src/exec_events.rs), [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), and [Claude streaming/subagent events](https://code.claude.com/docs/en/headless).

## Version and feature detection

Version strings are useful diagnostics, but capability checks are safer because configuration and managed policy can change behavior.

### Codex checks

1. Run `codex --version` and record the exact version.
2. Check `codex exec --help` for `--sandbox read-only`, `--ask-for-approval never`, `--json`, `--output-schema`, `--ephemeral`, and the `resume` subcommand.
3. Check `codex mcp --help` only for read-only monitoring configuration inspection. Do not use it as a prompt-launch path.
4. Treat unsupported required flags as a hard failure. Never fall back to a write-capable sandbox or bypass flag.

The observed Codex CLI was `0.154.0`, matching the official `rust-v0.154.0` release tag.

### Claude Code checks

1. Run `claude --version`; observed local version: `2.1.223`.
2. Check `claude -p`/`claude --help` for `--permission-mode`, `--tools`, `--disallowedTools`, `--output-format`, `--json-schema`, `--model`, session flags, and MCP config flags. The official CLI reference warns that `--help` does not list every flag, so help alone is not a complete feature registry.
3. For stream mode, inspect the first `system/init` event. Current docs define a `capabilities` array for protocol behaviors such as `interrupt_receipt_v1` and `interrupt_cancel_queued_v1`, and define `mcp_server_errors` for skipped MCP config entries.
4. Apply documented minimum versions before depending on newer controls: `--mcp-config` startup waiting requires v2.1.221; `mcp_server_errors` requires v2.1.219; `--max-budget-usd` enforcement requires v2.1.217; `--permission-prompts none` requires v2.1.259; `--restricted` requires v2.1.248. The local v2.1.223 can meet the first three listed requirements but cannot be assumed to support the latter two.
5. If a required version, flag, capability, or `system/init` field is missing, fail closed and report `unsupported`, not `false` or `zero`.

Sources: [Codex noninteractive mode](https://learn.chatgpt.com/docs/non-interactive-mode), [Codex release](https://github.com/openai/codex/releases/tag/rust-v0.154.0), [Claude CLI reference](https://code.claude.com/docs/en/cli-reference), and [Claude headless mode](https://code.claude.com/docs/en/headless).

## Not resolved by this ticket

- No final AI Fuel module layout, permission default, provider order, or Rust migration design is selected here.
- No provider prompt is launched through MCP.
- No inference result, live quota value, billing balance, or credential validity was tested in this session.
- Provider-specific quota endpoints and local credential discovery remain separate research questions from the CLI contracts recorded here.
