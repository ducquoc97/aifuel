# MCP Gateway protocol contract research

Checked 2026-09-17 for Wayfinder's gateway contract decision. This is primary-source research, not an implementation or an accepted protocol contract. The user selected public external servers plus user-supplied tokens, without managed browser login or token refresh, and support for tools, resources, and prompts.

Additional user decisions supplied during research: keep healthy servers available after a partial upstream failure; connect lazily and in parallel at first discovery, then reuse connections; resolve remote tokens through environment references only; give local children a minimal runtime environment plus explicit per-server variables; exclude sampling, elicitation, and roots; support live list updates and resource subscriptions. Configurable per-server default deadlines are 15 seconds for connection, 30 seconds for discovery, 120 seconds for operations, and 5 seconds for cleanup. These are user choices, not MCP-mandated values.

Repository context: [CONTEXT.md](../../CONTEXT.md), [separate MCP roles](../adr/0001-separate-mcp-server-and-client-roles.md), [central gateway](../adr/0002-centralize-external-mcp-access.md), and [capability adapters](../adr/0004-organize-capability-adapters-behind-application-facade.md). The AI Fuel MCP Gateway consumes External MCP Connections; it does not change the read-only AI Fuel MCP Server.

## Protocol version is an explicit compatibility choice

**Documented fact:** the live official versioning page names `2026-07-28` as the current version, and `/specification/latest` redirects there. `2025-11-25` is the previous revision. `/specification/draft` remains a separate development specification. Search results describing the July revision as a release candidate are stale relative to these live pages. Sources: [versioning](https://modelcontextprotocol.io/docs/2026-07-28/learn/versioning), [current specification](https://modelcontextprotocol.io/specification/2026-07-28), [draft](https://modelcontextprotocol.io/specification/draft).

The current revision is a breaking protocol change, not merely another initialization version string. It removes initialization and protocol sessions, adds per-request metadata, mandatory `server/discover`, and a required `resultType`. It adds cache hints to resource and list results, and changes the available schema shapes. Experimental tasks move into an extension. Roots, sampling, and logging are deprecated, while still present during their deprecation window. The changelog is useful context, but individual protocol pages define the wire behavior. Source: [July revision changes](https://modelcontextprotocol.io/specification/2026-07-28/changelog).

**Decision implication:** separately state the versions served to each MCP Host and the versions accepted from external servers. Transport support alone does not establish protocol compatibility. Choosing an initial `2025-11-25` compatibility profile can be deliberate, but must not be described as supporting the latest protocol. Supporting both eras requires behavior conversion, not just message forwarding.

**Version-scope recommendation:** start with an explicitly bounded legacy compatibility profile if the selected SDK and required MCP Hosts are verified only against that era. Do not add dual-era support merely because both versions use the same transport names. The supplied local-code review reports that the existing monitoring server echoes the requested initialization version; such an echo is not evidence that it implements that version's contracts.

## Transport and lifecycle facts

| Concern | `2025-11-25` profile | `2026-07-28` profile |
| --- | --- | --- |
| Startup | `initialize`, negotiated capabilities/version, then `notifications/initialized` | Required version and client capabilities in request `_meta`; server implements `server/discover` |
| HTTP session | Optional server-issued `MCP-Session-Id`; return it on later requests | No protocol session identifier |
| HTTP messages | POST, JSON or SSE response; optional GET event stream | POST, JSON or request-scoped SSE response; `subscriptions/listen` for changes |
| Interrupted stream | Disconnect does not itself mean cancellation; optional SSE resumption | Close means cancellation; no `Last-Event-ID` resumption |
| Client input during execution | Server sends a request to client | `input_required` result, then client retries with inputs |

Sources: [legacy lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle), [legacy transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), [modern transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http), [modern multi-round-trip requests](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr).

For host-facing stdio, the host launches a subprocess. Messages are UTF-8, newline-delimited JSON-RPC; stdout cannot contain diagnostics. Logs belong on stderr. This framing also suits existing local external commands. Host-facing stdio does not require external servers to use stdio: the gateway acts as a separate HTTP client upstream. Source: [stdio transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio).

For legacy Streamable HTTP, clients must accept both JSON and SSE responses. A server-issued session ID must be retained; a session-specific 404 requires fresh initialization, and DELETE can terminate a session. The deprecated `2024-11-05` HTTP+SSE transport is distinct from SSE responses within Streamable HTTP. Excluding the former does not allow an implementation to ignore the latter. Source: [legacy transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports).

Modern HTTP also requires the prescribed metadata headers, including `MCP-Protocol-Version`, `Mcp-Method`, and relevant `Mcp-Name`; mirrors must agree with the body. Tool schemas can require argument-derived headers. A gateway changing a routed tool name must construct upstream headers from the upstream identity. Source: [modern Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http).

Official dual-era guidance uses a `server/discover` stdio probe, and an HTTP request plus recognized error inspection, before falling back to legacy initialization. A modern version error means select a supported modern version, not blindly fall back. The compatibility matrix warns that legacy clients cannot automatically advance to modern-only servers. Source: [versioning and compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning).

## Identity and aggregation

**Tools:** names should be case-sensitive, unique within a server, 1-128 characters, and use ASCII letters, digits, `_`, `-`, or `.`. The specification explicitly recommends disambiguation for aggregators; `serverInfo.name` is not guaranteed unique. The catalog's server id is consequently a better namespace input. Prefixing still needs an unambiguous encoding and an explicit policy when the combined name exceeds the limit. Preserve tool schemas and structured/multimodal results; metadata and annotations are not proof of trust. Source: [tool identities and content](https://modelcontextprotocol.io/specification/2026-07-28/server/tools).

**Prompts:** `name` is the identifier used by `prompts/get`; `title` is display text. Prompts can contain image, audio, and embedded resource content. Aggregation must route get requests by the advertised identity and preserve message roles and content. Sources: [legacy prompt definitions](https://modelcontextprotocol.io/specification/2025-11-25/server/prompts), [current prompt discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/prompts).

**Resources:** URI is identity; changing a display name does not resolve URI collisions. `resources/read` can return several content records. Templates are URI templates, not just concrete URI strings; their variables must remain usable after any gateway transformation. Custom URI schemes are allowed when RFC 3986 compliant. HTTP resource URIs are recommended only when the client can fetch them directly. Source: [resources, templates, and URI schemes](https://modelcontextprotocol.io/specification/2026-07-28/server/resources).

**Completion:** `completion/complete` routes a `ref/prompt` by prompt name or `ref/resource` by resource/template URI, with an argument name/value and optional prior arguments. Thus completion needs the same reverse identity mapping as prompt and resource access. Source: [completion](https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/completion).

**Recommendations, not protocol requirements:** use stable catalog ids for namespacing, explicitly cover separator ambiguity and length, and choose a reversible resource identity strategy. A URI wrapper must preserve template expansion semantics, including reserved expansion operators. A lookup table alone cannot route every template-generated URI unless its matching contract is defined.

If URI rewriting is selected, consider every protocol-defined resource location: discovery results, read contents, resource links and embedded resources in tool results, embedded resources in prompts, subscription updates, and completion references. Arbitrary tool argument strings, arbitrary structured JSON, prose, icons, and extension metadata do not automatically become resource identifiers. Rewriting arbitrary strings could corrupt application data. Extension-specific URI semantics require separate support. These are gateway design implications of the cited identity contracts, not a claim that MCP standardizes a gateway URI scheme.

## Discovery changes and client-side features

The legacy lifecycle negotiates independent `tools`, `resources`, `prompts`, and `completions` capabilities. List changes and resource subscriptions are optional sub-capabilities. Only negotiated capabilities should be used. Timeouts should cover all requests with a configurable request limit and a maximum duration even if progress resets an idle timer. Closing stdin and then terminating an unresponsive child is the documented stdio shutdown sequence. Source: [legacy lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle).

In the current protocol, `subscriptions/listen` replaces resource subscribe/unsubscribe and the HTTP GET event stream. The client chooses notification types and resource URIs; acknowledgment identifies the accepted subset. Notifications carry the listen request's subscription id, including on shared stdio. Reconnection requires resubscribing. Aggregation therefore needs request/subscription identity mapping and resource URI mapping together. Source: [subscriptions](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/subscriptions).

Legacy sampling asks the host to perform model generation through `sampling/createMessage`; merely forwarding tools does not implement it. Source: [sampling](https://modelcontextprotocol.io/specification/2025-11-25/client/sampling).

Legacy roots use `roots/list` and optional root-change notifications. Root URIs must use `file://`; forwarding them exposes host filesystem context to an external server and needs an explicit gateway policy. A namespaced resource URI is not a valid substitute for a filesystem root. Source: [roots](https://modelcontextprotocol.io/specification/2025-11-25/client/roots).

Elicitation has distinct form and URL modes and must respect advertised client support. Form mode cannot collect API keys or access tokens. URL elicitation for a server's third-party service is separate from authorization of the MCP connection itself. Therefore excluding managed MCP OAuth does not automatically decide whether to relay URL elicitation. Source: [elicitation](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation).

Current multi-round-trip requests replace unsolicited server JSON-RPC requests. `tools/call`, `resources/read`, and `prompts/get` may return `input_required`. Retries use new request ids and echo opaque `requestState` unchanged. The requested input must be supported by client capabilities. A gateway crossing protocol eras must either bridge this interaction faithfully or state it is unsupported; it must not accidentally report an interim result as successful completion. Source: [multi-round-trip requests](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr).

**Recommendation:** advertise only capabilities that work end to end for the current host and gateway implementation. Tools/resources/prompts support is not equivalent to supporting every optional client feature, extension, or host UI. The user has selected live list updates and resource subscriptions and excluded sampling, roots, and elicitation. Completion remains a separate contract choice. Do not advertise excluded features upstream.

## Cancellation, failures, and retries

Current cancellation is transport-specific: a stdio cancellation notification names the original request, while closing an HTTP response stream cancels that request. Timeouts should trigger the appropriate cancellation and stop waiting; progress cannot extend a request forever. Late responses and cancellation races must be tolerated. Source: [cancellation and timeouts](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/cancellation).

**Recommendation:** separate reconnecting a connection from replaying an operation. Do not infer that an interrupted tool had no effects. Per-upstream request-id and progress-token mappings must keep cancellation and completion attached to the correct MCP Host request. The user chose to keep healthy servers available after a partial failure. Define the resulting capability-list changes and explicit diagnostics for an unavailable server; never turn upstream failure into a successful empty result. These are product choices, not determined by the transport name.

## User-supplied tokens and OAuth

Authorization is optional. MCP's HTTP authorization specification describes an OAuth-based flow, while stdio normally receives credentials through its environment. Bearer access tokens go in the `Authorization` header on every HTTP request, never in URI query strings; servers validate audience and return 401 for invalid/expired tokens and 403 for insufficient permissions. Sources: [current authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization), [legacy authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization).

**Scope implication:** sending a user-provided, upstream-specific bearer token is a useful limited credential mode, not an implementation of OAuth discovery, registration, consent, refresh, or scope escalation. Servers that demand those flows will remain unsupported unless a usable token can be supplied externally. A provider-specific API-key header is another explicit mode, not automatically equivalent to bearer authorization. Never describe arbitrary personal tokens as valid for every MCP server.

**Recommendations still requiring a contract:** define bearer-only versus named headers; HTTPS and redirect restrictions; redaction; missing/expired-token errors; and whether changing a token requires the already-approved gateway restart. The user selected environment references for remote tokens and a minimal runtime environment with explicit per-server variables for local children. Bind supplied credentials to their configured external endpoint. Do not repurpose AI coding Provider Credential Sources or forward a host credential intended for AI Fuel to an unrelated upstream.

## DeepWiki evidence and acceptance boundary

Cognition documents DeepWiki as a free, remote, unauthenticated service for public repositories. The recommended Streamable HTTP endpoint is `https://mcp.deepwiki.com/mcp`; `https://mcp.deepwiki.com/sse` is the deprecated legacy endpoint. Documented tools are `read_wiki_structure`, `read_wiki_contents`, and `ask_question`. Private repository access is a separate Devin MCP service with a Devin API key. The page does not establish a particular negotiated protocol version or advertise resources/prompts support. Source: [official DeepWiki MCP documentation](https://docs.devin.ai/work-with-devin/deepwiki-mcp).

This research fetched documentation only. It did not initialize/discover a live DeepWiki MCP connection, list its live capabilities, call a tool, register a gateway with an MCP Host, or test subscriptions, authentication, resource templates, or prompts. DeepWiki is a candidate public-server acceptance fixture, not proof of the complete gateway contract. A controlled external server is also needed to exercise duplicate identities, templates, pagination, optional capabilities, and failures.

## Remaining decisions

1. Initial protocol versions on each side: legacy profile, modern profile, or explicit dual-era support; whether cross-era conversion is in scope.
2. Host-facing stdio and upstream stdio/Streamable HTTP coverage; whether deprecated HTTP+SSE is excluded.
3. Stable tool/prompt identity encoding and resource/template identity strategy, including length and collisions.
4. Completion support and exact live-notification/subscription behavior within the chosen version profile; excluded client features must not be advertised.
5. Capability changes after a partial failure, cancellation, and safe retry policy within the chosen deadline defaults.
6. Bearer-only versus additional credential headers, and the environment-reference refresh/restart lifecycle.
7. Acceptance cases beyond DeepWiki and the host/platform/version combinations that will actually be exercised.

## Subsequent review evidence

During contract review on 2026-09-17, a direct unauthenticated POST initialize request to `https://mcp.deepwiki.com/mcp` requesting `2025-11-25` returned that version and server version `2.14.3`. This updates the earlier documentation-only evidence boundary: no gateway process, real-host integration, or tool call was tested. Final decisions and requested review corrections are captured in [the contract review artifact](gateway-contract-review.md) and its linked decision ticket; the earlier remaining-decisions list records the research phase, not current open work.
