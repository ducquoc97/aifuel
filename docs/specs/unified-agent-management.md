# Unified local agent management

## Problem Statement

Users with Codex, Claude Code, GitHub Copilot CLI, Antigravity CLI, and Gemini CLI must learn different commands, model identifiers, effort controls, session behavior, and MCP configuration. They want one global AI Fuel interface that selects the intended local agent and runs a prompt or repository task with predictable results.

AI Fuel already delegates explicit Agent Runs and provides quota monitoring and an external MCP Gateway. It does not yet provide a unified model/effort catalog, interactive selection, normalized streaming, persistent provider session associations, or an execution MCP interface. Existing access-mode flags are provider mappings, not proof that every adapter enforces equivalent permissions. Earlier translation and registration checks do not establish complete MCP, permission, model, or platform acceptance.

## Solution

Provide one application contract for local Agent Run management, accessible through a global terminal picker, scriptable CLI, and a separately registered execution MCP endpoint. Users explicitly select provider, model, and effort, directly or through named global profiles. AI Fuel resolves the request, checks capabilities and permissions, invokes the correct local Agent Integration, and exposes progress, results, cancellation, approvals, and same-provider resume.

Cover all five agents. This release requires live acceptance on WSL; native Windows, macOS, and other Linux targets are deferred as recorded outstanding acceptance. Expose reduced capabilities and verified blocked access modes explicitly. Keep monitoring, execution, and external MCP routing separate responsibilities with shared configuration and application workflows where appropriate.

## User Stories

1. As a user, I want one globally available command, so that I can use AI Fuel from any directory.
2. As a terminal user, I want an interactive provider/model/effort picker, so that I do not need to remember provider syntax.
3. As a script author, I want explicit flags for the same choices, so that runs are repeatable without interactive prompts.
4. As an MCP caller, I want the same selection rules, so that automation behaves like the terminal.
5. As a user, I want all five local agents represented, so that I can manage my existing subscriptions together.
6. As a user, I want explicit provider selection even for shared model names, so that AI Fuel uses the intended integration.
7. As a user, I want provider-reported models, so that I can choose from observed catalog data.
8. As a user, I want model-specific effort options where known, so that the interface reflects the selected model.
9. As an advanced user, I want an explicit model-ID override, so that incomplete discovery does not prevent a deliberate attempt.
10. As a user, I want unknown support labelled clearly, so that an override is not mistaken for verification.
11. As a user, I want separate provider-listed and run-tested labels, so that discovery and successful execution remain distinguishable.
12. As an offline user, I want a timestamped cached catalog, so that I can inspect the last successful discovery.
13. As a user, I want explicit catalog refresh, so that I can check newly available models.
14. As a user, I want unsupported effort to use the provider default and report the change, so that the selected provider/model remains usable.
15. As a user, I want requested and reported effective settings distinguished, so that AI Fuel does not invent what the agent used.
16. As a user, I want named global profiles, so that I can reuse common selections.
17. As a user, I want explicit inputs to override profiles and global defaults, so that each run remains under my control.
18. As a terminal user, I want to choose prompt-only scope or confirm a repository, so that task context is intentional.
19. As a user, I want read-only access by default, so that writes require explicit selection.
20. As a repository user, I want workspace-write access when enforceable, so that an agent can perform coding tasks within the chosen scope.
21. As an MCP user, I want repository starts restricted to locally configured allowed roots, so that callers cannot choose arbitrary workspaces.
22. As a user, I want unsupported permission requests blocked, so that labels match enforcement.
23. As a user, I want normalized streamed answers and progress, so that output is consistent across agents.
24. As a script author, I want structured events and results, so that I can consume runs without parsing terminal decoration.
25. As a user, I want final-answer-only support labelled when streaming is unavailable, so that reduced integrations remain useful.
26. As an MCP caller, I want a run ID immediately, so that long tasks do not depend on one long-running tool call.
27. As an MCP caller, I want progress, result, and cancel operations, so that I can manage the run explicitly.
28. As a user, I want cancellation and disconnect to clean up owned processes, so that abandoned runs do not continue unnoticed.
29. As a user, I want no default overall deadline, so that valid long tasks can finish.
30. As a user, I want optional profile/run deadlines, so that I can bound a specific task.
31. As a user, I want AI Fuel-owned connections bounded to ten seconds for setup, so that unreachable services fail promptly.
32. As a user, I want concurrent runs across worktrees with exclusive writes inside one worktree, so that independent tasks can proceed without competing AI Fuel writers.
33. As a user, I want same-provider session resume, so that follow-up work retains native context.
34. As a user, I want supported model/effort changes on resume, so that I can adjust later turns explicitly.
35. As a user, I want failed or cancelled sessions resumable when a provider session ID exists, so that useful context is not lost.
36. As a user, I want permissions revalidated on resume, so that an old session cannot bypass current restrictions.
37. As a user, I want ordinary questions and approval requests surfaced as waiting states, so that runs do not hang invisibly.
38. As a local user, I want permission responses handled through a local terminal operation, so that the execution MCP interface cannot approve its own requests.
39. As a user, I want approvals constrained by the run's existing limits, so that expanding access requires a deliberate new run.
40. As a user, I want external MCP definitions managed once through CLI commands, so that all agents reuse the same catalog.
41. As a user, I want per-agent MCP selections and per-run restrictions, so that only intended external tools are exposed.
42. As a read-only user, I want external tools disabled unless explicitly allowlisted for that mode, so that tool annotations are not treated as permission proof.
43. As a user, I want child agents excluded from AI Fuel execution tools, so that they cannot recursively launch AI Fuel runs.
44. As a user, I want failures and partial output preserved without automatic provider switching, so that I decide what happens next.
45. As a user, I want missing installation/login guidance, so that I can repair provider setup using native tools.
46. As a user, I want AI Fuel to retain metadata without prompts/answers by default, so that persistent content storage is opt-in.
47. As a user, I want completed results available within documented retention limits while the owning connection remains open, so that an MCP caller can retrieve them or receive an explicit eviction result.
48. As a user, I want native transcript storage described separately, so that AI Fuel's retention setting does not imply control over agent-owned history.
49. As a maintainer, I want provider behavior implemented in compiled adapters, so that new capabilities do not require duplicated CLI/MCP routing.
50. As a release owner, I want versioned live evidence for every target platform/provider combination, so that fixture tests are not mistaken for working integrations.

