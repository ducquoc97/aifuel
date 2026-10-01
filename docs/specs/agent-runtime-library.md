# Agent runtime library

## Problem Statement

Every local AI agent app reimplements the same machinery: wrapping provider agent interfaces, managing conversation sessions, streaming typed events, handling permission prompts, tracking models, and checkpointing work. AI Fuel already has this machinery embedded in its binary - Agent Integrations, Agent Sessions, Agent Runs, capabilities, approvals, and a per-user run store - but it is reachable only through AI Fuel's own CLI, dashboard, and MCP surfaces.

The reusable asset is the runtime contract, not the transport around it. This spec extracts that machinery into a portable agent runtime library that any local app can host: the AI Fuel binary itself, a Tauri or Electron GUI, a terminal UI, or a future remote layer that forwards the same contract over a network.

## Goals

- One typed event contract (`AgentEvent`) emitted by every provider adapter; no host ever renders raw terminal output.
- Agent Session and Agent Run lifecycle with a durable Session Event Log, so any consumer can detach and replay.
- A first-class approval/question channel: provider permission requests arrive as typed events and are answered through the same contract.
- Checkpoints per workspace-mutating Agent Run, enabling diff and restore.
- Model selection by IntegrationId plus model and effort, merging Advertised Model, Account Entitlement, Execution Availability, and Quota Pool observations.
- Two embedding surfaces: an in-process Rust trait, and a stdio JSON-RPC mode so apps in any language can host the runtime (the `codex app-server` pattern).
- Transport-agnostic: the library owns domain logic only. Network protocols, authentication, and multi-device concerns belong to the Host Application.

## Non-Goals

- Relay servers, remote control routing, device pairing, end-to-end encryption, push notifications. A future remote control plane is a Host Application that forwards this contract, not part of the library.
- Authentication, billing, entitlements, or any hosted service. The library assumes the local machine context.
- UI. Rendering typed events is the Host Application's job.
- Runtime-loaded provider plugins. Adapters remain compiled Rust per ADR-0003.
- Multi-user sessions. One runtime serves one machine user; authorization between users is a transport-layer concern.
- A full terminal/PTY abstraction. Provider processes are owned by adapters behind the contract.

## Domain Terms

This spec uses the CONTEXT.md terms Provider Integration, Integration Identity, Managed Credential, Credential Reference, Authentication Binding, Advertised Model, Account Entitlement, Execution Availability, Quota Pool, Agent Integration, Agent Session, Agent Run, and Monitoring Collection Contract. It adds the following terms, intended for CONTEXT.md after implementation.

**Host Application**:
Any local process that embeds the agent runtime library and exposes it through its own transport: the AI Fuel CLI and dashboard, a desktop GUI, a stdio bridge consumer, or a future remote relay. The Host Application owns transport, authentication, and UI.
_Avoid_: Client, frontend, server

**Session Event Log**:
The durable, per-session sequence of typed events persisted by the runtime, extending the existing per-user SQLite run store. It assigns each event a monotonic `seq` and powers replay for any consumer.
_Avoid_: Event store in the event-sourcing sense, chat history table

**Approval Request**:
A typed, blocking question from a running agent delivered to the Host Application: tool permission, plan approval, free-form question, or MCP elicitation. It carries explicit options and is answered through the contract.
_Avoid_: Prompt, dialog, notification

**Checkpoint**:
A hidden git ref recorded at the end of an Agent Run that mutates the workspace, enabling diff, restore, and PR flows per run.
_Avoid_: Snapshot, backup, commit

## Architecture

```text
Host Applications          Embedding surfaces          Agent runtime library
                                                             (Rust)
   aifuel CLI        - in-process Rust trait  ->   AgentRuntime facade
   aifuel dashboard  - stdio JSON-RPC bridge       ->  per-provider AgentAdapter
   Tauri / Electron      (`aifuel runtime`)        ->  Session Event Log (SQLite)
   future relay                                        ->  credential store rules
                                                              |
                                          provider protocols, not PTY scraping
                                          claude stream-json | codex app-server
                                          opencode serve     | ACP | CLI fallback
```

