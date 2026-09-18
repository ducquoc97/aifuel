# Rust user-path acceptance

This record documents the verification performed for the Rust replacement
branch and issue #29. It is intentionally secret-free. Fixture tests prove
parsing and process behavior; they do not replace live provider acceptance.

## Verified

- This workspace now requires Rust 1.88.0 or newer. The monitoring-only branch
  supported Rust 1.85.0 before the MCP Gateway dependency was added.
- The real Rust binary serves the embedded dashboard over loopback.
- The real Rust binary returns normalized JSON status.
- The real Rust binary serves MCP initialize, tools/list, resources/list,
  resources/read, and get_status over stdio.
- Gemini, Claude Code, Codex CLI, and GitHub Copilot launcher argument
  construction passes a real process boundary test with fake executables.
- Gemini quota collection passes a real Reqwest HTTP boundary test for
  loadCodeAssist and retrieveUserQuota.
- Claude, Codex, and Copilot quota normalization passes fixture HTTP tests.
- The installed Linux release binary is produced by the installer and prints
  the Rust command help.
- A live Gemini prompt succeeded with an explicit model:

    aifuel run --provider gemini --model gemini-2.5-flash --prompt "Reply with exactly OK. Do not use tools."

## Known acceptance boundaries

- The installed Gemini CLI default model currently returns an upstream HTTP
  400 because it sends both thinking_budget and thinking_level. AI Fuel does
  not silently choose another model. Use an explicit provider-supported model.
- The final status smoke returned live Codex and Copilot observations. Claude,
  Gemini, and Antigravity returned HTTP 401, so those monitoring observations
  remain unavailable in this environment.
- Windows and macOS are covered by the CI build and test matrix, but no live
  provider accounts were used for those platforms in this run.
- Provider account selection is fail-closed because the installed provider
  CLIs do not expose a verified account-binding flag through this interface.
- The 69-provider catalog is represented with explicit capability and
  per-platform states. Live Rust adapters currently cover the five AI Fuel
  integrations; catalog-only providers remain unsupported.
- The legacy Python source and tests remain as migration reference material.
  The Rust installer and runtime do not invoke Python.

## MCP Gateway

- `cargo test --workspace --locked` passed on Rust 1.96.0. Strict workspace
  Clippy passed on Rust 1.97.1. The gateway tests
  launch the real `aifuel mcp gateway --agent codex` binary and a separately
  compiled local MCP server fixture.
- The gateway/server process tests cover host negotiation for MCP 2025-06-18 and
  2025-11-25, counteroffers for unsupported host versions, local tool listing
  and calls, errors, deadlines, cancellation, bounded output, and cleanup of the
  gateway-owned process tree. The upstream fixture negotiates 2025-11-25.
- A terminal smoke of the built `aifuel mcp gateway --agent codex` binary with
  the local fixture returned protocol 2025-11-25, listed `local__echo`, and
  returned the fixture tool result.
- Codex CLI 0.155.0 completed a live call through a temporary
  `mcp_servers.aifuel-gateway` registration and temporary Codex home. It
  negotiated MCP 2025-06-18, called `fixture__echo` with
  `codex-gateway-smoke`, and returned `fixture-result`; the fixture log recorded
  the call. The run used `--disable plugins` to isolate the local host path and
  did not change the saved Codex configuration. Codex also logged a non-fatal
  model-catalog refresh timeout, but exited successfully after the tool call.
- Agent MCP Registration acceptance on 2026-09-18 used Codex CLI 0.155.0 on
  Linux x86_64 WSL2, with a temporary `CODEX_HOME`, temporary `HOME`, and
  temporary `XDG_CONFIG_HOME`. The public `aifuel mcp setup --agent codex`
  command wrote `[mcp_servers.aifuel-gateway]` with the built `aifuel` command,
  `mcp gateway --agent codex` arguments, and Codex's documented
  `env_vars = ["XDG_CONFIG_HOME"]` forwarding rule. Codex App Server loaded the
  registration, listed `fixture__echo`, and its `mcpServer/tool/call` request
  with `message = "codex-gateway-smoke"` returned `fixture-result`; the local
  fixture log recorded that call. The temporary setup wrote no provider
  credentials or upstream server definitions into Codex config.
- A separate model-driven `codex exec --json` check did not call the fixture.
  Without authentication in the temporary Codex home, it exited with HTTP 401.
  A temporary copy of the existing auth file allowed a model turn, but the
  headless run reported the fixture tool unavailable and logged an OAuth
  requirement for the hosted Cloudflare MCP service; no fixture process started.
  The direct App Server MCP call above verifies the registration and tool path
  independently of model authentication.
- The `rmcp` 1.6.0 dependency uses let-chain syntax, stabilized in Rust 1.88
  ([Rust 1.88 release notes](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0/)).
  Rust 1.85.0 fails while compiling that dependency, so the workspace manifest,
  CI toolchain, and README now use the 1.88.0 minimum.
- Rust 1.88.0 is the CI-pinned minimum but was not installed in this local
  environment; the local verification toolchains were Rust 1.96.0 and 1.97.1.

### Manual Codex acceptance path

1. Use a local MCP server executable that is already installed.
2. Add its local `stdio` definition and the `codex` selection to the central
   `aifuel/mcp.json` catalog shown in the README.
3. Add the `[mcp_servers.aifuel-gateway]` entry from the README to
   `~/.codex/config.toml`, then restart Codex.
4. Confirm the gateway appears in Codex's MCP server list and ask Codex to call
   one selected tool. Record the Codex version, OS, negotiated protocol, and
   returned result before marking that host combination verified.

The live host check used a separately compiled local fixture server rather than
an installed third-party MCP server. Codex's user-level configuration remained
unchanged.

## Agent Runs through registered execution adapters

- Workspace tests invoke the real `aifuel` process with controlled executables
  for Claude Code, Codex CLI, GitHub Copilot CLI, and Gemini CLI. They cover
  explicit provider selection, provider-specific model/access/output arguments,
  unsupported capabilities, nonzero exits, captured output, timeouts,
  continuation, stdin prompts, and working-directory behavior.
- The public application facade passes cancellation to the selected adapter.
  The provider process test confirms cancellation stops and reaps its child
  process while retaining output produced before cancellation.
- A live Gemini Agent Run was attempted on Linux x86_64 WSL2 with Gemini CLI
  0.42.0 and explicit model `gemini-2.5-flash`. The prompt was “Reply with
  exactly OK. Do not use tools.” The built `aifuel` binary ran with a private
  temporary `HOME`, workspace, and temporary copy of the Gemini OAuth file;
  the saved user configuration and `.env` files were not changed.
- The live command returned exit code 4 after 23.2 seconds because provider
  authentication was rejected. No successful authenticated Agent Run was
  verified in this attempt. Fake-executable process tests do not replace that
  live acceptance, and no provider account, effective model, or session
  identity is inferred from the failed attempt.
