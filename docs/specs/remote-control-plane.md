# Remote control plane

## Problem Statement

AI Fuel is local-first: a single binary owns Provider Discovery, Provider Integrations, Agent Runs, and quota monitoring on one machine. A remote-control product needs a way for a user's other devices, such as a phone, to drive those Agent Runs when the laptop is reachable only through a hosted relay.

The obvious design - relay all traffic through the server in plaintext - is not acceptable for a tool that moves prompts, source code, diffs, and terminal output. Equally, an adapter contract reduced to `send()` and `stream()` cannot express what interactive coding agents actually emit: permission requests, questions, tool calls, diffs, and compaction events. T3 Code solves the first problem by having no server at all; this product needs the same trust properties with a relay in the middle.

This spec defines the remote control plane: device identity and pairing, end-to-end encryption, the typed command/event protocol, the agent adapter contract, and the model-selection model.

## Goals

- Let any paired device of one user observe and command Agent Runs on another of that user's devices.
- Keep message content unreadable by the relay: commands and events are end-to-end encrypted between devices; the relay routes opaque ciphertext.
- Expose provider agent behavior through one typed event contract, so no client renders raw text deltas.
- Route remote permission approvals and questions back to the running agent, with audit.
- Let a reconnecting client replay a session without losing in-flight Agent Runs.
- Select execution by IntegrationId plus model and effort, keeping "claude via subscription" and "claude via API key" distinct choices.
- Surface quota observations so a client can answer "which integration has headroom".

## Non-Goals

- Multi-user or team sessions. All paired devices belong to one user; shared workspaces are out of scope.
- A full remote terminal/PTY. Mobile surfaces agent control, approvals, and status, not a shell.
- Full event sourcing on the desktop. A durable per-session event log supplies replay; there is no command decider or projection framework.
- Hosted inference or provider token resale. Execution stays on the user's devices with user-owned credentials, per the BYO-subscription model.
- Storing message content on the server. Neon holds metadata only.
- Queueing commands for offline devices. A command to an offline device is rejected, not stored.

## Domain Terms

This spec uses the CONTEXT.md terms Provider Integration, Integration Identity, Managed Credential, Credential Reference, Authentication Binding, Advertised Model, Account Entitlement, Execution Availability, Quota Pool, Agent Integration, Agent Session, Agent Run, and Monitoring Collection Contract. It adds the following terms, intended for CONTEXT.md after implementation.

**Device**:
A registered client installation belonging to one user: a desktop running the AI Fuel agent host, or a mobile/web client. A Device owns a long-term keypair and is the unit of pairing, revocation, and end-to-end key agreement.
_Avoid_: Client app, installation, node

**Relay**:
The hosted WebSocket control plane. It authenticates connections, tracks online Devices, routes envelopes between a user's Devices, and emits push triggers. It never decrypts agent payloads and never stores message content.
_Avoid_: Server-side session state, message store, backend of record

**Envelope**:
The wire unit crossing the Relay: a cleartext routing header plus an encrypted body. Header fields exist only where the Relay needs them to route, rate-limit, or trigger push.
_Avoid_: Message, packet, frame

**Session Event Log**:
The durable, per-session sequence of typed events persisted by the agent host. It assigns each event a monotonic `seq`, powers replay on reconnect, and extends the existing per-user SQLite run store.
_Avoid_: Event store in the event-sourcing sense, chat history table

**Approval Request**:
A typed, blocking question from a running agent to the user: tool permission, plan approval, free-form question, or MCP elicitation. It carries explicit options and can require re-authentication on the answering Device.
_Avoid_: Prompt, dialog, notification

**Checkpoint**:
A hidden git ref recorded at the end of an Agent Run that mutates the workspace, enabling diff, restore, and PR flows per run.
_Avoid_: Snapshot, backup, commit

**Pairing**:
The ceremony that establishes trust between two of a user's Devices: same-account registration plus explicit approval on the already-trusted Device, producing a shared encryption key.
_Avoid_: Login, linking, sync

## Architecture

```text
Mobile / web client                Relay (hosted)                 Agent host (desktop)
        |                               |                               |
        |--- Clerk JWT auth ----------->|                               |
        |                               |<---- Clerk JWT auth ----------|
        |                               |                               |
        |=== Envelope (E2E body) =======|=============================> |--- AgentAdapter
        |                               |                               |     |-- ClaudeAdapter
        |<== Envelope (E2E body) =======|=============================  |     |-- CodexAdapter
        |                               |                               |     |-- OpenCodeAdapter
        |                               |                               |     |-- AcpAdapter
        |                               |--- push trigger (opaque) --> APNs/FCM
        v                               v                               v
   renders typed events          routes by (user, device)         runs provider protocols,
                                                               persists Session Event Log
```