## Implementation Decisions

### Authority and existing architecture

- The explicitly answered interview decisions and the subsequently requested specification corrections are normative. Five-agent acceptance on WSL, connection-owned runs, and supported approval forwarding remain requirements; native Windows, macOS, and other Linux acceptance are deferred beyond this release. A daemon, WSL-first release gate, detached runs, and default persistent content retention are not part of this release.
- Reuse the domain contracts, application facade, compiled provider registry, provider-local adapters, and existing MCP transport boundaries established by ADR-0001 through ADR-0004. Add separate model-catalog and run-lifecycle capabilities where needed. Avoid a generic strategy framework until multiple real sources require it.
- The unified endpoint is one versioned application API consumed by CLI, picker, and execution MCP. Each invocation owns its run manager; an execution MCP process owns runs for its stdio connection. There is no network listener or shared run daemon. Run inspection/results/cancellation use the owning invocation; global active-run browsing, reconnecting to an active run, and transfer of run ownership are outside this release. A new invocation may explicitly resume a persisted native Agent Session as a new Agent Run.
- Cross-process functionality is limited to cooperating write locks and pending local approval delivery. An owner with a pending permission request exposes an ephemeral same-user IPC endpoint: a Unix-domain socket on Linux/macOS/WSL or a named pipe with a user-restricted ACL on native Windows. A private per-user discovery record maps the run and owner instance to this endpoint. It is created only when needed and removed on owner exit. The local approval command connects directly to the owning process; it does not take ownership of the run or persist approval payloads. Platform peer/ACL checks and owner-instance validation prevent other OS users or stale records from supplying responses. Existing ten-second connection setup policy applies to this IPC.
- The executable composes concrete adapters. CLI, picker, and MCP share request resolution, capability checks, lifecycle, session, and policy behavior. Monitoring remains independent.
- Support Codex, Claude Code, Copilot, Antigravity via agy, and Gemini CLI. Do not equate executable presence, Provider Discovery, Account Entitlement, or Execution Availability.

