# Provider integrations

## Problem Statement

AI Fuel runs every Agent Run through a provider-owned CLI and assumes that CLI owns the credential. This covers the six compiled Catalog Providers but excludes common setups: users with billed API keys, local model servers such as Ollama and LM Studio, and providers reachable only through an AI Fuel-owned OAuth client.

Provider identity is today the `ProviderKey` enum, so one upstream cannot have two configured execution paths. A bare provider name cannot distinguish "subscription through the CLI" from "billed API key", and silently picking one is a billing decision AI Fuel must never make for the user. Provider Discovery assumes a provider-specific credential file, so env-var keys, managed credentials, and local servers are invisible to discovery and monitoring.

## Goals

- Run prompts and monitor quota through CLI harnesses and direct HTTP integrations under one application contract.
- Separate upstream provider identity from configured integrations so several integrations can share one upstream.
- Support API keys, AI Fuel-owned OAuth, and unauthenticated endpoints as explicit configuration.
- Let users define providers and integrations in config alongside the compiled adapters, while keeping compiled adapter behavior per ADR-0003.
- Keep Provider Discovery offline and side-effect-free.
- Keep Managed Credentials bound to approved destinations and concurrency-safe under refresh.

## Non-Goals

- Runtime-loaded provider plugins or a generic strategy framework. Adapters and wire protocols remain compiled Rust per ADR-0003.
- Automatic provider switching, quota-driven routing, or silently substituting billed API execution for subscription execution.
- Importing or sharing provider CLI credentials into the AI Fuel store. CLI-owned logins stay CLI-owned.
- A hosted credential service, remote sync, or an OAuth/OIDC server for third-party tools.
- New wire protocols, OAuth flows, or quota collectors beyond the ones demonstrated in each phase.

## Domain Terms

This spec uses the CONTEXT.md terms Catalog Provider, Provider Discovery, Discovered Provider, Provider Credential Source, Provider Account, Quota Pool, Agent Integration, Agent Run, and Execution Availability. It adds the following terms, intended for CONTEXT.md after implementation.

**Provider Integration**:
A configured binding of one provider identity to one execution configuration and one optional monitoring configuration. A Provider Integration is what a user runs and monitors; a single upstream provider can back several Provider Integrations.
_Avoid_: Provider mode, provider profile, provider alias

**Managed Credential**:
A credential stored and maintained by AI Fuel in its own Credential Store: an API key or an OAuth grant pair. Provider CLI logins and unrelated account logins are not Managed Credentials.
_Avoid_: Provider credential, native credential, shared login

**Credential Reference**:
An opaque identifier naming one Managed Credential in the Credential Store. Integrations bind credentials by reference so secrets never appear in config, run records, or reports.
_Avoid_: Token value, inline key, credential path

**Wire Api**:
The named HTTP request/response protocol an endpoint speaks: OpenAI chat completions, OpenAI responses, or Anthropic messages. A Wire Api is protocol evidence, not a provider identity.
_Avoid_: Provider type, endpoint format, API kind

## Integration Model

### Identity

- `ProviderId` is an opaque string identifying the upstream service. Built-in ids equal the pinned catalog ids exposed by `ProviderKey::as_str()` (`claude`, `codex`, `copilot`, `gemini`, `antigravity`, `devin`). User-defined providers receive their own ids. A `ProviderId` is never parsed for semantics.
- `IntegrationId` is an opaque string identifying one configured Provider Integration, such as `claude-cli`, `work-anthropic`, or `ollama-local`. `aifuel run` selects an `IntegrationId`. It is never parsed; there is no `provider:mode` splitting for routing.
- `CredentialRef` is an opaque string identifying one Managed Credential. Several Provider Integrations may share a `CredentialRef` where the credential legitimately covers them (for example an API integration and its monitoring binding). CLI integrations never hold a `CredentialRef`; the provider CLI owns its credential.
- `ProviderKey` conversion is restricted to the six compiled CLI adapters. New code works in the string ids; only the CLI adapter factory maps an integration back to a `ProviderKey`.