- Existing surfaces become thin hosts over the same contract: `aifuel run`, the dashboard's local server, and the `mcp execution` server are consumers, not parallel implementations. This spec does not break them; it defines the contract they sit on.
- The stdio bridge (`aifuel runtime`, JSON-RPC over stdio) is how non-Rust hosts embed the runtime. The bridge is one-runtime-per-process, matching the `codex app-server` model: each consumer spawns its own process and owns the live sessions it starts, while the persisted run store remains shared. A shared multi-consumer daemon is a later decision, made only if a consumer needs to drive live runs it did not start.
- All wire shapes are versioned JSON (`v: 1` at the envelope/rpc level). TypeScript sketches below are illustrative; the Rust crate is the reference implementation, consistent with the repo's versioned-decoding rule.

## Contract

### Commands

Host to runtime. Every command carries a `commandId`; the runtime returns exactly one `Receipt`, making retries idempotent even when delivery is ambiguous.

```ts
type AgentCommand =
  | { type: "session.create";     commandId; cwd; selection: ModelSelection; access: AccessMode;
                                  resumeCursor?: string; externalTools?: string[] }
  | { type: "session.subscribe";  commandId; sessionId; lastSeenSeq: Seq }   // attach + replay
  | { type: "session.list";       commandId }
  | { type: "session.close";      commandId; sessionId }
  | { type: "run.start";          commandId; sessionId; input: UserInput }
  | { type: "run.cancel";         commandId; sessionId; runId }
  | { type: "approval.answer";    commandId; sessionId; requestId; decision: ApprovalDecision }
  | { type: "model.select";       commandId; sessionId; selection: ModelSelection }
  | { type: "checkpoint.restore"; commandId; sessionId; checkpointId }
  | { type: "integrations.list";  commandId }
  | { type: "models.list";        commandId; integrationId };

- `externalTools` is an exact-enforcement contract, not a hint: the serving adapter either restricts the provider session to exactly the declared AI Fuel Gateway tools or rejects `unsupported` before any provider transport starts. Where enforcement is a provider-side tool allowlist (Codex's `mcp_servers` thread config), the adapter gates session readiness on the provider reporting the gateway connected with exactly the selected set, on both session start and resume; the declared set is persisted on the session row so a startup continuation redeclares it rather than silently widening.
- `run.start`'s receipt carries the accepted `runId` so a host can issue `run.cancel` without waiting on `run.started` - the run is registered before the receipt flushes.

type AccessMode = "read_only" | "workspace_write" | "full";   // aifuel --access

interface UserInput {
  text: string;
  attachments?: { kind: "image" | "file"; path: string }[];  // host-resolved local paths
}

type Receipt =
  | { commandId; ok: true;  seq: Seq; sessionId?: SessionId; runId?: RunId; snapshot?: SessionSnapshot }
  | { commandId; ok: false; code: string; message: string };
```

Receipt `code` is a closed set: `unauthorized`, `unknown_session`, `already_resolved`, `invalid_selection`, `invalid_state`, `provider_error`, `unsupported`. Unknown codes are treated as `provider_error` by consumers; the set grows only by contract version.

### Events

Runtime to host. Every emission is one typed `AgentEvent`; `seq` is assigned by the Session Event Log before delivery to any consumer.

