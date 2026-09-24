# Unified Agent Run acceptance

Tracks [issue #75](https://github.com/ducquoc97/aifuel/issues/75). Build and controlled-process tests are separate from native-agent acceptance. No cell is passed by executable discovery, help output, configuration registration, or an exit code alone.

## Environment inspected on 2026-09-22 (provider versions rechecked on 2026-09-24)

The development host is WSL2 (`6.6.87.2-microsoft-standard-WSL2`). It does not establish native Linux, Windows, or macOS acceptance.

| Agent Integration | Installed version | WSL evidence at 2026-09-24 recheck | Native Windows | macOS | Linux |
| --- | --- | --- | --- | --- | --- |
| Codex | 0.156.1 | Current release CLI passed the exact prompt with ReadOnly and a selected Gateway tool; earlier sandbox and lifecycle evidence used 0.156.0 | Outstanding | Outstanding | Outstanding |
| Claude Code | 2.1.223 | Prompt-only ReadOnly and workspace-write requests now reject before launch; exact Gateway routing is explicitly unsupported by the adapter | Outstanding | Outstanding | Outstanding |
| GitHub Copilot CLI | 1.0.88 | Prompt-only ReadOnly and workspace-write requests now reject before launch; exact Gateway routing is explicitly unsupported by the adapter | Outstanding | Outstanding | Outstanding |
| Antigravity CLI | 1.2.8 (last version identified 2026-09-23) | Executable remains present; prompt-only ReadOnly and workspace-write requests reject before launch; exact Gateway routing is explicitly unsupported | Outstanding | Outstanding | Outstanding |
| Gemini CLI | Not installed | Blocked by missing executable | Outstanding | Outstanding | Outstanding |

The final follow-up in `rust-user-paths.md` records earlier translation and Gateway calls. They are historical evidence only; the current branch's managed-run acceptance is recorded below.

### WSL Codex managed-run evidence (2026-09-23)

The AI Fuel release binary was built from this completion worktree and run with Codex CLI 0.156.0 and ChatGPT authentication. At the 2026-09-23 final check, the cached catalog was stale (oldest scope age 17,991 seconds) and contained `gpt-6-astra`; its account entitlement remained unknown. The exact prompt below succeeded. Codex reported effective model `gpt-6-astra` and effective effort `max` even though no effort was requested; the result preserved requested effort as unset and its selection source as `native_default`. This is evidence for that run only, not a guarantee of future entitlement.

- The exact translation prompt succeeded with `--access read-only` and no external tools.
- In a disposable Git repository, Codex created `acceptance.txt` with the requested exact content under `workspace-write`. A separate read-only attempt to create `forbidden.txt` was denied by the sandbox; the requested escalation was declined through `aifuel approve`. A workspace-write attempt to create `../escape.txt` was also denied. Neither denied file exists.
- A selected `acceptance__record-call` Gateway tool returned a unique marker, and the local fixture server recorded the same marker. App Server status showed the exact selected Gateway tool and no other connected MCP server.
- A successful native Codex session was resumed in the same disposable repository under a read-only sandbox and returned the file contents without modifying it.
- An execution-MCP-owned Codex run was cancelled. The run reached the `cancelled` terminal state, and the observed App Server process was gone after cancellation.

Codex logged that `bubblewrap` was not on `PATH` and that it used its bundled fallback. The read-only and workspace-root enforcement checks above still produced the expected blocked results. This is WSL evidence only; it does not establish native Linux, Windows, or macOS behavior. WSL Copilot, Antigravity, and Gemini, Claude's remaining effects, and all native-platform cells remain outstanding.

### WSL Claude Code managed-run evidence (2026-09-23)

The AI Fuel release binary from commit `e6e9123` ran Claude Code 2.1.223 with the exact translation prompt and `--access read-only`, with no working directory (prompt-only mode). The response succeeded and preserved `requested_model: sonnet`; AI Fuel reported the override as explicit with unknown catalog evidence, and Claude did not report an effective model. No output or tool side effects were requested. This prompt-only result does not prove project read-only enforcement, Gateway routing, cancellation, resume, or native-platform support. The Claude WSL acceptance cell remains incomplete.

### WSL Copilot CLI managed-run attempt (2026-09-23)

Copilot CLI 1.0.88 was invoked through AI Fuel with the exact translation prompt, `--model auto`, `--access read-only`, no working directory, and a 90-second deadline. The run timed out with no output and a `timed_out` result; the requested model remained an unverified explicit override and no effective model was reported. No automatic login or permission-expanding tool option was enabled. This attempt does not establish authentication or execution support; the WSL Copilot acceptance cell remains outstanding.

### WSL Antigravity managed-run evidence (2026-09-23)

Antigravity CLI 1.2.8 listed `gemini-3.8-flash-low` as an available native model. AI Fuel's Antigravity catalog remains unsupported, so this exact ID was supplied as an explicit override and correctly retained as unknown catalog evidence. The exact translation prompt succeeded under `--access read-only` with no working directory (prompt-only mode); the provider did not report an effective model. In a disposable, configured project workspace, a read-only request to create `forbidden.txt` returned success, but Antigravity reported writing the file in its native scratch directory outside the selected workspace. The workspace itself remained empty. AI Fuel did not inspect or remove the reported native scratch artifact because it is outside this worktree. This does not establish read-only enforcement. The adapter declares Antigravity read-only unsupported, and AI Fuel rejects its read-only requests before launching providers. Its WSL acceptance cell remains outstanding.

### WSL prompt and Gateway follow-up (2026-09-24)

The host is WSL2 (`6.6.87.2-microsoft-standard-WSL2`). Non-interactive version probes returned Codex `0.156.1`, Claude Code `2.1.223`, and GitHub Copilot CLI `1.0.88`. `agy` was present at `/home/willnguyen/.local/bin/agy`; its version remains the last observed `1.2.8` because Antigravity documents no non-interactive version command. `gemini --version` found no executable. The release binary was built from the PR worktree after `cea230a`, with the prompt-only ReadOnly gate change in the working tree.

- `rtk target/release/aifuel run --provider codex --model gpt-6-astra --prompt "translate to Vietnamese: Fetch Codex redemption detail through account/rateLimits/read" --access read-only --timeout 90s --output json` succeeded in 18 seconds. Codex returned the Vietnamese text “Lấy thông tin chi tiết về việc đổi thưởng Codex thông qua account/rateLimits/read.” It reported effective model `gpt-6-astra` and effort `medium`. The model catalog cache was stale (oldest scope age 89,883 seconds); account entitlement and future execution availability remain unknown. Codex logged that it used its bundled bubblewrap because `bubblewrap` was absent from `PATH`.
- A disposable local stdio MCP fixture was selected as `issue75__echo` under an isolated `XDG_CONFIG_HOME` in `target/issue75-gateway`. The temporary execution policy allowlisted only that exact tool. A bounded prompt-only Codex run requested the tool with message `ISSUE75_GATEWAY_CALL_20260924`. In a PTY, the run accepted the provider's default effort and prompt-only scope, then approved the exact Gateway tool request with `{}`. AI Fuel returned `AIFUEL_ISSUE75_GATEWAY_MARKER_20260924`; the fixture log recorded a JSON-RPC `tools/call` with the same request message and Codex version `0.156.1` under the ReadOnly sandbox. A non-interactive attempt stopped at the provider's form-input request before the fixture received a call.
- On 2026-09-24, the same exact translation prompt with `--access read-only` and `--access workspace-write` exited with code 2 for Claude, Copilot, and Antigravity. The CLI reported that each adapter could not enforce the requested access and rejected before provider launch. Exact `issue75__echo` Gateway requests with `--access workspace-write` also exited with code 2 and reported that each adapter could not enforce exact external MCP tool selection. These results record the supported boundary; they do not count as native prompt execution for those providers.

The successful live prompt and Gateway tool call are WSL Codex evidence only. Earlier 2026-09-23 Claude and Antigravity prompt successes used the then-permitted prompt-only ReadOnly path and are historical evidence for those adapter versions. The 2026-09-24 gate now rejects those requests until the adapters establish an access boundary. WSL acceptance remains incomplete for Claude, Copilot, Antigravity, Gemini, and every native Windows, macOS, and Linux cell.

### Controlled verification for the ReadOnly gate (2026-09-24)

The end-to-end `aifuel run` regression uses fake provider executables for Claude, Copilot, Gemini, and Antigravity. Before the gate change, the Antigravity request returned the fake provider response. With the gate change, all four prompt-only ReadOnly requests exit with code 2 before a provider response; the supported Codex CLI test explicitly requests ReadOnly and verifies the App Server sandbox value. Generic CLI process failure and timeout tests use a test adapter that declares ReadOnly support.

`rtk cargo test --workspace -- --test-threads=1` passed 307 tests across 44 suites. `rtk cargo fmt --all -- --check`, `rtk cargo check --workspace`, and `rtk cargo clippy --workspace --all-targets --all-features -- -D warnings` also passed.

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

All read-only requests, including prompt-only and resumed-session requests, require the provider adapter to declare read-only enforcement; unknown and unsupported providers are rejected before process launch. A prompt-only run's AI Fuel-owned temporary working directory does not sandbox task-directed writes outside that directory. Codex is currently the only adapter with verified read-only support. Generic CLI capability help probes are separate, bounded setup processes and do not receive the prompt. Native provider scratch and transcript behavior remains outside AI Fuel's retention control.

## Interaction protocol evidence

The managed Codex adapter uses bidirectional App Server JSON-RPC for thread start/resume, turns, streamed answer deltas, and server-initiated questions and permission requests. Its wire shapes were checked against the generated schema from installed `codex-cli 0.156.0`. Controlled fixtures verify native-thread resume, model/effort and sandbox propagation, fragmented stream frames, multiple question-ID answers, typed elicitation content, correlated responses, and explicit failures for malformed or oversized frames. The standalone `aifuel run` command and execution MCP both use the shared RunManager lifecycle. These checks do not establish live authentication, permission enforcement, or successful provider acceptance.

For Codex, App Server receives only the per-run AI Fuel Gateway configuration when exact external tools are selected; user plugins and unrelated connected MCP servers are disabled or rejected before the turn starts. Unix approval IPC uses a private per-owner discovery record and lazily started socket. The Windows backend has a current-user ACL, remote-client rejection, bounded overlapped I/O, and owner-instance checks; app-crate Windows-target tests compile, while native pipe runtime and ACL acceptance remain pending. Read-only external tools still require an exact local allowlist. [OpenAI's Codex harness overview](https://developers.openai.com/blog/codex-as-a-platform) describes App Server's thread, streaming, tools, sandbox, and approval responsibilities; [Codex plugin configuration](https://developers.openai.com/plugins/build/plugins) documents plugin-owned MCP servers as a separate source that managed runs must isolate. [GitHub's Copilot CLI documentation](https://docs.github.com/en/copilot/concepts/agents/copilot-cli/about-remote-control) describes interactive permission and question responses, but the current Copilot adapter remains final-answer-only. Protocol fixtures and cross-target compilation are implementation evidence, not successful native-platform acceptance evidence.

## Windows dependency check (2026-09-23)

`windows-sys` resolves to 0.61.2, is dual-licensed MIT/Apache-2.0, and declares Rust 1.71 as its minimum supported version. The `microsoft/windows-rs` upstream repository is active, with release tag 74 published on 2026-09-03 and commits on 2026-09-18 and 2026-09-19. The locked dependency tree also shows existing project dependencies including Tokio, rustix, and reqwest already use `windows-sys`; the new direct use does not introduce an unfamiliar platform stack.