### Selection, catalog, and profiles

- For new sessions, selection precedence is explicit CLI flags/MCP arguments, then selected named profile, then global defaults. Profiles are global and saved only by an explicit user operation. Expose resolved settings and their source so precedence is understandable. Permission policy is a ceiling applied after preference resolution, never a lower-precedence setting that a flag can override.
- Missing required selection opens the terminal picker in an interactive terminal. Non-interactive CLI and MCP return an actionable validation error. A missing effort uses the provider's own default.
- Model discovery is separate from metadata-only Provider Discovery and quota collection. Prefer provider-supported structured interfaces; preserve exact provider IDs and model identifiers. Historical usage or a quota label alone must not populate an authoritative Advertised Model catalog.
- Catalog entries include provider, model ID, display label, provenance, discovery timestamp, available effort evidence, and independent advertisement, entitlement, and execution observations. Unknown values remain unknown.
- Cache successful catalogs with timestamps. Refresh failure retains prior data marked stale and reports the error; it must not become a successful empty catalog. An explicit override is allowed without turning unknown support into verified support.
- Cache and execution observations are scoped by provider, native integration version, platform, and provider account/billing context when reported. Model-specific effort evidence also includes the model ID. Refresh catalogs after five minutes by default; a lookup failure returns cached data as stale with its age. Account or integration-context changes make prior account-specific evidence inapplicable until refreshed. Never store credential values as cache keys. If account identity cannot be established, label it unknown and do not use historical success to assert entitlement under the current login. Account-independent catalog evidence may be reused only when its provenance establishes that scope.
- Provider-listed and run-tested are distinct evidence labels. Run-tested records identify observed model/effort, integration version, platform, and time where available; they do not guarantee current entitlement or future acceptance.
- Effort values remain model/provider-specific. Known unsupported effort is omitted or reset through the provider-supported default mechanism, with an explicit fallback report and no model/provider change. If support is unknown, attempt the requested value; retry with the default only after an unambiguous unsupported-effort rejection before execution began. Never replay a task after partial execution or an uncertain outcome.
- Record requested effort, applied setting, fallback reason, and effective effort only if the provider reports it. Omitting an effort flag may use native configuration; do not claim a specific effective level without evidence.

### Agent Run lifecycle and sessions

- Extend the existing Agent Run facade into the common management boundary. Operations cover listing integrations/capabilities/models, resolving requests, starting runs, observing ordered progress, retrieving results, cancelling, answering supported ordinary questions, and resuming Agent Sessions.
- Execution MCP returns a run ID promptly after acceptance and exposes progress/result/cancel operations. Validation failures occur before provider execution. Run IDs are opaque and owned by the originating connection; knowing an ID alone does not grant access from another MCP connection.
- Expose starting, running, waiting-for-input, waiting-for-approval, and terminal outcomes. Preserve provider diagnostics and partial answers separately from normalized user output. A native error or incomplete structured result must not be labelled successful solely because its process exited zero.
- Prefer native structured streaming and lifecycle interfaces. Final-answer-only support is acceptable with an explicit capability label. Workflows needing unsupported interactive approvals are blocked with guidance rather than silently hanging.
- Active runs end when explicitly cancelled, their optional overall deadline expires, or their owning terminal/MCP connection closes. Stop and reap only owned processes and descendants; cancellation does not undo already completed external effects. Cancelling one run must not cancel unrelated runs.
- There is no default overall task deadline and no silence-based task cancellation. AI Fuel-controlled connection setup has a ten-second timeout. This does not set native agents' provider HTTP/WebSocket timeouts and does not change unrelated gateway operation deadlines.
- Retain Agent Session associations separately from individual Agent Runs. Persist valid provider session IDs when returned. Support explicit same-provider resume after successful, failed, or cancelled runs where the native session remains available.
- Resume may explicitly change model/effort if supported. Revalidate current access, root policy, and external tool restrictions on every resume. Reject continuation if the provider cannot apply required restrictions or session context cannot be safely reconciled with the requested workspace. Do not silently start a new conversation instead of resuming.
- Resume selection precedence is explicit model/effort inputs, then an explicitly selected profile's model/effort values, then the last session selection. Global provider/model/effort defaults never silently replace session settings. Any explicitly supplied or profile-supplied conflicting provider is rejected. Record whether inherited effort means a concrete applied value or provider-managed default; native changes can make the effective value unknown. Resume does not inherit permission grants: resolve the new access request, defaulting to read-only, and reapply current root and tool policy.