### Execution configuration

Execution is a closed enum, not a credential-kind flag:

```rust
enum ExecutionConfig {
    Cli { adapter: CliAdapterId },
    Http {
        endpoint: EndpointConfig,
        protocol: WireApi,
        auth: AuthBinding,
    },
}

enum AuthBinding {
    None,
    ApiKey { source: ApiKeySource, delivery: KeyDelivery },
    OAuth { credential: CredentialRef, profile: OAuthProfileId },
}

enum WireApi {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}
```

- `EndpointConfig` carries the base URL, optional extra headers, and request timeouts. `KeyDelivery` describes how an API key reaches the wire (`Authorization: Bearer`, `x-api-key`, or a documented provider placement), because header conventions differ across OpenAI-compatible services.
- Rationale for this shape: a local endpoint can still require auth, as with a remote Ollama or a LiteLLM proxy, so "local implies unauthenticated" is wrong. Endpoint is configuration; the auth binding is a separate explicit choice. This is not a free Cartesian product: `Cli` carries no endpoint or auth fields because the provider CLI owns credentials, and `Http` always carries an explicit `AuthBinding`, including an explicit `None`.
- `WireApi::OpenAiChat` initially serves Ollama, LM Studio, and OpenAI-compatible gateways. A native `OllamaChat` variant waits for a concrete requirement instead of speculative coverage.
- `CliAdapterId` names a compiled CLI adapter from the existing registry; `OAuthProfileId` names a compiled OAuth flow specification. Both are closed compile-time sets, so config can select but cannot invent behavior.

### Selection and defaults

- `aifuel run` selects an `IntegrationId`. Selection flags, profiles, and defaults follow the existing precedence rules in `unified-agent-management.md`.
- A bare `ProviderId` resolves only when it maps to exactly one configured Provider Integration. Otherwise the request fails with an actionable error naming the candidates, or the user sets an explicit default integration for that provider.
- AI Fuel never silently switches from subscription execution to billed API execution. Ambiguity is an error, not a guess.
- User config may add integrations for built-in providers and may define new providers. Built-in integrations ship as built-in config-equivalent definitions so the runtime model is uniform.

### Monitoring configuration

- Each Provider Integration carries an `Option<MonitoringConfig>` with its own optional credential binding. The inference Wire Api says nothing about quota APIs, so monitoring is a separate contract per integration, not derived from execution.
- A CLI integration keeps its existing collector behavior. An HTTP integration may attach a managed quota collector (for example OpenRouter `/key` or the Copilot usage endpoint) or report Unsupported.

## Credential Store Contract

### Storage format

- The Credential Store is `credentials.json` inside the AI Fuel config directory (`~/.config/aifuel/` by default). On Unix the file is created mode 0600 and repaired toward 0600 on write. Mode 0600 is not portable: on Windows the file uses a user-restricted ACL or an OS credential backend; keychain or Secret Service backends are a later option, not a P1 promise.
- The schema follows the opencode `auth.json` shape: a map from Credential Reference to a typed record.

```json
{
  "work-anthropic": { "type": "api", "key": "..." },
  "copilot-oauth": {
    "type": "oauth",
    "access": "...",
    "refresh": "...",
    "expires": 1735689600,
    "account_id": "..."
  }
}
```

- The store holds only Managed Credentials. It never contains provider CLI credentials, prompts, or run data.

### Transaction rules

All of the following are normative:

