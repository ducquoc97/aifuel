# AI Fuel

AI Fuel identifies locally configured AI coding providers and reports their subscription quota.

## Language

**Catalog Provider**:
An AI coding provider represented in AI Fuel's coverage catalog, including providers whose monitoring or execution capabilities are unavailable, unknown, or unsupported. Catalog membership alone does not establish a working integration.
_Avoid_: Supported provider when only catalog membership is known

**Supported Provider**:
An AI coding provider for which AI Fuel has a built-in quota integration. Supported providers form a static catalog and are not necessarily present on a user's machine.
_Avoid_: Active provider, available provider

**Provider Discovery**:
A fast, local, side-effect-free check of which providers have a provider-specific credential source present. Discovery runs before each quota collection and never contacts provider APIs, refreshes tokens, or writes credentials.
_Avoid_: Provider validation, provider authentication

**Discovered Provider**:
An AI coding provider whose provider-specific local credential source is present. A discovered provider remains visible when credential validation or quota retrieval fails.
_Avoid_: Found provider, installed provider, authenticated provider

**Discovery Failure**:
The condition where AI Fuel cannot determine whether a provider-specific credential source is present. The provider is excluded from the discovered set, while the application reports the failure separately.
_Avoid_: Undiscovered provider, provider error

**Provider Credential Source**:
A local credential store created specifically for an AI coding provider. A general account login, such as GitHub CLI authentication, is not a credential source for a related provider such as GitHub Copilot.
_Avoid_: Shared login, reusable credential

**Codex Rate-Limit Reset Credit**:
An earned Codex allowance that can reset eligible rate-limit windows when explicitly redeemed. A credit has an available count and can include a reset type, description, and expiry.
_Avoid_: Quota-window reset, purchased credit

**Provider Account**:
A provider-specific account and its applicable billing or access scope, such as organization, project, workspace, tenant, or region. Two credential sources identify the same account scope only when matching evidence establishes that relationship.
_Avoid_: Credential, email identity

**Advertised Model**:
A model that evidence identifies as part of a provider's catalog. Advertisement does not establish account entitlement or execution availability.
_Avoid_: Runnable model, historical model ID

**Account Entitlement**:
An observed account's right to use a model or capability within a particular provider scope. Entitlement may be unknown independently of model advertisement and execution availability.
_Avoid_: Model availability, installed model

**Quota Pool**:
An allowance shared within an identified provider account scope, potentially consumed by several models. Models sharing a pool do not each have an independent copy of its remaining allowance.
_Avoid_: Per-model balance when allowance is shared

**Agent Integration**:
A provider-scoped integration with a supported agent execution interface, whose local presence and capabilities can be identified. Monitoring support alone does not establish an Agent Integration.
_Avoid_: Provider quota adapter, model API

**Agent Session**:
A provider-scoped agent conversation or execution context that can continue across multiple Agent Runs. A session retains its provider identity and known account and integration associations.
_Avoid_: Single run, quota window

**Agent Run**:
One execution attempt through an Agent Integration within an Agent Session. Each run has its own requested model, execution context, and outcome.
_Avoid_: Session, account usage

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

**Execution Availability**:
An observation of readiness to attempt a selected model through an Agent Integration in a particular account, platform, version, authentication, and permission context. Readiness does not guarantee that the provider will accept the run.
_Avoid_: Guaranteed execution, catalog availability

**AI Fuel MCP Server**:
AI Fuel's read-only interface through which an MCP host obtains provider status, model information, and quota observations.
_Avoid_: External MCP connection, agent launcher

**External MCP Connection**:
An association through which AI Fuel acts as a client of an external MCP server. It is distinct from the AI Fuel MCP Server and does not by itself establish an Agent Integration.
_Avoid_: AI Fuel MCP Server, provider account

**AI Fuel Execution MCP Server**:
AI Fuel's separately started execution interface through which an MCP host resolves and manages Agent Runs owned by that connection. It shares the application run-management contract with the CLI. It cannot grant permission approvals and is separate from the read-only AI Fuel MCP Server and the external MCP Gateway.
_Avoid_: Monitoring server, MCP Gateway, shared run daemon

**AI Fuel MCP Gateway**:
The per-user shared access point through which selected local agents use external MCP servers configured once in AI Fuel. It is separate from the read-only AI Fuel MCP Server; external tools retain their own capabilities and effects.
_Avoid_: Provider monitor, agent launcher

**Agent MCP Registration**:
Configuration in a selected MCP Host that connects it to the AI Fuel MCP Gateway. External server definitions are managed in AI Fuel rather than repeated in each host's configuration.
_Avoid_: Provider credential, external server installation

**MCP Host**:
A local client identified in AI Fuel's gateway configuration that consumes external server capabilities through the gateway. A host may have a registration adapter without supporting Agent Runs or having an Agent Integration.
_Avoid_: Agent Integration, external MCP server

**Provider Integration**:
A configured binding of a provider identity to an execution configuration and credential. It is the unit a user selects for an Agent Run or a monitoring collection. Two integrations may share one upstream provider, so a provider CLI integration and an API-key integration for the same provider can coexist.
_Avoid_: Catalog Provider, Supported Provider

**Integration Identity**:
The opaque, stable identifier of a Provider Integration. It names a configured integration without encoding its provider or authentication details for routing.
_Avoid_: Provider key, provider:mode compound ids parsed for routing

**Provider Integration Instance**:
A named selection overlay on one Provider Integration, declared in `providers.json` under `instances`. It carries the instance's own Integration Identity, its base integration, an environment overlay applied to the provider process at spawn, and an optional Managed Credential binding. It never changes the base integration's capabilities.
_Avoid_: Named profile, provider account alias

**Managed Credential**:
An AI Fuel-owned credential, such as an API key, an OAuth token set, or a pasted browser-session credential, stored in AI Fuel's credential store. It is distinct from a Provider Credential Source, which a provider CLI owns.
_Avoid_: User credential, app password

**Credential Reference**:
The opaque identity under which a Managed Credential is stored and shared by integrations. It names the credential slot, not the credential material itself.
_Avoid_: Credential value, stored secret

**Key Pool**:
The set of API-key Managed Credentials one Authentication Binding may draw on: the record at the bound Credential Reference plus every record whose reference extends it with a `/` suffix. A rate-limited key cools down while execution rotates to the next healthy member. Session credentials are single records and never join a pool.
_Avoid_: Shared key, credential bundle

**Session Credential**:
A Managed Credential holding pasted browser-session material - a bare session token or a copied `Cookie` header line - delivered to a `*:web` Provider Integration as the `Cookie` header rather than `Authorization: Bearer`. Entry is explicit paste or stdin only; nothing reads a browser profile or an OS keyring.
_Avoid_: Browser import, keychain read

**Authentication Binding**:
The association between an execution configuration and the credential it applies to requests: none, an API key, a session credential, or a managed OAuth credential. A configured endpoint does not imply one.
_Avoid_: Auth mode baked into a provider

**Wire Api**:
The HTTP request and response protocol that an Http execution configuration speaks, such as OpenAI chat completions, OpenAI responses, or Anthropic messages.
_Avoid_: SDK, provider type

**Configured Endpoint Provider**:
A provider integration whose presence is established by configuration rather than by a provider-owned credential file. It may still carry an Authentication Binding.
_Avoid_: Free provider, offline provider

**Monitoring Collection Contract**:
The optional per-integration contract that produces quota and usage observations with metric, unit, scope, and provenance. It is distinct from the execution contract, and an inference protocol does not imply it.
_Avoid_: Usage API, quota endpoint assumed from inference protocol