### Public run-management contract

The following operation names define the application contract and execution MCP tools. CLI commands map to these operations rather than implementing separate workflows. Use schema version 1 for the new execution contract; it does not replace the existing monitoring schema.

| Operation | Request | Observable response |
| --- | --- | --- |
| list_agents | Optional provider filter | Native presence/version, independently supported capabilities, and reasons for unsupported or unknown capabilities |
| list_models | Provider and explicit refresh option | Provider-scoped catalog, effort evidence, observation context, age, and refresh diagnostics |
| resolve_run | Provider/model/effort/profile, scope, access, tool restrictions, optional deadline | Fully resolved request and setting sources, policy/capability verdict, and fallback warnings; no Agent Run is started |
| start_run | New-session request | Accepted run ID and initial state, or validation error without launching an agent |
| resume_session | AI Fuel session ID and explicit overrides | A new run ID associated with the same provider session, or a session/capability/policy error |
| get_run | Owned run ID | Current state, timestamps, settings, capabilities, pending input/approval identity where applicable, and content availability |
| read_events | Owned run ID, optional opaque cursor and page byte limit | Ordered events, next cursor, explicit gap indicator, and terminal-state indicator |
| get_result | Owned run ID | Pending state, or terminal outcome plus available answer/diagnostics and truncation/content-availability flags |
| cancel_run | Owned run ID | Current state; repeated cancellation is idempotent and terminal results are unchanged |
| answer_input | Owned run ID, pending ordinary-input ID, response | Acknowledged response or stale/conflicting/unsupported-input error; never grants permissions |
| approve locally | Run ID, pending permission ID, local grant/deny response | One response delivered to the owning process over local IPC, within immutable run limits; not an execution MCP tool |

- Resolve again at start/resume against current policy and capabilities. A preview is informational, not a stored authorization. Metadata-only validation and capacity/write-lock admission precede provider launch. Slow native startup occurs in the starting state after acceptance so the MCP call can return promptly.
- State transitions: accepted runs begin in starting, then running; supported questions enter waiting-for-input or waiting-for-approval and return to running after an accepted response. Any active state may enter cancelling, then cancelled or timed-out after cleanup. Provider startup/execution may terminate as failed; normal completion is succeeded. Terminal outcomes are immutable. A cancel request received after completion returns that outcome without rewriting it.
- Serialize event ordering within each run. Each public event has schema version, run ID, increasing sequence number, timestamp, type, and typed payload. Events distinguish answer fragments, progress, diagnostics, waiting requests, setting fallback, and terminal outcome. Do not expose hidden reasoning as normalized answer text. Only provider-exposed public events are eligible for output.
- Event reads are non-consuming and paginated, defaulting to a maximum 256 KiB payload and capped at 1 MiB per page. Cursors are scoped to their run/owner; wrong-run or malformed cursors fail. A cursor older than the retained buffer returns an explicit gap and the earliest available sequence. Repeated result reads do not delete content. A terminal outcome remains visible independently of answer/event eviction.
- Stable execution error categories cover invalid_request, unsupported_capability, policy_denied, agent_unavailable, authentication_required, model_unavailable, provider_failed, connection_timeout, deadline_exceeded, resource_limit, write_conflict, session_unavailable, run_not_found, invalid_cursor, and input_conflict. Unauthorized cross-connection run IDs return run_not_found without exposing another run's details. Native diagnostics are separate optional content, not the stable error code.
- Start/resume are effectful operations. The MCP layer must not automatically replay them after an uncertain delivery failure. Preserve the accepted run ID when available and query that run rather than starting another. Same-provider session resume also requires no currently active AI Fuel run for that session, including read-only runs, to avoid competing native conversation turns.
- A repeated identical response for a resolved input ID is acknowledged without forwarding again; a conflicting, cancelled, or expired input ID is rejected. Classify ordinary questions versus permission requests from native protocol evidence. If the adapter cannot distinguish them, block the response capability rather than routing a permission decision through answer_input.