- The Relay runs Hono on a single VPS per the product architecture note. Its responsibilities are authentication, device registry, routing, presence, rate limits, and opaque push triggers. It is not a database of record for content.
- The agent host is the existing AI Fuel runtime extended with an outbound persistent WebSocket to the Relay. The Relay never initiates inbound connections to the host, so NAT traversal needs no extra machinery.
- The wire contract is JSON and language-neutral. TypeScript sketches in this spec are illustrative; the Rust host serializes the same shapes. A schema package or shared definitions keep clients and host aligned.
- Execution Availability, Entitlements, and quota come from the host's existing registry and collectors; the Relay and clients never contact provider APIs.

## Protocol layers

### Envelope

```ts
interface Envelope {
  v: 1;
  id: string;
  from: DeviceId;
  to: DeviceId;
  ts: string;                 // ISO
  kind:
    | "agent.command"         // body = AgentCommand
    | "agent.event"           // body = AgentEvent, may be batched
    | "agent.receipt"         // body = Receipt
    | "notify"                // opaque push trigger
    | "device.ping" | "device.pong";
  enc: "x25519-chacha20poly1305";
  body: string;               // base64 ciphertext for agent.* kinds; JSON for control kinds
}
```

- `v` is the protocol version; unknown versions fail rather than best-effort parse, matching the repo's versioned-decoding rule.
- The Relay routes on `(authenticated user, to)` and enforces per-plan rate limits on `kind`. It must not require body readability to operate.
- `notify` carries only `{ category, sessionId }` where `category` is one of `approval_needed | run_finished | run_failed`. Category exists so push works under E2E; content never does.
- Heartbeat uses `device.ping`/`pong`; presence is derived from connection state, not a table.

### Commands and receipts

Client to host, one `Receipt` each, `commandId` makes retries idempotent:

```ts
type AgentCommand =
  | { type: "session.create"; commandId; cwd; selection: ModelSelection; access: AccessMode }
  | { type: "session.attach"; commandId; sessionId; lastSeenSeq: Seq }
  | { type: "session.list";   commandId }
  | { type: "session.close";  commandId; sessionId }
  | { type: "run.start";      commandId; sessionId; input: UserInput }
  | { type: "run.cancel";     commandId; sessionId; runId }
  | { type: "approval.answer";commandId; sessionId; requestId; decision: ApprovalDecision }
  | { type: "model.select";   commandId; sessionId; selection: ModelSelection }
  | { type: "checkpoint.restore"; commandId; sessionId; checkpointId }
  | { type: "integrations.list"; commandId }
  | { type: "models.list";    commandId; integrationId };

type AccessMode = "read_only" | "workspace_write" | "full";   // aifuel --access
```

```ts
type Receipt =
  | { commandId; ok: true;  seq: Seq; sessionId?: SessionId; snapshot?: SessionSnapshot }
  | { commandId; ok: false; code: string; message: string };
```

Receipt `code` values are a closed set: `device_offline`, `unauthorized`, `entitlement_required`, `unknown_session`, `already_resolved`, `invalid_selection`, `provider_error`, `unsupported`. Unknown codes render generically; the set grows only by protocol version.

### Events

Every host emission is one typed `AgentEvent`; `seq` is assigned by the Session Event Log before anything touches the network:

```ts
type AgentEvent = { sessionId; seq: Seq; ts: string } & (
  | { type: "session.created"; integrationId; cwd }
  | { type: "session.status"; status: SessionStatus }
  | { type: "session.closed"; reason?: string }
  | { type: "run.started";  runId; selection: ModelSelection }
  | { type: "run.completed";runId; outcome: "success" | "failed" | "cancelled"; usage?: Usage }
  | { type: "message.delta";     runId; stream: "assistant" | "thinking"; text }
  | { type: "message.completed"; runId; stream: "assistant" | "thinking" }
  | { type: "tool.started";   runId; tool; summary }
  | { type: "tool.completed"; runId; tool; ok; diff?: FileDiff; output?: string }
  | { type: "todos.updated";  runId; items: TodoItem[] }
  | { type: "approval.requested"; runId; requestId; request: ApprovalRequest }
  | { type: "approval.resolved";  requestId; decision; answeredBy: DeviceId }
  | { type: "checkpoint.created";  runId; checkpointId; diffstat }
  | { type: "checkpoint.restored"; checkpointId }
  | { type: "quota.observed"; integrationId; quota: QuotaSummary }
  | { type: "error"; runId?; code; message; retryable: boolean }
);

type SessionStatus =
  | "idle" | "working" | "waiting_approval" | "compacting" | "interrupted" | "closed";
```