1. A stable sidecar lock file (`credentials.json.lock`) in the same directory serializes mutations. The lock lives beside the data file, never at its path, because atomic replacement swaps the inode and a lock on the data file would not survive a rename.
2. The mutation sequence is: acquire the lock, reread the store, recheck expiry, perform a bounded refresh if still needed, persist, release. Rereading after lock acquisition avoids a duplicate refresh when a concurrent process already renewed the grant.
3. Writes are atomic: write to a temporary sibling file, flush, then rename over the data file.
4. When a refresh response omits a replacement refresh token, the stored refresh token is preserved. A missing field is not a deletion.
5. A malformed store is never treated as empty and never overwritten. Parse failure stops credential operations and reports repair guidance.
6. After a refresh request has been sent, an ambiguous network failure must not blindly retry the possibly-consumed refresh token. Bounded retry applies only before the request is sent; afterward the outcome is verified through a subsequent read or a bounded single confirm, not a replayed grant.
7. There is an accepted crash window between provider-side grant rotation and local persist. If the stored grant is dead after this window, recovery is re-authentication, not replay.
8. Blocking lock and file operations run off async worker threads (blocking section or synchronous caller context), matching the existing CLI adapter thread-scope pattern.
9. A metadata-only read operation returns presence, credential type, and expiry state for discovery and `auth list`. It never returns token material.
10. The store carries an explicit schema version and rejects unknown versions instead of guessing.

### Environment precedence and auth commands

- Each integration declares which environment variables it accepts (for example `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`). Env-var precedence is explicit per integration in its config, not a global guess across provider names.
- `aifuel auth set-key <integration>` writes an API-key Managed Credential and binds it to that integration. `aifuel auth list` reports the effective credential source per integration - managed ref, named env var, CLI-owned, or none - and never prints values. `aifuel auth remove <credential>` deletes a Managed Credential.
- `aifuel auth remove` warns when removing a managed credential leaves an env-var credential active for the same integration, since the user may believe they signed out.

## Discovery Changes

- Provider Discovery stays offline and side-effect-free per CONTEXT.md. It returns evidence states, not readiness: configured, credential present, installed, and last validated where a prior validation timestamp was recorded.
- New source kinds join the existing file and directory markers:
  - `EnvVar`: presence of a declared, non-empty environment variable. The value is never read into reports.
  - `ManagedEntry`: presence of a Credential Reference in the store, through the metadata-only read only.
  - `ConfiguredEndpoint`: a config entry for the integration, or presence of a provider-owned marker directory such as `~/.ollama` or `~/.lmstudio`.
- Endpoint reachability, credential validity, and quota are checked during collect and validate. Discovery never opens a socket, refreshes a token, or reads credential content, and a failed endpoint stays discoverable like any other Discovered Provider.

## Execution Contract

- Add `AgentCapability::PromptCompletion`: a direct prompt completion through a Wire Api in which AI Fuel executes no provider-side tools. The existing registry test asserting every adapter declares each `AgentCapability::ALL` member forces honest declarations from CLI and wire adapters alike.
- Wire adapters honestly declare `WorkspaceWrite`, `ExternalMcpTools`, `Resume`, `OrdinaryInput`, and `PermissionApproval` as Unsupported until demonstrated. `Effort`, `AccountSelection`, `Streaming`, `StructuredOutput`, and `ModelCatalog` are declared per observed Wire Api support.
- `ReadOnly` is honestly enforceable in prompt-completion mode because AI Fuel executes no provider-side tools and the model cannot write. `ReadOnly` here means "no workspace writes"; it does not mean "cannot read files", since the request itself may carry file content and the endpoint operator sees the prompt.
- Normalized run events: text delta, usage, completion, cancellation, failure. A stream that ends (EOF) without a terminal completion event is a failure, not success; this matches the existing rule that exit zero on a structured protocol is not proof of a completed turn.
- Stream decoding handles fragmented UTF-8 at chunk boundaries, multiline SSE events, and provider-specific keepalive lines. Buffering is bounded with backpressure so an unbounded stream cannot grow memory without limit. An idle timeout applies between events, independent of the optional overall deadline. Partial output is preserved and reported.
- AI Fuel never auto-replays an inference request after an ambiguous disconnect. The request may have been consumed and billed; replay is a user decision. This mirrors the existing no-replay rule for start/resume delivery uncertainty.

## Monitoring Contract

