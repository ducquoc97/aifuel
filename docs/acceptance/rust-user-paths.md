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
- The gateway/server process tests cover exact MCP 2025-11-25 negotiation,
  local tool listing and calls, errors, deadlines, cancellation, bounded output,
  and cleanup of the gateway-owned process tree.
- A terminal smoke of the built `aifuel mcp gateway --agent codex` binary with
  the local fixture returned protocol 2025-11-25, listed `local__echo`, and
  returned the fixture tool result.
- Codex CLI 0.155.0 listed an `mcp_servers.aifuel-gateway` registration
  supplied with CLI `--config` overrides. This confirms config parsing only; it
  does not verify a live gateway handshake or tool call.
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

This worktree has not yet recorded a live Codex tool invocation. The process
fixture verifies the gateway protocol and call path independently.
