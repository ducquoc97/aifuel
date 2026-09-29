# Provider integration references: BYOK, OAuth, and local endpoints

Date: 2026-09-28

Upstream sources below were inspected on each repository's default branch on 2026-09-28 (codex `main`, opencode `dev`, zed `main`, goose `main`, catwalk `main`, crush `main`). No commits are pinned yet; see the follow-up note at the end.

This digest supports [ADR-0005](../adr/0005-provider-integrations-beyond-cli-harness.md): AI Fuel is extending beyond provider-CLI harnesses to BYOK API keys, AI Fuel-owned OAuth, and local endpoint providers such as Ollama and LM Studio. It records what each reference project actually does for provider and auth architecture, what AI Fuel borrows, and what it explicitly avoids. Vocabulary follows [`CONTEXT.md`](../../CONTEXT.md): a configured endpoint is not an Account Entitlement, and an Advertised Model is not Execution Availability.

## Executive result

All five projects converge on the same split: a provider descriptor as plain data, a credential as a typed union or tagged record, and a resolver that binds them at request time. None of them solve everything AI Fuel needs:

- Only Zed binds a stored credential to its endpoint URL; the others let a base-URL override silently inherit a built-in credential.
- None of them give a cross-process refresh guarantee for credential files. codex-rs keeps auth in one process; opencode rewrites a JSON file.
- None of them treat quota monitoring as a separate contract. opencode and crush have no quota monitoring at all.

AI Fuel's ADR-0005 shape (endpoint configuration plus a separate `AuthBinding`, credentials bound to approved destinations, monitoring as an optional per-integration contract) is stricter than every reference. That strictness is deliberate, and the caveats below explain why each upstream shortcut was rejected.

## openai/codex (codex-rs, Rust)

### Mechanism