```ts
type AgentEvent = { sessionId; seq: Seq; ts: string } & (
  | { type: "session.created"; integrationId; cwd }
  | { type: "session.status"; status: SessionStatus }
  | { type: "session.closed"; reason?: string }
  | { type: "run.started";   runId; selection: ModelSelection }
  | { type: "run.completed"; runId; outcome: "success" | "failed" | "cancelled"; usage?: Usage }
  | { type: "message.delta";     runId; stream: "assistant" | "thinking"; text }
  | { type: "message.completed"; runId; stream: "assistant" | "thinking" }
  | { type: "tool.started";   runId; tool; summary }
  | { type: "tool.completed"; runId; tool; ok; diff?: FileDiff; output?: string }
  | { type: "todos.updated";  runId; items: TodoItem[] }
  | { type: "approval.requested"; runId; requestId; request: ApprovalRequest }
  | { type: "approval.resolved";  requestId; decision; answeredBy: string }  // consumer channel id
  | { type: "checkpoint.created";  runId; checkpointId; diffstat }
  | { type: "checkpoint.restored"; checkpointId }
  | { type: "quota.observed"; integrationId; quota: QuotaSummary }
  | { type: "error"; runId?; code; message; retryable: boolean }
);

type SessionStatus =
  | "idle" | "working" | "waiting_approval" | "compacting" | "interrupted" | "closed";
```

- A stream that ends without a terminal `run.completed` or `error` is a failure, matching the existing rule that exit zero on a structured protocol is not proof of a completed turn.
- The runtime never auto-replays an inference request after an ambiguous disconnect; the request may have been consumed and billed. Replay is a user decision, per the existing no-replay rule.
- `quota.observed` is emitted post-run when the integration's Monitoring Collection Contract reports. This is the differentiator: no comparable library surfaces Account Entitlement and Quota Pool state next to model selection.

### Subscribe and replay

- `session.subscribe { lastSeenSeq }` returns a `SessionSnapshot` plus all events with `seq > lastSeenSeq`, in order. A UI reload, dashboard reconnect, or bridge restart reattaches without losing in-flight Agent Runs.
- Replay is bounded on two axes: event count and payload bytes. When `lastSeenSeq` lags `headSeq` beyond either bound, the runtime skips replay and returns a fresh `SessionSnapshot` instead - the same fallback T3 Code uses (`afterSequence` bounded by `THREAD_RESUME_MAX_EVENTS` and `ORCHESTRATION_REPLAY_PAYLOAD_BUDGET_BYTES`). The bounds are internal guard constants with conservative defaults, tunable between versions; they are not part of the versioned contract surface.
- `SessionSnapshot` is the materialized read model: status, `ModelSelection`, cwd, pending Approval Requests, Checkpoints, `headSeq`, and `tail`.
- `tail` is bounded semantically, not numerically: it carries the transcript items of the most recent Agent Run - the last turn - which is what a UI needs to render current state on reattach. Older turns are paginated by `seq` on demand.
- Host Applications persist `lastSeenSeq` per session across consumer restarts, so resubscription is a fast replay rather than a cold snapshot.
- Multiple consumers may subscribe to one session (dashboard plus MCP host). The event log serializes order. When two consumers answer one Approval Request, the first wins and the second receives `already_resolved`; the `approval.resolved` event's `answeredBy` keeps every consumer's UI honest.

### Host restart and resume

- On graceful shutdown the runtime marks its own in-flight Agent Sessions `interrupted` and persists each session's provider resume cursor where the adapter supports it. Each Agent Session row records a process-instance owner so two runtimes sharing one store never mark or adopt each other's live sessions.
- On startup it reconciles only persisted sessions whose recorded owner is gone, mirroring T3 Code's `reconcileProviderSessions`: where the adapter declares `resume`, the runtime attempts provider-side continuation, claims the session's owner, and emits `session.status` (`working` on success, `interrupted` on failure). The claim is a compare-and-swap on the dead owner observed at enumeration and lands before any provider attach, so two runtimes racing one orphaned session cannot double-attach it. Sessions on adapters without `resume` stay `interrupted` permanently; continuing work means a new session.
- Interruption is recorded as a fact in the Session Event Log and continuation is attempted, never assumed.

## Approvals