- Monitoring produces typed observations, not prose. Each observation identifies the metric, the unit, the scope (Provider Account, organization, or key), the observation time, the reset time where known, and provenance.
- Observation states: `Unsupported` (no monitoring contract), `Unauthenticated` (credential missing or rejected), `Unavailable` (endpoint unreachable or changed), `Observed` (values returned). A missing limit is unknown; it is never reported as zero remaining quota.
- Integrations that observe the same Provider Account and Quota Pool are deduplicated only when matching account-scope evidence establishes the relationship, consistent with the CONTEXT.md Provider Account rule. A CLI integration and an API integration against one upstream account report one shared pool; unknown account identity is never deduplicated by guesswork.

## Registry and Migration

- The compiled adapter-factory registry per ADR-0003 and ADR-0004 is unchanged: CLI adapters, wire protocol engines, OAuth flow mechanics, and monitoring collectors are compiled Rust. No runtime plugins.
- A new owned runtime registry is built at startup from built-in definitions plus validated user config. Built-ins describe Provider Integrations backed by compiled adapters; config entries select compiled capabilities (`CliAdapterId`, `WireApi`, `OAuthProfileId`, monitoring collector ids) but cannot inject behavior.
- Capability indexes for discovery, execution, and monitoring are derived views over the runtime registry. The registry owns its strings; no leaked allocations fake `'static` lifetimes.
- Policies:
  - A duplicate `IntegrationId` is a config error. The offending entry is rejected and reported; nothing is silently renamed.
  - Built-in integration ids are reserved; user config cannot redefine them, only configure documented fields.
  - Ordering is deterministic: built-ins in pinned catalog order, then config entries in file order.
  - Aliases map legacy keys and prior config shapes onto current ids at decode time only; aliases are never a second runtime identity.
  - An unknown adapter, protocol, or profile id is an explicit error, never a fallback.
- Stored runs, selection config, and MCP schemas decode through explicit schema versions, as `STATUS_SCHEMA_VERSION` and `RUN_MANAGEMENT_SCHEMA_VERSION` do today. Unknown versions fail with a version error rather than best-effort parsing.

## Security and Trust Boundary

- Credentials are bound to approved destinations. A Managed Credential records the destination it was created for; a config endpoint override must not inherit a built-in credential reference, because that would send keys to a different host. Overriding an endpoint requires binding a credential to that endpoint explicitly. Zed's `api_compatible.rs` handles this same endpoint-change problem and is the reference for the check.
- Redirects are validated. Managed auth material is not forwarded across origins; a redirect that changes origin fails the request rather than silently replaying headers.
- Custom endpoint headers cannot override managed auth. The managed binding is applied last so config cannot smuggle a different `Authorization` value.
- Credential material never enters logs, diagnostics, run records, the status report, or MCP output. `RunRequest`'s `Debug` already redacts prompts; the same rule extends to credential fields.
- Store protection is filesystem ACL-based in P1. Document this honestly; an OS credential backend is a hardening option, not a claim.

## OAuth Integrations

- Each OAuth integration ships only after demonstrated client access with AI Fuel's own registered OAuth client: login, inference, and monitoring must all be observed with that client. AI Fuel acts as an RFC 8252 public client with no confidential secret.
- Reusing a provider CLI's stored tokens is forbidden. Rotating refresh grants would invalidate the CLI's session, so AI Fuel registers its own client per integration.
- `OAuthFlowSpec` is declarative data (authorization, token, and device endpoints; client id; scopes). Flow mechanics live in aifuel-providers, not aifuel-core, keeping aifuel-core free of HTTP and OAuth machinery.
- Order: GitHub Copilot device flow first. Note it is a two-token flow - the GitHub device grant exchanges for a separate expiring Copilot token - so the integration manages both lifetimes. Then browser plus PKCE on a loopback listener; `tiny_http` and `sha2` are already workspace dependencies.
- Anthropic OAuth is experimental until demonstrated; Anthropic's public guidance directs third-party tools to API keys.

