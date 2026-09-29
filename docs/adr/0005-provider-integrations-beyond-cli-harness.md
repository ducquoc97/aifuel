# Model provider integrations beyond the CLI harness

Each selectable integration is an Integration: an opaque IntegrationId, a ProviderId keyed to the catalog (a plain string, no longer only the ProviderKey enum), an ExecutionConfig, and an optional MonitoringConfig. Two integrations may share one upstream provider, so a provider CLI integration and an API-key integration for the same provider coexist as separate selections for Agent Runs and monitoring collections.

An ExecutionConfig is either Cli { adapter }, which keeps the existing spawned provider-CLI harness, or Http { endpoint, protocol: WireApi, auth: AuthBinding }. AuthBinding is None, ApiKey { source, delivery }, or OAuth { credential: CredentialRef, profile }. The endpoint is configuration and authentication is a separate binding; they are not a free product, because a configured local endpoint such as Ollama or LM Studio can still require a key. Monitoring is a separate optional contract per integration with its own credential binding; speaking an inference protocol does not imply that a quota API exists.

AI Fuel-owned credentials live in a CredentialStore at ~/.config/aifuel/credentials.json, distinct from provider-owned credential sources. Store access follows cross-process transaction rules: a sidecar lock serializes writers, token refresh rereads, rechecks, refreshes, and persists under the lock, an omitted refresh token in a response preserves the stored one, a malformed store is never overwritten, a possibly-consumed refresh token is never blindly retried after an ambiguous network failure, and the crash window between refresh and persist is documented. Credentials are bound to approved destinations, so a configured endpoint override never inherits a built-in credential.

Provider Discovery stays offline and side-effect-free. It reports evidence states (configured, credential present, installed, last validated) rather than readiness.

The compiled adapter-factory registry from ADR-0003 remains, joined by an owned runtime registry built from built-ins plus validated user configuration. Versioned decoding and aliases migrate stored runs, selection configuration, and MCP schemas. OAuth uses AI Fuel's own registered public client only (RFC 8252); an integration ships only after demonstrated client access, with the Copilot device flow first and Anthropic OAuth experimental.

Wire integrations declare a new AgentCapability::PromptCompletion. WorkspaceWrite, ExternalMcpTools, and Resume are declared unsupported for them until a tool loop exists.

Trade-offs are accepted. A string ProviderId loses compile-time exhaustiveness, mitigated by registry validation and reserved ids. The runtime registry adds startup construction, mitigated by deterministic ordering and explicit errors. Keeping the ProviderKey enum for the six CLI adapters preserves the proven harness while the wider catalog uses string ids.