- Approval Request kinds: `tool_permission`, `plan_approval`, `question`, `mcp_elicitation`. Each carries `title`, `detail`, explicit `options`, and `requiresConfirm`. Requests translated from a typed provider interaction also retain its `interactionKind`, normalized `questions`, native `parameters`, and `nativeMethod`, so a host can reconstruct the provider-native ask and answer it directly.
- `ApprovalDecision` is a union: `optionId` for declared options, `text` for free-form input, `answers` carrying one answer list per question id for multi-question asks, and `elicitation` carrying the raw MCP elicitation JSON. Answers bind to the questions the request declared - an answer id the request never asked is rejected rather than forwarded. Decision content can carry credential material (an elicitation exists to collect it), so the durable log stores `approval.resolved` with the decision's content scrubbed - which questions were answered survives, what was answered does not - while the full decision still reaches live consumers.
- `requiresConfirm` means the Host Application should re-authenticate the user (biometric, PIN, confirm dialog). The runtime cannot verify device biometrics; it records which consumer answered.
- Pending Approval Requests are durable in the event log: they survive consumer disconnects and appear in `SessionSnapshot` until resolved.
- Answering is never implicit. The runtime never auto-approves, consistent with the existing rule that permission requests are never auto-approved. A consumer that disappears leaves the request pending, not granted.

## Checkpoints

- On `run.completed` for `workspace_write` or `full` runs, the runtime records a hidden git ref and emits `checkpoint.created` with a diffstat. `read_only` runs produce no Checkpoint because they produce no diff.
- `checkpoint.restore` resets the workspace to that ref. It is an explicit command because it discards working state; the runtime records it in the event log like any other fact.
- This borrows T3 Code's turn-checkpoint model and is what makes per-run diff review, undo, and one-button PR flows possible for hosts.

## Model selection

```ts
interface ModelSelection { integrationId: IntegrationId; model: string; effort?: Effort }
type Effort = "low" | "medium" | "high" | "max";

interface ModelDescriptor {
  provider: string;
  model: string;
  label: string;
  efforts: Effort[];                 // empty = fixed effort
  advertised: boolean;               // Advertised Model
  entitled: boolean | "unknown";     // Account Entitlement
  availability: ExecutionAvailability; // ready | needs_auth | unsupported | unknown
  quota?: QuotaSummary;              // Quota Pool headroom when the integration monitors it
}
```

- Selection is by Integration Identity, never a `provider:mode` compound parsed for routing - the same rule `provider-integrations.md` applies to `aifuel run`.
- `models.list` merges the Advertised Model catalog with Account Entitlement and Execution Availability for that integration's account. `session.create` and `model.select` call `resolve` first; a selection that is not `ready` fails with `invalid_selection`. The runtime never silently falls back to another model or another credential.
- The runtime never substitutes billed API execution for subscription execution. Ambiguity is an error, not a guess, per the existing no-silent-switch rule.

## Agent adapter contract

One adapter per provider protocol. Complexity lives at the adapter boundary; the runtime facade and hosts stay provider-agnostic. Adapters wrap provider protocols - stream formats, JSON-RPC servers, HTTP APIs - not terminal output:

| Adapter | Wraps |
| --- | --- |
| `ClaudeAdapter` | `claude` CLI stream-json mode (`--output-format stream-json`) |
| `CodexAdapter` | `codex app-server` (JSON-RPC over stdio) |
| `OpenCodeAdapter` | `opencode serve` HTTP API / SDK protocol |
| `AcpAdapter` | Agent Client Protocol (covers Cursor and other ACP-speaking agents) |
| `CliAdapter` | Fallback for providers exposing only a one-shot CLI |