Provider descriptor and OSS constructors live in [`codex-rs/model-provider-info/src/lib.rs`](https://github.com/openai/codex/blob/main/codex-rs/model-provider-info/src/lib.rs):

- `ModelProviderInfo` is a serde data struct: `name`, `base_url`, `model_catalog_url`, `env_key`, `env_key_instructions`, `wire_api`, `http_headers` / `env_http_headers` (static and env-interpolated), `request_max_retries`, `stream_max_retries`, `stream_idle_timeout_ms`, and `requires_openai_auth`.
- `built_in_model_providers()` returns a compiled map. User-defined entries in `~/.codex/config.toml` under the `model_providers` table are merged over the built-ins at runtime.
- `create_oss_provider(default_provider_port, wire_api)` builds local-endpoint descriptors: `DEFAULT_OLLAMA_PORT = 11434`, `DEFAULT_LMSTUDIO_PORT = 1234`, with `CODEX_OSS_PORT` / `CODEX_OSS_BASE_URL` env overrides.
- `WireApi` is a single-variant enum (`Responses`). Deserializing `"chat"` returns a hard error (`CHAT_WIRE_API_REMOVED_ERROR`, pointing at codex discussion 7782); the legacy `ollama-chat` provider id is removed the same way.

Credential union and ownership live in [`codex-rs/login/src/auth/manager.rs`](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/manager.rs):

- `CodexAuth` is an enum: `ApiKey`, `Chatgpt` / `ChatgptAuthTokens` (OAuth id/access/refresh token data), `PersonalAccessToken`, `AgentIdentity`, `BedrockApiKey`, `BedrockAccessKeys`.
- `AuthManager` is the single source of truth: it loads `auth.json` from `codex_home` once and owns refresh in-process.
- ChatGPT login is browser + PKCE loopback (`codex-rs/login/src/server.rs`, `pkce.rs`); `device_code_auth.rs` also exists.

Resolution lives in [`codex-rs/model-provider/src/auth.rs`](https://github.com/openai/codex/blob/main/codex-rs/model-provider/src/auth.rs): `resolve_provider_auth(auth, provider)` maps a `CodexAuth` x `ModelProviderInfo` pair to a `BearerAuthProvider`, an env-var API key, or an `UnauthenticatedAuthProvider` that adds no headers. Providers whose key source is absent resolve to unauthenticated rather than panicking.

### What AI Fuel borrows

- The `ModelProviderInfo` shape maps almost one-to-one onto `ExecutionConfig::Http { endpoint, protocol, auth }`: endpoint, wire protocol id, env-key name plus setup instructions, static and env-interpolated extra headers, and retry/timeout knobs all belong in the descriptor, not the credential.
- `create_oss_provider` is the reference for Ollama/LM Studio built-ins: a port default plus env override, resolved to an unauthenticated binding.
- `resolve_provider_auth` is the model for request-time auth resolution: a pure function from (stored credential, integration config) to a transport auth strategy, including an explicit "no auth" result.

### What AI Fuel avoids / caveats

- codex-rs is not a multi-protocol engine. Its `WireApi` now rejects Chat Completions outright, so it is a reference for descriptor shape and auth resolution only, not for protocol coverage. AI Fuel's `WireApi` needs at least OpenAI Chat Completions for the local-endpoint and BYOK cases.
- Credential choice changes the default endpoint: API-key mode targets `api.openai.com`, ChatGPT OAuth targets `chatgpt.com/backend-api/codex` (`CHATGPT_CODEX_BASE_URL`). This is real evidence that endpoint and credential are coupled upstream, which is why ADR-0005 binds credentials to approved destinations instead of letting an endpoint override inherit them.
- `AuthManager` is a single-process cache over `auth.json`. It gives no protection against two processes refreshing at once; AI Fuel's CredentialStore transaction rules (sidecar lock, reread-recheck-refresh under lock) exist precisely because this reference does not cover the multi-process CLI case.

## sst/opencode (TypeScript)

### Mechanism

Credential storage is [`packages/opencode/src/auth/index.ts`](https://github.com/sst/opencode/blob/dev/packages/opencode/src/auth/index.ts):

- `Auth.Info` is a discriminated union on `type`: `Oauth { refresh, access, expires, accountId?, enterpriseUrl? }`, `Api { key, metadata? }`, `WellKnown { key, token }`.
- Storage is one file, `auth.json` under `Global.Path.data` (`~/.local/share/opencode/auth.json`), written with mode `0o600`. `OPENCODE_AUTH_CONTENT` overrides the file entirely for tests/CI.
- `set()` is a read-all, merge-in-memory, `writeJson` sequence. There is no lock and no transaction.

Plugin-declared auth lives in [`packages/opencode/src/provider/auth.ts`](https://github.com/sst/opencode/blob/dev/packages/opencode/src/provider/auth.ts) and `packages/opencode/src/plugin/index.ts`:

- A plugin declares an `auth` field: `{ provider, methods: [{ type: "oauth" | "api", label, prompts? }], loader? }`. Built-in plugins cover Codex, Copilot, GitLab, Azure, xAI, and others.
- `ProviderAuth.Service` aggregates plugin hooks. `authorize()` runs the method, parks the pending result, and returns `{ url, method, instructions }` to the UI. `callback()` completes the exchange (code or device-poll) and stores either an `api` record (`key`) or an `oauth` record (`access`, `refresh`, `expires`, plus extras like `accountId`).

Provider resolution is [`packages/opencode/src/provider/provider.ts`](https://github.com/sst/opencode/blob/dev/packages/opencode/src/provider/provider.ts): it merges the models.dev catalog (`ModelsDev` database), plugin auth loaders, built-in custom loaders (env vars, stored auth, config), and `opencode.json` provider entries (marked `source: "config"`). Each model carries `api.npm`, the npm package name that selects the SDK and therefore the wire format; `BUNDLED_PROVIDERS` maps `@ai-sdk/*` packages, and anything else is installed at runtime via `Npm.add`.

### What AI Fuel borrows

- The discriminated union credential record (`type: "api" | "oauth" | ...`) is the shape ADR-0005's `AuthBinding` takes, with the OAuth extras (`accountId`, `enterpriseUrl`) as evidence that provider-scoped account metadata belongs inside the credential variant.
- The authorize/callback split is the right lifecycle for interactive OAuth: produce `{ url, instructions }`, park pending state keyed by provider, then complete the exchange in a separate step. This matches a CLI that cannot hold a UI open across the flow.
- models.dev is proof that an external catalog can feed provider descriptors; opencode keeps catalog, credentials, and user config in three separate sources and merges them deterministically.

### What AI Fuel avoids / caveats

- Do not copy the storage semantics. `auth.json` writes are a plain read-modify-write with no lock; two concurrent refreshes can lose a token. It is a convention, not a concurrency guarantee. AI Fuel's CredentialStore transaction rules exist because this file does not provide them.
- The union has three variants (`wellknown` exists beyond `api`/`oauth`) and the `Api` variant carries free-form `metadata`. A typed union does not prevent schema sprawl; AI Fuel should keep variants minimal and validated.
- Runtime `npm` install of wire-format packages is a supply-chain and reproducibility surface AI Fuel must not adopt. Wire protocols are compiled in, per ADR-0003.

## zed-industries/zed (Rust)

### Mechanism

The provider contract is [`crates/language_model/src/language_model.rs`](https://github.com/zed-industries/zed/blob/main/crates/language_model/src/language_model.rs): `trait LanguageModelProvider` with `id()`, `name()`, `icon()`, `provided_models()`, `is_authenticated()`, `authenticate() -> Task<Result<(), AuthenticateError>>`, `set_api_key()`, and `settings_view()`. [`crates/language_model/src/registry.rs`](https://github.com/zed-industries/zed/blob/main/crates/language_model/src/registry.rs) holds `LanguageModelRegistry::register_provider` and emits `Event::ProviderStateChanged` so the UI reacts to auth and model-list changes.

API-key state is [`crates/language_model/src/api_key.rs`](https://github.com/zed-industries/zed/blob/main/crates/language_model/src/api_key.rs): `ApiKeyState` resolves a key from the provider's env var first, then the OS keychain via `CredentialsProvider`. Keys are stored in the keychain associated with the provider URL (`write_credentials(&url, "Bearer", key)`), and `load_if_needed` / `handle_url_change` re-resolve when the URL differs.

The endpoint-override boundary is [`crates/language_models/src/provider/api_compatible.rs`](https://github.com/zed-industries/zed/blob/main/crates/language_models/src/provider/api_compatible.rs): when `settings.api_url()` changes, it calls `api_key_state.handle_url_change(api_url)` so a stored key is re-resolved against the new endpoint rather than silently reused. Copilot is OAuth device flow through the copilot crate (`request::PromptUserDeviceFlow`, completed via the `github.copilot.finishDeviceFlow` command).

### What AI Fuel borrows

- The responsibility split: `is_authenticated` (credential presence), `authenticate` (interactive repair), and `provided_models` (catalog) are separate operations. AI Fuel keeps the same split across Provider Discovery, credential management, and model discovery.
- `ApiKeyState`'s precedence (env var over OS keychain over settings) is the precedence AI Fuel adopts for `ApiKey { source }`, minus the keychain for now.
- `handle_url_change` is the only upstream implementation of credential-to-endpoint binding found in this review. ADR-0005's rule that a configured endpoint override never inherits a built-in credential is the same idea, applied at the binding layer instead of at credential lookup.

### What AI Fuel avoids / caveats

- Do not copy the interface signature. `authenticate()` takes GPUI's `&mut App` context; the useful part is the responsibility split, not GUI coupling. AI Fuel's equivalents are plain async operations on the facade.
- Zed assumes one credential per provider URL inside a GUI session. AI Fuel integrations are finer-grained (two integrations can share one upstream provider with different endpoints or credentials), so binding belongs on the Integration, not on a global provider slot.

## block/goose (Rust)

### Mechanism

The provider trait now lives in [`crates/goose-provider-types/src/base.rs`](https://github.com/block/goose/blob/main/crates/goose-provider-types/src/base.rs): `trait Provider { get_name, stream, complete, fetch_supported_models, configure_oauth, refresh_credentials }` where `configure_oauth` and `refresh_credentials` default to an error, plus `trait ProviderDescriptor { metadata() -> ProviderMetadata }`. [`crates/goose/src/providers/base.rs`](https://github.com/block/goose/blob/main/crates/goose/src/providers/base.rs) re-exports it and adds `trait ProviderDef` with `from_env` constructors. [`crates/goose/src/providers/init.rs`](https://github.com/block/goose/blob/main/crates/goose/src/providers/init.rs) holds a static `OnceCell<RwLock<ProviderRegistry>>`, `register` / `register_with_inventory`, calls `register_declarative_providers`, and exposes a `create(name, extensions) -> Arc<dyn Provider>` factory.

Declarative providers span two files: [`crates/goose/src/config/declarative_providers.rs`](https://github.com/block/goose/blob/main/crates/goose/src/config/declarative_providers.rs) re-exports `goose_providers::declarative::*` and owns `custom_providers_dir()` (`<config dir>/custom_providers/*.json`, written via `write_private_file`). The types live in [`crates/goose-providers/src/declarative.rs`](https://github.com/block/goose/blob/main/crates/goose-providers/src/declarative.rs): `DeclarativeProviderConfig { name, engine: ProviderEngine, display_name, api_key_env, base_url, models, headers, requires_auth, dynamic_models, env_vars, auth }`. `ProviderEngine` is a closed enum: `openai`/`openai_compatible`, `anthropic`/`anthropic_compatible`, `ollama`/`ollama_compatible`; `from_str` rejects everything else. An optional `auth.command` runs a program to fetch the credential and is validated as mutually exclusive with `api_key_env`.

[`crates/goose-providers/src/api_client.rs`](https://github.com/block/goose/blob/main/crates/goose-providers/src/api_client.rs) has `enum AuthMethod { NoAuth, BearerToken, ApiKey { header_name, key }, Custom }`, applied at request build time with redacted `Debug` impls. `NoAuth` is the local-endpoint path.

### What AI Fuel borrows

- The declarative provider is the direct model for `[[providers]]` config: name, engine, base_url, api_key_env, static model list, requires_auth, headers. Goose proves a no-code provider works when `engine` selects a compiled wire implementation.
- `AuthMethod` is the credential-as-union applied at request time, including `NoAuth` for local endpoints and a named-header `ApiKey` variant (not every provider takes `Authorization: Bearer`).
- `fetch_supported_models` with a `dynamic_models` switch (call `/v1/models`, or use the static list, or API-with-fallback) is the model for AI Fuel's optional model discovery on HTTP integrations.

### What AI Fuel avoids / caveats

- The engine set is closed. A declarative provider cannot invent a wire protocol; it selects one of three compiled engines. AI Fuel's `[[providers]]` config inherits exactly this constraint: `protocol` names a compiled `WireApi`, never a user-supplied format.
- Mutual exclusivity of `api_key_env` and `auth.command` is a runtime validation, not a type-level union. AI Fuel makes the exclusivity structural instead (`AuthBinding` is a union; exactly one method is active per binding).
- Goose's `Provider` trait mixes concerns AI Fuel keeps separate: streaming/complete are the execution contract, `configure_oauth`/`refresh_credentials` are credential lifecycle. In AI Fuel the monitoring contract and the credential store stay outside the execution adapter.

## charmbracelet/catwalk + charmbracelet/crush (Go)

### Mechanism

catwalk is a pure-data catalog library. [`pkg/catwalk/provider.go`](https://github.com/charmbracelet/catwalk/blob/main/pkg/catwalk/provider.go) defines `Provider { ID: InferenceProvider, Name, APIEndpoint, APIKey, Type, DefaultLargeModelID, DefaultSmallModelID, Models }`, a `Type` string enum (`openai`, `openai-compat`, `anthropic`, `google`, `azure`, `bedrock`, `google-vertex`, `vercel`, `openrouter`, `hyper`), and `Model { ID, Name, CostPer1MIn/Out (+cached variants), ContextWindow, DefaultMaxTokens, CanReason, SupportsImages }`. No I/O, no auth logic; provider JSON is data.

crush consumes it. [`internal/config/config.go`](https://github.com/charmbracelet/crush/blob/main/internal/config/config.go) defines `ProviderConfig` as a superset: `ID, Name, BaseURL, Type: catwalk.Type, APIKey, APIKeyTemplate, OAuthToken *oauth.Token, Disable, ExtraHeaders, Models []catwalk.Model, ChatGPTModels []catwalk.Model, AutoDiscoverModels *bool`. [`internal/config/load.go`](https://github.com/charmbracelet/crush/blob/main/internal/config/load.go) `configureProviders` merges the catwalk known-providers list with user config, resolving `$VAR` and `$(cmd)` through a `VariableResolver` (`NewShellVariableResolver`). [`internal/agent/coordinator.go`](https://github.com/charmbracelet/crush/blob/main/internal/agent/coordinator.go) `buildProvider` switches on `providerCfg.Type` to pick the wire client. `/v1/models` auto-discovery is a per-provider flag (`discover_models`), merging discovered models under user-listed ones. The catalog itself refreshes over the network from `defaultCatwalkURL = https://catwalk.charm.land`, falling back to cached or embedded data.

### What AI Fuel borrows

- catwalk is the proof that a provider catalog can be a pure-data library with no code paths: id, endpoint, wire type, model metadata, cost hints. AI Fuel's catalog entries for BYOK/local providers are the same shape.
- `ChatGPTModels` is honest prior art for a real problem: one provider id can serve a different model catalog depending on which credential is bound (subscription OAuth vs API key). AI Fuel's per-integration model list inherits this; the catalog is a property of the binding, not just the provider.
- `discover_models` as an explicit per-integration flag matches AI Fuel's rule that model discovery never runs implicitly inside quota collection.

### What AI Fuel avoids / caveats

- `ProviderConfig` holds `APIKey` and `OAuthToken` as two separate optional fields; the either-or rule is convention, not enforced by the type. AI Fuel instead makes the credential a union (`AuthBinding`), so exactly one method is active per binding and invalid states are unrepresentable.
- crush refreshes its provider catalog over the network during config load. AI Fuel must never do that inside offline Provider Discovery; if a remote catalog is ever used, it is an explicit, timestamped refresh with cached fallback, kept outside the discovery path.
- `ExtraHeaders` with shell expansion (`$(cmd)`) is powerful and dangerous; if AI Fuel adopts env/command interpolation in provider config it needs the same strict error contract crush applies (failed expansion aborts load, empty results are omitted).

## Cross-cutting patterns

### The convergent architecture

| Concern | codex-rs | opencode | zed | goose | catwalk/crush | AI Fuel (ADR-0005) |
| --- | --- | --- | --- | --- | --- | --- |
| Descriptor as data | `ModelProviderInfo` | models.dev record + config `provider` | provider settings | `DeclarativeProviderConfig` | catwalk `Provider` / `ProviderConfig` | `ExecutionConfig::Http` + catalog entry |
| Credential union | `CodexAuth` enum | `Auth.Info` union | `ApiKeyState` + OAuth per provider | `AuthMethod` enum | `APIKey`/`OAuthToken` fields (not a union) | `AuthBinding` union |
| Auth resolved at request time | `resolve_provider_auth` | `Auth.get` per provider | `load_if_needed` | `ApiClient` auth application | `buildProvider` per `Type` | resolver over `CredentialStore` |
| No-auth local endpoint | `UnauthenticatedAuthProvider` | `api` record optional | key absence = unauthenticated | `AuthMethod::NoAuth` | empty key | `AuthBinding::None` |
| Local endpoint built-ins | `create_oss_provider` (11434/1234) | config providers | api_compatible settings | declarative `ollama` engine | user `ProviderConfig` | `[[providers]]` + built-ins |

All five keep endpoint configuration separate from credentials at the data level, but only Zed enforces the binding at lookup time.

### Credential file conventions

- opencode: `~/.local/share/opencode/auth.json`, mode `0o600`, env override `OPENCODE_AUTH_CONTENT`.
- codex-rs: `auth.json` inside `codex_home` (`~/.codex` by default), loaded once by `AuthManager`.
- zed: OS keychain keyed by provider URL, env var taking precedence.
- AI Fuel: `~/.config/aifuel/credentials.json` per ADR-0005, distinct from provider-owned credential sources. Precedence follows the shared pattern: explicit env/config source over stored credential; keychain is out of scope for now.

### OAuth split

Two flow families appear:

- Device flow: Copilot in Zed (`PromptUserDeviceFlow`) and opencode (`CopilotAuthPlugin`). Note the two-token reality: the GitHub device-flow token is itself exchanged for a separate, expiring Copilot session token. A stored credential is therefore a chain, not a single token; ADR-0005 ships the Copilot device flow first.
- Browser + PKCE loopback: ChatGPT in codex-rs (`server.rs`, `pkce.rs`) and Anthropic-style flows in opencode plugins. Both are public clients with a loopback redirect; RFC 8252 applies, and AI Fuel registers its own public client rather than borrowing a provider CLI's client id.

### What nobody solved

- Cross-process refresh races. opencode's `auth.json` is a non-transactional rewrite; codex-rs's `AuthManager` is single-process. AI Fuel's sidecar lock plus reread-recheck-refresh is strictly stronger than every reference; it is also new code, so the transaction rules deserve tests, not just review.
- Credential-to-endpoint binding. Only Zed's `api_compatible.rs` re-resolves a key when the URL changes, and only per-provider. Codex's API-key vs ChatGPT endpoint split shows the same coupling handled by convention. Nobody binds a stored credential to a user-overridden endpoint; AI Fuel does, and there is no upstream precedent to copy.
- Monitoring as a separate contract. opencode and crush have no quota monitoring at all; codex and goose monitor nothing outside their own billing paths. OpenRouter's `/key` endpoint is the cautionary example: it reports key-scoped credit, not a subscription. AI Fuel's optional per-integration `MonitoringConfig` is the differentiator here, and "speaks an inference protocol" must never imply "has a quota API."
- Union-typed credentials vs parallel optional fields. crush's `APIKey` + `OAuthToken` pair and goose's `api_key_env` + `auth.command` pair both rely on convention or runtime validation. Only codex-rs and opencode make the credential a real union; both are single-process stories.

## Follow-ups

- Pin every upstream file reference above to a commit SHA when the provider-integration spec or a follow-up ADR is finalized; all links here track moving default branches, and goose already moved its `Provider` trait between crates during this review.
- If BYOK monitoring is added for OpenRouter-style providers, verify per-endpoint whether the quota API is key-scoped or account-scoped before claiming a Quota Pool.
- Record which `discover_models`-style flags AI Fuel adopts per integration; crush's merge rule (user-listed models win over discovered ones) is the sane default.