## Reference Mapping

| Reference | Borrow | Avoid |
| --- | --- | --- |
| codex-rs (`model-provider-info/src/lib.rs`, `login/src/auth/manager.rs`) | `ModelProviderInfo` shape (name, base_url, env_key, wire_api, requires_auth) and the `CodexAuth` union of API key versus OAuth | Treating it as multi-protocol: its `WireApi` supports Responses only |
| sst/opencode | `auth.json` record schema and the plugin `authorize()`/`callback()` shape | Its storage as a concurrency guarantee; our lock and transaction rules are separate |
| zed-industries/zed | Responsibility split and `crates/language_models/src/provider/api_compatible.rs` endpoint-change credential handling | Its `authenticate()` interface, which is GPUI-coupled |
| block/goose | Declarative providers selecting a closed engine set (`goose-providers/src/declarative.rs`; `ProviderEngine` is a closed enum) and an explicit `AuthMethod::NoAuth` | A plugin or engine model beyond the compiled set |
| charmbracelet/catwalk + crush | Catalog as pure data | crush's credential struct, which does not enforce one active credential per binding; ours does |

## Delivery Sequence

1. P0 - model and store. Identity types (`ProviderId`, `IntegrationId`, `CredentialRef`), `ExecutionConfig`/`AuthBinding`, the Credential Store with its transaction rules, the monitoring result model, `AgentCapability::PromptCompletion`, discovery evidence states, and the runtime registry with migration and versioned decoding.
2. P1 - one complete OpenAI-compatible path. `WireApi::OpenAiChat` only, `aifuel auth` (`set-key`, `list`, `remove`), minimal custom-endpoint config honoring the trust boundary, exactly one managed quota collector (OpenRouter `/key` or the Copilot usage endpoint), and Ollama through the OpenAI-compatible API.
3. P2 - one OAuth integration with demonstrated client access: the Copilot device flow end to end.
4. P3+ - expand the matrix: `AnthropicMessages`, `OpenAiResponses`, the PKCE loopback flow, and declarative provider hardening, only after the P1 path and the test matrix hold.

## Testing Decisions

- The primary seam is the application contract, matching the unified-agent-management testing boundary: drive CLI and MCP surfaces with controlled endpoints and controlled executables. Add narrow tests only where wire-format edge cases cannot be covered there.
- Required before matrix expansion (the P3 gate):
  - Concurrent refresh: two contenders serialize on the sidecar lock; the second rereads and skips the refresh.
  - Refresh versus logout: a removal during refresh does not resurrect the credential.
  - Interrupted persistence: a crash mid-write leaves the old or the new store, never a truncated one; malformed content is never overwritten.
  - Offline discovery: discovery output is identical with no network.
  - Legacy config loading: versioned decode of stored runs and selection config; unknown versions error.
  - Truncated SSE streams: EOF without a terminal event is failure; partial output is preserved; nothing is replayed.
- Trust-boundary cases: an endpoint override cannot reuse a built-in credential reference; a cross-origin redirect drops managed auth and fails; a custom header cannot override managed auth; `auth list` reports env precedence accurately; two integrations for one provider make bare-id selection an error.
- Refresh-transaction cases: missing replacement refresh token preserves the stored one; ambiguous post-send failure does not retry the consumed grant; the documented crash window resolves to re-authentication.

## Open Questions

- The OS credential backend choice for Windows, macOS Keychain, and Secret Service, and whether the file store remains the fallback when a backend is absent.
- Whether user-defined `ProviderId`s get a reserved prefix to prevent future collisions with built-in catalog ids.
- How Advertised Model evidence is labelled for local servers exposing `/models`, given that listing does not establish entitlement or Execution Availability.
- Whether a monitoring binding may share an OAuth access token with execution or requires separate scopes.
- Retention and expiry policy for stale Managed Credentials after the upstream account signs out or the grant is revoked.