- A stream that ends without a terminal `run.completed`/`run.failed` is a failure, matching the existing rule that exit zero is not proof of a completed turn.
- `quota.observed` is emitted post-run when the integration's Monitoring Collection Contract reports. This is the surface that makes "route to the integration with headroom" possible; no competitor in this class exposes it.

### Attach and replay

- `session.attach { lastSeenSeq }` returns a `SessionSnapshot` plus all events with `seq > lastSeenSeq`, in order.
- `SessionSnapshot` is the materialized read model: status, `ModelSelection`, cwd, pending Approval Requests, Checkpoints, `headSeq`, and a bounded `tail` of transcript items. `tail` is capped; older content is paginated by `seq`, because large WS payloads are a known performance hazard.
- A command to a device with no open Relay connection fails with `device_offline` immediately. Commands are never queued.
- Any paired Device may send commands; the event log serializes order. When two Devices answer one Approval Request, the first wins and the second receives `already_resolved`. A `approval.resolved` event naming `answeredBy` keeps every client's UI honest.

## Device identity and encryption

- Each Device generates a long-term X25519 keypair at first launch. Registration publishes the public key under Clerk-authenticated identity into `devices` (`id`, `user_id`, `name`, `platform`, `app_version`, `public_key`, `last_seen_at`, `revoked_at`).
- Pairing two Devices requires the same user account plus explicit approval on an already-trusted Device. Pairing derives a pairwise session key (X25519 ECDH, ChaCha20-Poly1305 AEAD). Account compromise alone must not silently grant control; the approval step is the guard.
- `agent.*` envelope bodies are ciphertext. Control-plane messages (`device.register`, presence, pairing, `notify`) are cleartext because the Relay legitimately needs them.
- Revocation sets `devices.revoked_at`; the Relay refuses routing to revoked Devices and the host drops their sessions.
- Defense in depth: the host enforces its own policy on what remote control may do - honoring Authentication Binding destinations and the credential trust-boundary rules from `provider-integrations.md` - rather than trusting Relay authorization alone.

## Approvals and push

- Approval Request kinds: `tool_permission`, `plan_approval`, `question`, `mcp_elicitation`. Each carries `title`, `detail`, explicit `options`, and `requiresConfirm`. Decisions select an option or supply free text for `question`.
- `requiresConfirm` demands fresh biometric/PIN on the answering Device. The host cannot verify biometrics; it trusts the paired Device's claim, which is why pairing approval and revocation exist.
- Every `approval.answer` writes an audit record on the host (device, request, decision, timestamp) and is forwarded to the Relay audit log for the account.
- When an Approval Request lands while no client is attached, the host emits `notify { category: "approval_needed" }`. Push carries no content; opening the app attaches and replays the request.

## Checkpoints

- On `run.completed` for `workspace_write` or `full` runs, the host records a hidden git ref and emits `checkpoint.created` with a diffstat. `read_only` runs produce no Checkpoint since they produce no diff.
- `checkpoint.restore` resets the workspace to that ref. Restore is itself an auditable command because it discards working state.
- This borrows T3 Code's turn-checkpoint model. It is what makes one-button diff review and PR flows possible later.

## Model selection

```ts
interface ModelSelection { integrationId: IntegrationId; model: string; effort?: Effort }
type Effort = "low" | "medium" | "high" | "max";

interface ModelDescriptor {
  provider: string;
  model: string;
  label: string;
  efforts: Effort[];
  advertised: boolean;                       // Advertised Model
  entitled: boolean | "unknown";             // Account Entitlement
  availability: ExecutionAvailability;       // ready | needs_auth | unsupported | unknown
  quota?: QuotaSummary;                      // Quota Pool headroom when monitored
}
```

- Selection is by Integration Identity, never a `provider:mode` compound parsed for routing - the same rule the provider-integrations spec applies to `aifuel run`.
- `models.list` merges the Advertised Model catalog with Account Entitlement and Execution Availability for that integration's account. `session.create` and `model.select` call `resolve` first; a selection that is not `ready` fails with `invalid_selection`, never silently falls back to another model or another credential.
- Mid-session `model.select` applies to the next Agent Run, matching how provider harnesses treat model switching.
- The host never substitutes billed API execution for subscription execution, extending the existing no-silent-switch rule to remote clients.

## Agent adapter contract

One adapter per provider protocol. Complexity lives at the adapter boundary; the host and clients stay provider-agnostic. Adapters wrap provider protocols, not PTY output:

| Adapter | Wraps |
| --- | --- |
| `ClaudeAdapter` | `@anthropic-ai/claude-agent-sdk` query sessions |
| `CodexAdapter` | `codex app-server` (JSON-RPC over stdio) |
| `OpenCodeAdapter` | `@opencode-ai/sdk` |
| `AcpAdapter` | Agent Client Protocol (covers Cursor and other ACP agents) |
| `CliAdapter` | Fallback for providers with only a CLI |

```ts
interface AgentAdapter {
  readonly provider: string;
  capabilities(): AdapterCapabilities;             // streaming, resume, remoteApprovals,
                                                   // checkpoints, effort, images, todos
  resolve(selection: ModelSelection): Promise<ModelDescriptor>;
  listModels(integration: IntegrationDescriptor): Promise<ModelDescriptor[]>;
  start(integration: IntegrationDescriptor, opts: StartOptions): Promise<AgentSessionHandle>;
  stop(handle): Promise<void>;
  send(handle, input: UserInput): Promise<RunId>;
  cancel(handle, runId): Promise<void>;
  answer(handle, requestId, decision: ApprovalDecision): Promise<void>;
  events(handle): AsyncIterable<AgentEvent>;       // typed stream, seq assigned by host log
  checkpoint(handle, runId): Promise<CheckpointId>;
  restoreCheckpoint(handle, checkpointId): Promise<void>;
}
```

- Where the host is Rust, this contract is the port boundary between the runtime and provider adapters; capability declarations remain honest per the compiled-adapter registry rules (an adapter that cannot answer approvals remotely declares `remoteApprovals: false`, and the UI hides the affordance rather than faking it).
- One ACP adapter covers every ACP-speaking agent, which is why it is its own row rather than a per-vendor adapter.

## Relay data model

Neon stores metadata only:

```text
users            (internal identity; auth_provider + auth_subject, per architecture note)
devices          (registry incl. public_key, revoked_at)
sessions         (id, user_id, device_id, created_at, status - never content)
audit_logs       (approvals, pairing, revocation, checkpoint.restore)
subscriptions    (Paddle webhook writes; entitlements gate remote_access)
```

Message content and event bodies never reach Neon. Session rows exist so clients can list sessions of an online device without a round trip; the authoritative log stays in the host's SQLite store.

## Security and trust boundary

- The Relay authenticates via Clerk JWT for both device classes. Entitlement checks (`remote_access`) run server-side; client claims are never trusted.
- The Relay is a ciphertext pipe for `agent.*` kinds. A compromised Relay can drop or delay traffic but cannot read or forge commands; forged ciphertext fails AEAD.
- Remote approvals are the highest-risk remote action. Pairing approval, `requiresConfirm`, device revocation, and audit logging all exist to bound it.
- The host applies the credential destination-binding rules from `provider-integrations.md` unchanged: remote commands cannot redirect Managed Credentials to unapproved endpoints.
- Push triggers reveal only a category and session id to Apple/Google infrastructure.

## Reference mapping

| Reference | Borrow | Avoid |
| --- | --- | --- |
| pingdotgg/t3code | Adapter boundary with normalized `ProviderRuntimeEvent`; turn Checkpoints via hidden git refs; command receipts; wrapping provider protocols (agent SDK, app-server, ACP) | Full event sourcing (decider/projector machinery); Electron desktop |
| happy (Claude Code mobile client) | E2E encryption between mobile and host over a relay; QR pairing ceremony | Its scope (single provider) |
| sst/opencode | SDK-level integration over CLI scraping | - |
| Zed / Gemini CLI (ACP) | One protocol covering several agents | Per-vendor ACP forks |

## Delivery sequence

1. P0 - loop without encryption. Envelope and control-plane kinds, device registration and presence, `session.create/attach/list`, `run.start/cancel`, typed events, `approval.answer`, snapshot plus seq replay, `device_offline` rejection. Dogfood only; no public traffic while bodies are cleartext.
2. P1 - E2E encryption and pairing (required before any external user), push triggers, Checkpoints, `quota.observed`, `model.select`.
3. P2 - mobile client on the shared protocol, entitlement gating, R2-backed attachments, session archive/export.

## Open questions

- `tail` transcript size in `SessionSnapshot`: cap and pagination strategy need one measurement pass against real sessions before fixing constants.
- Whether Checkpoint refs should be pushed or kept local-only; pushing enables cross-device diff review but leaks repository shape to no one - the refs stay on the host either way, so this is about whether restore can run from a device that was offline at run time.
- Multi-desktop arbitration for `session.create` targeting "the user's workspace" when two desktops are online: addressed by explicit `to: DeviceId` routing today; any convenience routing is a later decision.