### Scope, permissions, approvals, and concurrency

- The terminal picker asks users to choose prompt-only scope or confirm the repository. Scripted requests express scope explicitly; no repository scope is inferred merely from the invocation directory. Prompt-only execution does not imply an OS sandbox by itself.
- Read-only is the default. Workspace-write is explicit. Only expose an access mode as supported after the relevant adapter/platform enforcement is established. An agent planning flag or sandbox flag alone is not proof. Reject unsupported modes; building new OS isolation is outside this release.
- Read-only prohibits task-directed filesystem creation, modification, and deletion, including through shell tools. Workspace-write allows those effects only inside the resolved workspace; symlink targets outside it do not inherit write permission. Neither mode automatically grants arbitrary shell commands or remote writes. External MCP effects are separately controlled by the exact selection and read-only allowlist. Enforcement must cover the native agent's applicable file and shell tools, not merely AI Fuel path validation.
- Read access follows the native provider policy and may extend beyond the workspace. Allowed roots are workspace-admission controls, not a read confidentiality sandbox. Prompt-only runs receive an isolated temporary working directory and no implicit repository attachment; this alone does not guarantee inability to read the wider filesystem. Expose this read boundary in resolved capabilities so callers can reject it when unsuitable.
- Native agent-owned session, authentication, cache, and operational bookkeeping may write outside the task workspace under native settings. This exception does not authorize model-directed writes to those locations or arbitrary user files. Each adapter's capability evidence must distinguish native bookkeeping from task-directed tools. If that distinction cannot be enforced for the claimed mode, block that mode. AI Fuel itself does not refresh/write provider credentials.
- For execution MCP, canonical repository paths must be within roots configured by the local user. Root admission and runtime filesystem permissions are separate checks. Path aliases must not bypass admission or write coordination.
- Only the local user changes allowed roots, read-only external tool allowlists, profiles, and content-retention policy. MCP callers select profiles and request restrictions within policy; they cannot expand policy through run arguments or approval responses.
- The owning MCP caller may answer ordinary agent questions. Permission requests require the local user through an AI Fuel terminal operation, unless already preapproved within policy. Approval is bound to a particular active run and pending request, cannot be reused, and cannot expand run limits. A broader request requires a policy change and a new run.
- Local approval separates supported interfaces; it does not prove human presence against code already running as the same OS user. The execution MCP interface exposes no permission-grant operation; managed child launches reject the local approval command and do not inherit approval-channel credentials. This is not a claim that an environment marker or same-user IPC defeats a malicious same-user process. Human-attested approval would require a separately designed trusted channel and is outside this release. Even an accepted approval cannot expand the run's enforced roots/access/tool ceiling.
- Coordinate writes across AI Fuel CLI and MCP processes for the same OS user. Different Git worktrees may run independently; only one workspace-write run is allowed within one worktree. Canonical overlapping non-Git workspaces also conflict. Reject conflicts with the active run ID; do not queue silently. Locks must release after run exit/crash. Unrelated editors and native agent processes are outside this coordination guarantee.
- Git worktrees share references, object storage, and administrative state. Per-worktree write coordination protects working-file operations only; it is not serialization of Git operations across the shared repository. Permission resolution must account for actual shared metadata write destinations. Such effects are not implicitly authorized by workspace-write access. Do not add automatic commits, branch switching, worktree removal, or shared-repository mutation to the runner. Document the separate native Git-policy boundary and test that distinct worktree locks are not presented as protection of shared Git state.

### MCP roles and external tool selection

