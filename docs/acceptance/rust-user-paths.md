# Rust user-path acceptance

This record documents the verification performed for the Rust replacement
branch. It is intentionally secret-free. Fixture tests prove parsing and
process behavior; they do not replace live provider acceptance.

## Verified

- Rust 1.85.0 builds and passes the locked workspace test suite.
- Rust 1.85.0 passes strict workspace Clippy.
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
- The current local account returned live Codex, Gemini, Copilot, and
  Antigravity observations. Claude returned HTTP 401, so its current account
  remains an unavailable live observation.
- Windows and macOS are covered by the CI build and test matrix, but no live
  provider accounts were used for those platforms in this run.
- Provider account selection is fail-closed because the installed provider
  CLIs do not expose a verified account-binding flag through this interface.
- The 69-provider catalog is represented with explicit capability and
  per-platform states. Live Rust adapters currently cover the five AI Fuel
  integrations; catalog-only providers remain unsupported.
- The legacy Python source and tests remain as migration reference material.
  The Rust installer and runtime do not invoke Python.
