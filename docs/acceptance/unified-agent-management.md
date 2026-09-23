# Unified Agent Run acceptance

Tracks [issue #75](https://github.com/ducquoc97/aifuel/issues/75). Build and controlled-process tests are separate from native-agent acceptance. No cell is passed by executable discovery, help output, configuration registration, or an exit code alone.

## Environment inspected on 2026-09-22 (Codex rechecked on 2026-09-23)

The development host is WSL2 (`6.6.87.2-microsoft-standard-WSL2`). It does not establish native Linux, Windows, or macOS acceptance.

| Agent Integration | Installed version | WSL evidence at initial inspection | Native Windows | macOS | Linux |
| --- | --- | --- | --- | --- | --- |
| Codex | 0.156.0 | Live managed acceptance passed for the exact prompt, repository boundaries, selected Gateway tool, resume, and cancellation; see evidence below | Outstanding | Outstanding | Outstanding |
| Claude Code | 2.1.223 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| GitHub Copilot CLI | 1.0.87 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| Antigravity CLI | 1.2.8 | Version and help inspected; effects not verified | Outstanding | Outstanding | Outstanding |
| Gemini CLI | Not installed | Blocked by missing executable | Outstanding | Outstanding | Outstanding |

The final follow-up in `rust-user-paths.md` records earlier translation and Gateway calls. They are historical evidence only; the current branch's managed-run acceptance is recorded below.

### WSL Codex managed-run evidence (2026-09-23)

The AI Fuel release binary was built from this completion worktree and run with Codex CLI 0.156.0 and ChatGPT authentication. At the 2026-09-23 final check, the cached catalog was stale (oldest scope age 17,991 seconds) and contained `gpt-6-astra`; its account entitlement remained unknown. The exact prompt below succeeded. Codex reported effective model `gpt-6-astra` and effective effort `max` even though no effort was requested; the result preserved requested effort as unset and its selection source as `native_default`. This is evidence for that run only, not a guarantee of future entitlement.

- The exact translation prompt succeeded with `--access read-only` and no external tools.
- In a disposable Git repository, Codex created `acceptance.txt` with the requested exact content under `workspace-write`. A separate read-only attempt to create `forbidden.txt` was denied by the sandbox; the requested escalation was declined through `aifuel approve`. A workspace-write attempt to create `../escape.txt` was also denied. Neither denied file exists.
- A selected `acceptance__record-call` Gateway tool returned a unique marker, and the local fixture server recorded the same marker. App Server status showed the exact selected Gateway tool and no other connected MCP server.
- A successful native Codex session was resumed in the same disposable repository under a read-only sandbox and returned the file contents without modifying it.
- An execution-MCP-owned Codex run was cancelled. The run reached the `cancelled` terminal state, and the observed App Server process was gone after cancellation.

Codex logged that `bubblewrap` was not on `PATH` and that it used its bundled fallback. The read-only and workspace-root enforcement checks above still produced the expected blocked results. This is WSL evidence only; it does not establish native Linux, Windows, or macOS behavior. The other four WSL provider integrations and all native-platform cells remain outstanding.

## Enforcement observations

- Managed Codex runs start App Server with an empty user MCP configuration and disable Codex plugins, while preserving the normal `CODEX_HOME` authentication lookup. When exact external tools are selected, AI Fuel waits for App Server's MCP status and rejects startup unless only the Gateway is connected with exactly those tool names. Live effect checks are recorded above.
- Claude exposes `--strict-mcp-config`, a built-in tool selection option, and safe mode. The older `plan` and `acceptEdits` mappings alone do not prove filesystem boundaries.
- Copilot describes `--plan` as an initial agent mode. Additional MCP configuration augments existing configuration. Its help describes built-in file-edit sandbox policy as best effort. These facts do not establish exact tool or write isolation.
- Antigravity describes `--sandbox` as enabling terminal restrictions. That description alone does not establish read-only filesystem enforcement.
- Gemini has no current installed interface to verify on this host. Earlier observations of another version do not establish current availability.

## Required native acceptance

For every platform/provider cell, record the AI Fuel commit and installed version, native version, model and effort evidence, permission boundary, and selected external tools. Run the exact translation prompt:

> translate to Vietnamese: Fetch Codex redemption detail through account/rateLimits/read

Also exercise a disposable repository task, denied writes and workspace escapes, a selected external tool with server-side call evidence, cancellation and descendant cleanup, and resume where supported. Record reduced or blocked capabilities explicitly. Missing hardware, credentials, or native interfaces remain outstanding acceptance.

AI Fuel-owned content retention does not control native agent transcript storage. Cancellation does not reverse completed effects. Worktree locks do not serialize shared Git metadata operations across worktrees.

Connection-only content remains memory-bound to the owner and is discarded on owner shutdown. The opt-in persistent content policy stores bounded answer and diagnostic content in private local files with metadata-only defaults otherwise.

## Follow-up implementation

The post-merge completion branch adds profile save/list/remove, profile/global default resolution, terminal model selection, explicit catalog list/refresh commands, scoped freshness diagnostics, same-provider resume with private metadata-only session storage, canonical cross-process workspace-write locks, bounded streamed output, structured CLI output, and an owner-local run lifecycle. Codex uses the bidirectional App Server protocol; other providers retain explicitly reduced final-answer-only support. The Codex bundled model catalog is connected; other providers report catalog discovery as unsupported instead of fabricating model or effort evidence.

The execution MCP endpoint now uses the same selection resolver as the CLI for explicit arguments, named profiles, and global defaults. `list_models` exposes explicit refresh and stale/error diagnostics; `list_agents` returns provider-owned presence/version, per-capability declared/current evidence with reasons, and native setup guidance. Current enforcement remains unknown until live evidence is recorded. Claude authentication status is checked with a bounded non-interactive native command; only its documented exit state is retained, and stdout/stderr are discarded. Authentication remains unknown for the other providers unless a safe signal is established; listing never invokes login. Local version probes use documented, non-interactive flags for Codex, Claude Code, Copilot CLI, and Gemini CLI ([Codex](https://developers.openai.com/cookbook/examples/codex/using_goals_in_codex), [Claude Code](https://code.claude.com/docs/en/cli-usage), [Copilot CLI](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference), [Gemini CLI](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md)); Antigravity presence is checked without launching it, and its version remains unknown.

Exact external tool selections are routed through the AI Fuel Gateway for Codex runs and filtered both when tools are listed and when calls are routed. Managed Codex runs disable user plugins and verify the connected server/tool inventory before the first turn. Read-only runs require an exact local allowlist. Local approvals use Unix-domain sockets on Unix hosts and a current-user-restricted named-pipe backend on Windows. WSL Codex live checks and controlled cross-platform tests exercise those interfaces; remaining native provider/platform cells are separate gates. Catalog entitlement and execution evidence remain unknown until observed.

## Interaction protocol evidence

The managed Codex adapter uses bidirectional App Server JSON-RPC for thread start/resume, turns, streamed answer deltas, and server-initiated questions and permission requests. Its wire shapes were checked against the generated schema from installed `codex-cli 0.156.0`. Controlled fixtures verify native-thread resume, model/effort and sandbox propagation, fragmented stream frames, multiple question-ID answers, typed elicitation content, correlated responses, and explicit failures for malformed or oversized frames. The standalone `aifuel run` command and execution MCP both use the shared RunManager lifecycle. These checks do not establish live authentication, permission enforcement, or successful provider acceptance.

For Codex, App Server receives only the per-run AI Fuel Gateway configuration when exact external tools are selected; user plugins and unrelated connected MCP servers are disabled or rejected before the turn starts. Unix approval IPC uses a private per-owner discovery record and lazily started socket. The Windows backend has a current-user ACL, remote-client rejection, bounded overlapped I/O, and owner-instance checks; app-crate Windows-target tests compile, while native pipe runtime and ACL acceptance remain pending. Read-only external tools still require an exact local allowlist. [OpenAI's Codex harness overview](https://developers.openai.com/blog/codex-as-a-platform) describes App Server's thread, streaming, tools, sandbox, and approval responsibilities; [Codex plugin configuration](https://developers.openai.com/plugins/build/plugins) documents plugin-owned MCP servers as a separate source that managed runs must isolate. [GitHub's Copilot CLI documentation](https://docs.github.com/en/copilot/concepts/agents/copilot-cli/about-remote-control) describes interactive permission and question responses, but the current Copilot adapter remains final-answer-only. Protocol fixtures and cross-target compilation are implementation evidence, not successful native-platform acceptance evidence.

## Windows dependency check (2026-09-23)

`windows-sys` resolves to 0.61.2, is dual-licensed MIT/Apache-2.0, and declares Rust 1.71 as its minimum supported version. The `microsoft/windows-rs` upstream repository is active, with release tag 74 published on 2026-09-03 and commits on 2026-09-18 and 2026-09-19. The locked dependency tree also shows existing project dependencies including Tokio, rustix, and reqwest already use `windows-sys`; the new direct use does not introduce an unfamiliar platform stack.