- Preserve the read-only AI Fuel MCP Server. Add a distinct execution MCP entrypoint registered explicitly. Keep the AI Fuel MCP Gateway responsible for external tools; it is not the Agent Run launcher.
- Retain central external server definitions, defaults, and exact per-MCP Host selections. Add CLI list/add/remove/validate/select operations for servers and tools, building on the existing catalog and managed registration workflow. Extend Agent MCP Registration to Gemini as required for five-agent coverage.
- A run may restrict or disable its selected external MCP tools. It cannot broaden the locally allowed selection. Snapshot the resolved selection for the run so later catalog edits do not silently grant new tools.
- Enforce selection both in exposed tools and tool-call routing. Suppress unrelated native MCP registrations for managed restricted runs through supported invocation/configuration isolation. Reject the run when exact selection cannot be enforced; avoid permanently rewriting unrelated native settings.
- Read-only external tools are disabled unless the local user explicitly allowlists them for that mode. An annotation alone does not establish safe behavior; an allowlist is a user trust decision, not a proof about a remote service.
- Do not expose AI Fuel execution MCP tools to child agents. Reject nested AI Fuel execution attempts within the managed launch chain. This is a recursion control, not a promise of isolation from all same-user software.
- Preserve existing external transport, credential reference, routing, and registration ownership contracts. Do not install MCP packages or refresh monitoring credentials as part of this feature.

### Storage, diagnostics, and supported versions

- Persist an allowlisted metadata schema: run/session identifiers, provider and reported account context, selected/applied/reported settings, integration version/platform, timestamps, workspace associations needed for resume, state, stable outcome/error code, and content availability. Do not persist free-form provider errors or diagnostics by default. Prompts, answers, tool arguments/results, approval descriptions, ordinary input, and diagnostic payloads all count as content, even when embedded in an error. Apply the content-retention opt-in to all of them and keep secrets redacted in either mode.
- Without content retention, results stay temporarily available only for the owning connection's lifetime and within the limits below. On connection closure discard AI Fuel-held content; persisted metadata records that content is unavailable. Native transcripts are not copied into AI Fuel to reconstruct evicted answers. Content retention, when explicitly enabled, uses private local storage and does not expand what other MCP connections may retrieve.

| Execution resource | Default limit | Behavior at limit |
| --- | --- | --- |
| Active Agent Runs per owner process | 4, including starting and waiting states | Reject new starts with resource_limit before launch; do not cancel existing runs or silently queue |
| Completed results with in-memory content per owner | 32 | Evict oldest completed content first; retain terminal metadata and report content unavailable |
| Retained event payload per run | 8 MiB | Drop oldest events, report a sequence gap, and continue draining native output |
| Retained final answer plus diagnostics per run | 8 MiB | Truncate with explicit byte counts/flags; preserve terminal outcome separately |
| Total in-memory event/result payload per owner | 128 MiB | Evict completed content first, then trim event buffers; report all gaps/truncation |
| Total accepted run records per owner connection | 1,024 | Reject further starts with resource_limit; existing IDs remain inspectable until disconnect |

- These are documented initial execution defaults, adjustable by local configuration only within validated bounds. They are per owner, not a machine-wide resource guarantee. Existing Gateway budgets remain separate. Keep reading provider pipes after output retention reaches a cap so bounded storage cannot deadlock provider completion. Bound native frame decoding and IPC payloads as well; malformed/oversized protocol frames fail explicitly rather than accumulating unbounded memory.
- Native agent transcript storage remains controlled by each provider's settings. Explain this separately from AI Fuel retention; do not promise erasure of native session history.
- Detect missing agents and unavailable authentication and provide native setup instructions. Users own installation/login. A failed quota lookup alone is not evidence that Agent Runs cannot authenticate.
- Report failures and partial output; the user chooses retry or a different provider. No automatic provider/model switching, generic task replay, or quota-driven routing.
- Record tested versions and check required capabilities. Newer versions are not blocked solely because their version is untested, but unproven permission/restriction modes remain unavailable. Capability detection must not invoke arbitrary words as native prompts or initiate login unintentionally.

### Delivery sequence and dependencies