```rust
trait AgentAdapter {
    fn capabilities(&self) -> AdapterCapabilities;
    fn resolve(&self, selection: &ModelSelection) -> Result<ModelDescriptor>;
    fn list_models(&self, integration: &IntegrationDescriptor) -> Result<Vec<ModelDescriptor>>;
    fn start(&self, integration: &IntegrationDescriptor, opts: StartOptions)
        -> Result<AgentSessionHandle>;
    fn send(&self, h: &AgentSessionHandle, input: UserInput) -> Result<RunId>;
    fn cancel(&self, h: &AgentSessionHandle, run: RunId) -> Result<()>;
    fn answer(&self, h: &AgentSessionHandle, req: RequestId, d: ApprovalDecision) -> Result<()>;
    fn events(&self, h: &AgentSessionHandle) -> AgentEventStream;
    fn checkpoint(&self, h: &AgentSessionHandle, run: RunId) -> Result<CheckpointId>;
    fn restore_checkpoint(&self, h: &AgentSessionHandle, cp: CheckpointId) -> Result<()>;
    fn stop(&self, h: AgentSessionHandle) -> Result<()>;
}
```

- `AdapterCapabilities` flags: `streaming`, `resume`, `approvals`, `checkpoints`, `effort`, `images`, `todos`, `external_tools`. Declarations are honest, per the compiled-adapter registry rules: an adapter that cannot surface permission requests declares `approvals: false`, and hosts hide the affordance rather than fake it.
- One `AcpAdapter` covers every ACP-speaking agent, which is why it is its own row rather than a per-vendor adapter.
- The existing six compiled CLI adapters remain the `CliAdapter` fallback path; nothing in the current run pipeline is discarded.

## Security and trust boundary

- The library inherits the credential rules from `provider-integrations.md` unchanged: Managed Credentials bound to approved destinations, no credential material in events, logs, or run records, and no redirect-following that crosses origins with auth headers.
- Policy flows inward: the Host Application declares `AccessMode` and approval policy per session; the library enforces it but does not invent authorization. A host adding remote access authenticates and authorizes in its own transport layer.
- The library makes no network calls beyond what provider integrations already make. There is no telemetry, no home server, no update check inside the contract.
- Attachments are host-resolved local paths; the runtime never fetches URLs on a host's behalf.

## Reference mapping

| Reference | Borrow | Avoid |
| --- | --- | --- |
| pingdotgg/t3code | Normalized `ProviderRuntimeEvent`-style union; per-turn Checkpoints via hidden git refs; command receipts; replay-to-snapshot fallback bounds; restart reconciliation via provider resume cursors | Full event sourcing (decider/projector machinery); Electron-coupled packaging |
| codex `app-server` | stdio JSON-RPC as the embeddable host surface | Treating it as multi-protocol: it speaks Codex's own schema, not this contract |
| sst/opencode | `serve` HTTP surface as a provider integration point | Its auth storage model as a concurrency guarantee |
| Zed / ACP agents | One protocol adapter covering several vendors | Per-vendor ACP forks |
| `provider-integrations.md` | AuthBinding, Credential Reference, registry, and trust-boundary rules reused unchanged | Duplicating credential logic inside the runtime contract |

## Delivery sequence

1. P0 - contract and facade. `AgentCommand`/`AgentEvent`/`Receipt`/`SessionSnapshot` types, the in-process `AgentRuntime` facade over the existing run machinery, `CliAdapter` as the honest fallback, `integrations.list`, `models.list` merging Advertised/Entitled/Available, session.subscribe replay from the run store.
2. P1 - deep adapters and durability. `CodexAdapter` over app-server and `ClaudeAdapter` over stream-json with live approvals, Checkpoints, `quota.observed`, and the stdio JSON-RPC bridge (`aifuel runtime`).
3. P2 - coverage. `AcpAdapter`, `OpenCodeAdapter`, `checkpoint.restore`, `model.select` mid-session, image attachments.
4. P3 - thin hosts. Structured approval answers and elicitation, `externalTools` enforcement with provider-side readiness, session-owner scoped reconcile, and `RuntimeExecutionAdapter`: a shim implementing the legacy execution contract over `AgentRuntime` so `aifuel run` and `aifuel mcp execution` drive real runtime sessions without discarding owner routing, deadlines, or `RunResult` semantics.
