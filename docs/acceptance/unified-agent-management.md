# Unified Agent Run acceptance

Tracks [issue #75](https://github.com/ducquoc97/aifuel/issues/75). Build and controlled-process tests are separate from native-agent acceptance. No cell is passed by executable discovery, help output, configuration registration, or an exit code alone.

## Environment inspected on 2026-09-22

The development host is WSL2 (`6.6.87.2-microsoft-standard-WSL2`). It does not establish native Linux, Windows, or macOS acceptance.

| Agent Integration | Installed version | WSL evidence at initial inspection | Native Windows | macOS | Linux |
| --- | --- | --- | --- | --- | --- |
| Codex | 0.155.1 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| Claude Code | 2.1.223 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| GitHub Copilot CLI | 1.0.87 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| Antigravity CLI | 1.2.7 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| Gemini CLI | Not installed | Blocked by missing executable | Outstanding | Outstanding | Outstanding |

The final follow-up in `rust-user-paths.md` records earlier translation and Gateway calls. Those observations do not prove the new run-management contract, denied effects, workspace boundaries, or exact per-run tool selection.

## Enforcement observations

- Codex exposes native read-only and workspace-write sandbox modes, `--ignore-user-config`, and `--ignore-rules`. Ignoring user configuration preserves normal authentication lookup. Native task-effect checks and checks for project/plugin configuration sources are still required.
- Claude exposes `--strict-mcp-config`, a built-in tool selection option, and safe mode. The older `plan` and `acceptEdits` mappings alone do not prove filesystem boundaries.
- Copilot describes `--plan` as an initial agent mode. Additional MCP configuration augments existing configuration. Its help describes built-in file-edit sandbox policy as best effort. These facts do not establish exact tool or write isolation.
- Antigravity describes `--sandbox` as enabling terminal restrictions. That description alone does not establish read-only filesystem enforcement.
- Gemini has no current installed interface to verify on this host. Earlier observations of another version do not establish current availability.

## Required native acceptance

For every platform/provider cell, record the AI Fuel commit and installed version, native version, model and effort evidence, permission boundary, and selected external tools. Run the exact translation prompt:

> translate to Vietnamese: Fetch Codex redemption detail through account/rateLimits/read

Also exercise a disposable repository task, denied writes and workspace escapes, a selected external tool with server-side call evidence, cancellation and descendant cleanup, and resume where supported. Record reduced or blocked capabilities explicitly. Missing hardware, credentials, or native interfaces remain outstanding acceptance.

AI Fuel-owned content retention does not control native agent transcript storage. Cancellation does not reverse completed effects. Worktree locks do not serialize shared Git metadata operations across worktrees.

Execution MCP rejects the opt-in persistent content policy until its private metadata store is implemented; connection-only content remains memory-bound to the owner and is discarded on owner shutdown.