1. Establish the five-agent capability/evidence matrix and identify one native integration with a provable access mode for the first complete workflow. Correct existing metadata inconsistencies. Interface and enforcement research precede dependent capabilities; it does not require completing every catalog before run management begins.
2. Deliver explicit selection through start, progress, result, and cancel in both CLI and execution MCP for that integration. Include owner isolation, disconnect cleanup, budgets, and one real prompt plus denied-effect test. This is an internal milestone, not reduced release coverage.
3. Extend the same workflow to all five adapters, with their supported effort handling and labelled reduced/blocked capabilities. Add repository execution and cross-process write/session coordination before enabling concurrent repository runs. Exercise the same contract fixtures through each adapter.
4. Deliver model discovery, scoped caches, profiles, resolved settings, and the terminal picker through the existing workflow. This can run alongside provider expansion; release still requires honest catalog/unknown reporting for all five.
5. Add same-provider resume and supported questions/approvals through CLI and MCP, including local IPC delivery, selection inheritance, policy revalidation, and stale-response handling. Each new capability includes a complete user-facing acceptance path.
6. Complete central external MCP management and exact per-run restrictions through the shared Gateway. It can progress alongside run/session work, but tool-enabled Agent Runs depend on both capability and policy enforcement. Preserve independent Gateway process sessions.
7. Complete installed and live verification across the WSL target, resolve review findings, and publish the exact tested/blocked matrix. Native Windows, macOS, and other Linux cells remain recorded outstanding acceptance. CI and live evidence accumulate with each workflow milestone rather than being deferred entirely to this phase.

## Testing Decisions

- Primary seam: the public application run-management contract, driven through the actual CLI and execution MCP protocol. Use controlled local agent processes and external MCP servers for repeatable tests. Add narrow parser tests only where wire-format edge cases cannot be covered clearly through this boundary.
- The user confirmed this public CLI/MCP interface test boundary, with controlled local agents and separate live provider acceptance on the agreed platforms.
- Prior art includes real-binary launcher tests with fake executables, application-facade dispatch/cancellation tests, gateway transport and process-lifecycle tests, monitoring MCP requests, and registration tests using temporary configuration.
- Good tests assert observable contracts: correct selected provider and arguments, result/error semantics, denied effects, preserved sessions, and cleaned-up owned processes. Do not treat argument presence or exit zero alone as proof of permissions, MCP execution, or completed work.
- Selection cases cover profile precedence, interactive versus non-interactive missing inputs, known/unknown model support, stale caches, capability/version changes, and provider-listed versus historical run evidence.
- Verify account-context changes do not reuse entitlement evidence; unknown account identity does not acquire run-tested authority from another login. Verify resume ignores changed global model/effort defaults, accepts explicit supported overrides, and rejects provider conflicts.
- Effort cases cover supported effort, known unsupported fallback, explicit rejection before execution, unknown effective defaults, and no replay after output or uncertain effects.
- Lifecycle cases cover prompt/file/stdin inputs, streamed versus final-only outputs, waiting states, cancellation, optional deadlines, silent but active work, disconnect cleanup, result lifetime, and same-provider resume including failed/cancelled sessions.
- Verify event ordering, cursor scoping/gaps, repeated result reads, idempotent cancellation and ordinary-input delivery, terminal-state immutability, unknown run errors, and no automatic start/resume replay after delivery uncertainty. Exercise active-run and retained-record limits before any child launches, plus output/result eviction while the provider continues producing data.
- Policy cases cover denied writes, writes outside the permitted workspace, root/path alias escapes, restricted native MCP configurations, rejection of tools not selected, refusal of nested AI Fuel runs, ordinary-input versus permission-approval authority, and rejection of stale or replayed approvals.
- Verify agent bookkeeping remains possible while model-directed file/shell writes are denied in read-only mode. Record the actual read boundary and shared Git metadata boundary. Local approval IPC tests cover owner-instance binding, OS-user restrictions, cleanup, conflicting responses, managed-child rejection, and absence of a permission-grant MCP operation; do not claim these tests establish human presence against arbitrary same-user code.
- Cross-process tests cover concurrent independent worktrees, conflicting writes from CLI and MCP processes, path aliases, owner failure and lock cleanup, and unauthorized cross-connection run access.
- Retention tests prove content is absent from AI Fuel persistent state by default, disappears when the owner disconnects, and is retained only under opt-in policy. Native provider transcript behavior is reported separately.
- Include provider errors containing prompt text, tool outputs in diagnostics, and pending approval descriptions in retention tests. Verify bounded content is evicted/truncated explicitly, terminal metadata survives until owner closure, and evicted data is not reconstructed by reading native transcripts.
- Preserve existing status CLI/dashboard/read-only MCP and Gateway behavior. Verify registration preview, repeat, conflicts, removal, and preservation of unrelated configuration.
- Public picker acceptance covers the main select/confirm/cancel path using a controlled terminal; avoid tests tied to visual layout details.
- Live acceptance must record the exact installed AI Fuel version, native agent version, OS, model/effort evidence, result, and supported or blocked permission/MCP capabilities. Use disposable workspaces for effect checks and never commit credentials.
- Each successful integration needs the exact translation prompt from the conversation, a representative repository task in an enforceable mode, selected external MCP tool invocation with server-side call evidence, cancellation, and resume where supported. Model discovery or registration listing does not substitute for these paths.
- Build/fixture CI and live acceptance are separate evidence. The required platform matrix is below; these cells are requirements, not results from this spec.

