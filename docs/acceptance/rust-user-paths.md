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
- The remote Streamable HTTP fixture tests run the real `aifuel mcp gateway
  --agent codex` binary. They cover upstream 2025-11-25 initialization, JSON
  tool calls, SSE delivery and GET resumption with `Last-Event-ID`, duplicate
  event suppression, concurrent calls, cancellation and late-response discard,
  60,000 ms retry versus an absolute operation deadline, session-specific 404
  reinitialization without tool POST replay, response-size limits, malformed
  JSON, redirect rejection, non-loopback plain HTTP rejection, and session
  shutdown.
- On 2026-09-18 in Linux 6.6.87.2 WSL2 (x86_64), the real gateway process
  connected to `https://mcp.deepwiki.com/mcp`, negotiated its supported
  2025-11-25 upstream profile, and listed the live `ask_question`,
  `read_wiki_contents`, and `read_wiki_structure` tools. A call to
  `read_wiki_structure` for `modelcontextprotocol/rust-sdk` returned its
  documentation structure with `isError: false`. A call for `ducquoc97/aifuel`
  also reached DeepWiki, which returned the server error `Error fetching wiki
  for ducquoc97/aifuel: Repository not found. Visit
  https://deepwiki.com/ducquoc97/aifuel to index it.` This verifies the remote
  transport and tool roundtrip while recording the unindexed-repository case.
  It does not establish that Codex loaded a remote registration. The smoke used
  a temporary AI Fuel catalog and did not change the saved Codex configuration.
- On rustc 1.97.1, `cargo test --workspace --locked` passed 64 tests across 24
  suites, and `cargo clippy --workspace --all-targets --locked -- -D warnings`
  completed without issues.
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
- The `rmcp` 1.6.0 dependency uses let-chain syntax, stabilized in Rust 1.88
  ([Rust 1.88 release notes](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0/)).
  Rust 1.85.0 fails while compiling that dependency, so the workspace manifest,
  CI toolchain, and README now use the 1.88.0 minimum.
- Rust 1.88.0 is the CI-pinned minimum but was not installed in this local
  environment; the local verification toolchains were Rust 1.96.0 and 1.97.1.

### Manual Codex acceptance path

1. Use an installed local MCP server executable or an unauthenticated remote
   Streamable HTTP server.
2. Add its `stdio` or `streamable-http` definition and the `codex` selection to
   the central `aifuel/mcp.json` catalog shown in the README.
3. Add the `[mcp_servers.aifuel-gateway]` entry from the README to
   `~/.codex/config.toml`, then restart Codex.
4. Confirm the gateway appears in Codex's MCP server list and ask Codex to call
   one selected tool. Record the Codex version, OS, negotiated protocol, and
   returned result before marking that host combination verified.

The live host check used a separately compiled local fixture server rather than
an installed third-party MCP server. Codex's user-level configuration remained
unchanged.