| Agent Integration | WSL | Native Windows | macOS | Linux |
| --- | --- | --- | --- | --- |
| Codex | Verify live | Deferred | Deferred | Deferred |
| Claude Code | Verify live | Deferred | Deferred | Deferred |
| GitHub Copilot CLI | Verify live | Deferred | Deferred | Deferred |
| Antigravity CLI | Verify live | Deferred | Deferred | Deferred |
| Gemini CLI | Verify live | Deferred | Deferred | Deferred |

- Verified blocked access modes and labelled reduced capabilities are acceptable. An untested WSL/agent combination is not release-ready; deferred non-WSL cells are recorded as outstanding acceptance, never silently counted as passed. Missing hardware, credentials, or native interfaces must likewise be recorded as outstanding acceptance.

## Out of Scope

- Automatic provider selection, quota-based routing, provider/model switching, and cross-provider session handoff.
- A new public HTTP API, remote hosted service, always-running agent daemon, or detached runs that survive their owning client disconnecting.
- Building an OS sandbox, enforcing policy against all unrelated same-user software, or claiming that the Gateway makes remote tools inherently read-only.
- Human-attested approval, global active-run attachment/reconnection, and automatic Git mutation or shared-Git-state serialization.
- Installing/updating agents or external MCP packages, managing provider login, or editing environment-variable files.
- Runtime-loaded provider plugins, every provider in the coverage catalog, model pricing/spend dashboards, and wholesale CodexBar parity.
- Web dashboard execution controls, project-specific profile overrides, and new credential-broker/account-switching functionality.
- Altering native transcript retention or guaranteeing that cancellation rolls back already performed actions.
- Implementation, deployment, issue closure, or a claim of completed acceptance as a result of publishing this specification.

## Further Notes

- This extends [the provider-adapter and centralized MCP Gateway specification](https://github.com/ducquoc97/aifuel/issues/24). Preserve its protocol and registration contracts except for the explicit five-agent and four-platform coverage expansion here. The execution MCP role requires its own documented domain term; it must not redefine the read-only AI Fuel MCP Server.
- Revision following specification review: define owner-local management and approval IPC, precise task effects versus native bookkeeping, resume precedence, a versioned operation/event/error contract, account-scoped catalog evidence, concrete buffering/admission limits, diagnostic content retention, and the shared Git-state boundary. Deliver complete CLI/MCP workflows incrementally. Preserve all five providers, all four platform targets, supported approvals, and cancellation/content cleanup on disconnect.
- CodexBar is a reference for provider-local source selection and parsing, not an established five-agent model/effort execution library. Inspect and pin any source used for a port, retain required attribution, and verify its behavior against the native agent. Do not import browser/Keychain/process-scanning fallback scope merely because the reference supports it.
- Reference architecture: https://github.com/steipete/CodexBar/blob/main/docs/provider.md . Reference provider sources: https://github.com/steipete/CodexBar/blob/main/docs/providers.md . These references motivate investigation; they do not prove AI Fuel runtime support.
- The agreed acceptance prompt is: "translate to Vietnamese: Fetch Codex redemption detail through account/rateLimits/read". It requests a translation, not an account API operation or credit redemption.
- Publishing as ready-for-agent authorizes implementation against these requirements. Live platform evidence and capability gaps remain explicit completion gates; this label does not assert that every native agent can satisfy every requested mode.
