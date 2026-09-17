# Gateway protocol and authentication contract

For [Resolve gateway protocol and authentication contracts](https://github.com/ducquoc97/aifuel/issues/28).

Status: approved by Quoc, including the requested review corrections. This is the review artifact; the canonical resolution is recorded on the linked decision ticket. It does not establish implementation or verified integration support.

## Boundary and compatibility

- The AI Fuel MCP Gateway serves selected MCP Hosts over stdio. External MCP Connections use local stdio or remote Streamable HTTP, including JSON and SSE responses. The separate deprecated HTTP+SSE transport is excluded.
- Support exactly MCP `2025-11-25` on both sides initially. Negotiate honestly using that revision's initialization lifecycle; reject incompatible peers explicitly. Do not echo arbitrary requested versions as if implemented. Defer `2026-07-28` and cross-version translation.
- Each MCP Host launches its own gateway. Connections, request IDs, subscriptions, caches, and owned children are isolated per gateway process. No shared daemon or shared upstream sessions.
- Keep `aifuel mcp` as the read-only monitoring interface. Gateway external capabilities retain their effects. Gateway credential handling does not authorize Provider Discovery or monitoring to refresh, write, or reuse credentials.
- Configuration follows the approved per-user `aifuel/mcp.json`, central `servers`, `defaults`, and `agents` contract. Missing host overrides inherit defaults; explicit lists replace them, including empty lists. Load configuration and environment references once at gateway startup.
- Unknown selected server IDs or invalid definitions prevent startup. Missing credentials, unavailable commands, connection errors, or incompatible upstream versions affect that server; healthy servers remain available. Invalid global configuration is not converted into partial success.

## Capabilities

Support tools (`list`, `call`), resources (`list`, `templates/list`, `read`, `subscribe`, `unsubscribe`), prompts (`list`, `get`), and argument completion. Support list-change notifications, resource updates, request-scoped progress, cancellation, and protocol ping.

Advertise only implemented capabilities. Determine each upstream's optional support through negotiation; unsupported completion or subscriptions produce explicit errors. Do not advertise sampling, elicitation, roots, upstream protocol logging, durable tasks, MCP Apps, or other unimplemented extensions. Reject unsolicited excluded requests according to the protocol rather than hanging or pretending success.

Exclude tools whose `execution.taskSupport` is `required`, with a sanitized unsupported-capability diagnostic. Tools with optional task support remain available as ordinary calls; set their gateway-facing task support to `forbidden`. Never downgrade a task-required tool into a normal call. This capability adaptation is an explicit exception to metadata preservation.

Preserve upstream schemas, tool annotations, prompt roles, multimodal content, structured results, and protocol-defined metadata except for required gateway identity and request-token translation. Annotations do not establish trust or read-only behavior. Preserve the distinction between tool execution errors and JSON-RPC errors. Never replace upstream failures with successful empty results.

Forward progress only to its originating request and only when requested. Progress never extends the absolute operation deadline. Keep raw upstream protocol logs and stderr out of host protocol output and gateway diagnostics.

## Stable identity and routing

Use the configured server ID, never an upstream display name, as the namespace. Resolve every host request against the selected server set. Never route unknown identities by guessing or falling back to another server.

Approved name encoding:

1. Encode each UTF-8 byte of server ID and upstream tool/prompt name independently. Preserve ASCII letters, digits, hyphen, and period. Encode every other byte as `_HH`, uppercase hexadecimal, including underscore.
2. Join the two components with `__`. Example: server `docs` and tool `search` become `docs__search`. Escaping underscore makes the boundary unambiguous.
3. If the result exceeds 128 ASCII characters, use its first 62 characters, then `__`, then the 64 lowercase hexadecimal characters of SHA-256 over the complete unshortened encoding. A routing table retains the original pair; shortening is not decoded.
4. Detect duplicate emitted identities and reject the conflicting capability explicitly; never overwrite a route or rename an existing capability. Names remain stable across restarts, discovery order, and additions of unrelated servers. Renaming a catalog server ID intentionally changes its namespace.

Approved resource URI encoding:

Replace only the original absolute URI's scheme with:

`aifuel-resource+<lowercase-hex-UTF8-server-id>+<lowercase-hex-original-scheme>`

Example: server `docs`, resource `file:///guide.md` becomes `aifuel-resource+646f6373+66696c65:///guide.md`. For `https://[::1]/guide`, the same transformation preserves the IPv6 authority in its original syntactic position.

The colon and everything after it remain unchanged, including authority, escaping, query, and fragment. Hex-encode the original scheme's exact ASCII bytes, preserving its original case on reversal. Decode the server ID and original scheme when routing; never parse/reserialize or normalize the upstream remainder. Apply the same scheme replacement to URI templates with a literal scheme, preserving expressions and expansion behavior after the colon. Variable-scheme templates and relative resource references are explicitly unsupported initially and reported as such. Validate the custom URI/URI-template contract; never silently alter unsupported forms. Template expansion before or after scheme replacement must produce the same routed upstream URI. Real-host acceptance must check long custom schemes and that host parsing does not change identity.

Rewrite server-routed resource identities in resource lists/templates/read contents, tool resource links and embedded resources, prompt embedded resources, subscription updates, and completion references. Preserve HTTP(S) resource links returned in tool/prompt content as direct web links so clients can fetch them normally, including from tool-only upstreams. Such direct links are not gateway routes or subscription identities. Catalogued resources/templates still use server-scoped identities for MCP reads, subscriptions, and completion. Do not introduce gateway HTTP fetching for arbitrary returned links; the host controls direct retrieval. Do not rewrite arbitrary arguments, structured JSON, prose, icons, or unknown extension metadata. Server-routed resources remain server-scoped even if two upstreams advertise the same original URI. No implicit host-filesystem access follows from an upstream `file://` URI.

## Discovery, pagination, and live changes

- Connect selected servers lazily, in parallel, on the first capability discovery request, then reuse healthy connections. Coalesce simultaneous connection attempts to the same server. A directly addressed request may trigger connection/discovery, but is never automatically replayed after failure.
- First listing waits only for the bounded discovery window. Return healthy results and report individual failures separately through sanitized diagnostics. If no server has a usable result, report failure rather than a misleading successful empty list. An explicitly empty selection or healthy servers with no matching capabilities may return empty lists successfully.
- Retain known tool/resource/prompt entries through an outage. Calls to their unavailable server fail explicitly. A server that has never completed discovery contributes no invented entries.
- Use complete, validated per-server list snapshots. Configurable defaults: 8 MiB per decoded JSON message, 32 MiB of decoded JSON per server/list snapshot, and 10,000 entries per snapshot. Bound buffering before full allocation; reject oversized or endlessly paginated discovery as a server-scoped failure and retain its previous successful snapshot. Aggregate in deterministic server-ID/identity order. Host-visible pagination is required whenever the aggregate cannot fit the downstream message budget, measured after identity rewriting and JSON serialization including the response envelope. Cursors are opaque, gateway-owned, bound to the listing kind and snapshot, and cannot be sent to a different upstream. A single rewritten entry that cannot fit is an explicit server-scoped discovery failure; retain that server's previous snapshot rather than silently truncating an entry. Reject stale/invalid cursors explicitly. Never expose only an upstream's first page as its complete list.
- On negotiated upstream list changes, refresh the affected list and notify the host when its aggregate changes. Commit a refreshed list atomically, including genuine removals. Keep the last successful list if refresh fails.
- After reconnecting, rediscover capabilities, restore active supported subscriptions, and notify the host that subscribed resources may have changed. Do not promise replay of missed updates. Report subscriptions that can no longer be restored. Translate resource identities and deduplicate upstream subscriptions while retaining host ownership.

## Requests, limits, recovery, and shutdown

Defaults, configurable per server unless noted:

| Limit | Default |
| --- | --- |
| Connection, including protocol initialization | 15 seconds |
| Discovery, including connection and required list pagination | 30 seconds |
| Tool calls, resource reads, prompt retrieval, completion, and other finite operations | 120 seconds |
| Graceful owned-process shutdown | 5 seconds |
| Concurrent external requests per server | 8 |
| Concurrent external requests per gateway | 64, gateway-wide setting |

Use absolute deadlines. A discovery's connection attempt is bounded by both its 15-second connection limit and remaining discovery budget. Operation deadlines include any required connection work. A long-lived event stream or subscription is not a 120-second finite operation. Bound finite setup/recovery requests separately.

Reject excess work with a clear busy error; do not build an unbounded queue. Long-lived notification streams do not occupy finite-request slots. Cancellation and shutdown control traffic must remain possible when request slots are full. Map request IDs, progress tokens, and subscription ownership per upstream and host request.

The 8/64 limits count gateway-managed in-flight requests, not guaranteed active remote executions. A cancelled or timed-out request releases its slot; the upstream may continue work. Remove its bookkeeping, never reuse its upstream request ID within the connection, and discard unknown/late responses. Do not retain an unbounded cancelled-request history.

Bound host-facing queued output by serialized bytes, default 16 MiB gateway-wide, with an 8 MiB downstream message limit. Coalesce pending list-change notifications by list kind, resource-change notifications by URI, and progress by request/token, retaining the latest state. Do not drop final responses. Use bounded backpressure when responses cannot be queued; continue deadline/cancellation handling independently of the stdout writer. If stdout makes no write progress for 30 seconds while output is pending, fail the host connection and perform owned-process cleanup. Bound coalescing tables within the same output budget; if safe coalescing cannot free space, terminate explicitly rather than silently lose responses. Include stalled-reader and notification-flood acceptance cases.

Cancellation or timeout ends the gateway's wait promptly and sends upstream cancellation where supported. Initialization is the exception: never send a cancellation notification for `initialize`; on initialization timeout close the connection and clean up its owned child. Ignore late responses safely. Do not kill an otherwise shared child to cancel one request. Report that cancellation cannot undo external effects; an interrupted operation may have an unknown outcome. Never replay tool calls or other uncertain operations automatically.

For transient failures, retry connection/discovery with delays of 1, 2, 4, 8, 16, then 30 seconds, with jitter, for at most five minutes. Jitter: independently choose 80-100% of each delay, retaining the 30-second cap. Apply the five-minute wall-clock bound across delays and connection attempts. A later discovery/use request may start a new bounded recovery cycle; it is not automatically replayed. Authentication and configuration failures require correction and a new gateway process. No token refresh or browser login occurs.

SSE response-stream resumption is distinct from connection/session recovery. Retain per-stream event IDs and respect the upstream SSE `retry` delay, even when longer than 30 seconds. Resume an interrupted resumable stream with HTTP GET and its `Last-Event-ID`, never by resending the original POST. Response redelivery is not operation replay. Keep the original absolute request deadline; if the required delay would exhaust it, time out instead of reconnecting early. Deduplicate redelivery by stream/event identity with bounded state. A session-specific 404 requires fresh initialization without the expired session ID; never replay the failed operation automatically. Persistent subscription streams use the bounded recovery window while respecting the server's minimum retry delay.

On host stdin EOF, termination, or protocol shutdown: stop accepting work, cancel outstanding requests, close event streams/subscriptions and HTTP sessions where supported, and close local child stdin. Allow the configured grace period, then forcibly clean up and reap only gateway-owned process trees using platform-specific ownership. Never terminate unrelated processes or remote servers. Verify graceful and forced cleanup on the target platforms; document that abrupt OS termination cannot promise graceful cleanup.

## Local process contract

Launch a configured executable with an argument array, without an implicit shell. Resolve commands through the permitted PATH; do not install software. An explicitly configured shell executable is a user-selected program, not a hidden interpretation step.

Use a configured absolute `cwd`, defaulting to the user's home directory resolved at the executable boundary. Do not inherit the launching agent's project directory implicitly.

Approved minimal inherited environment:

- Unix-like platforms: `PATH`, `HOME`, `TMPDIR`, `LANG`, and `LC_ALL`, if present.
- Windows: `PATH`, `SystemRoot`, `WINDIR`, `COMSPEC`, `PATHEXT`, `USERPROFILE`, `HOMEDRIVE`, `HOMEPATH`, `TEMP`, and `TMP`, if present.
- All other values require explicit per-server literal values or environment references. Case-insensitive Windows keys must be checked for duplicate/conflicting entries.

Treat explicit environment values as potentially sensitive. Do not automatically inherit unrelated credentials, proxy settings, or runtime injection variables. This limits accidental inheritance; it is not an OS security boundary for child processes running as the same user. Drain child stderr to avoid deadlocks without exposing its raw content.

## Remote credentials and diagnostics

- Support public endpoints, explicit bearer tokens, and named secret headers such as `X-API-Key`. Secret values come only from named gateway environment variables, resolved at startup. No literal remote secrets in the catalog, URLs, arguments, receipts, or generated registrations.
- Do not reuse AI coding Provider Credential Sources. Credentials are specific to the configured external endpoint and are not forwarded between servers. This is static credential injection, not OAuth discovery, registration, consent, refresh, or scope escalation.
- Require HTTPS except plain HTTP to loopback. Loopback rule: literal `127.0.0.0/8`, `::1`, or `localhost` resolving exclusively to loopback. Reject URL userinfo and fragments. Reject redirects and report that the final endpoint must be configured explicitly. Never forward secrets to a redirect target.
- Named headers cannot override `Host`, connection/framing headers, content negotiation, protocol/session headers, or bearer `Authorization`. Reject duplicate header names case-insensitively, CR/LF values, and missing/empty referenced secrets. Additional permitted secret headers are sent only to the configured server.
- Invalid/missing credentials affect that server. Report authentication failure or insufficient permission without printing response bodies or secrets. Changed environment values require a new gateway process. Never retry authentication indefinitely.
- Generated logs contain server/request identifiers and sanitized categories. Exclude tokens, header values, arguments, results, raw URLs containing sensitive query values, upstream error bodies, and raw child stderr. No raw diagnostic capture mode in this release.
- Preserve legitimate upstream protocol results for the requesting host; log sanitization does not authorize rewriting tool results or guarantee an upstream never returns sensitive content.

## Catalog fields and example

These fields complete the transport-specific part of the approved configuration decision. Reject unknown transport/auth types and invalid limits before running the affected configuration. These approved names and field shapes are not claimed to exist in the CLI today.

```json
{
  "servers": {
    "docs": {
      "transport": "stdio",
      "command": "/opt/example/bin/docs-mcp",
      "args": [],
      "cwd": "/home/example",
      "env": { "DOCS_MODE": { "value": "public" } },
      "envFrom": { "DOCS_TOKEN": "DOCS_MCP_TOKEN" }
    },
    "deepwiki": {
      "transport": "streamable-http",
      "url": "https://mcp.deepwiki.com/mcp"
    },
    "private": {
      "transport": "streamable-http",
      "url": "https://example.com/mcp",
      "auth": { "bearerTokenEnv": "PRIVATE_MCP_TOKEN" },
      "secretHeaders": { "X-API-Key": { "env": "PRIVATE_MCP_API_KEY" } },
      "limits": {
        "connectSeconds": 15,
        "discoverySeconds": 30,
        "operationSeconds": 120,
        "shutdownSeconds": 5,
        "maxConcurrentRequests": 8,
        "maxMessageBytes": 8388608,
        "maxListSnapshotBytes": 33554432,
        "maxListEntries": 10000
      }
    }
  },
  "defaults": ["docs", "deepwiki"],
  "agents": { "codex": { "servers": ["deepwiki", "private"] } },
  "gateway": {
    "maxConcurrentRequests": 64,
    "maxMessageBytes": 8388608,
    "maxOutputBufferBytes": 16777216,
    "outputStallSeconds": 30
  }
}
```

`auth` and `secretHeaders` are optional; omitting both selects unauthenticated access. They may coexist only with distinct, permitted headers. `env` supplies explicit literal local-process values; `envFrom` maps child variable names to gateway environment names. Reject overlap rather than inventing precedence. Environment references are never interpolated into command/argument strings. Limits must be positive bounded numeric values; concrete parser ranges are implementation details and must be documented. Paths are illustrative and platform-specific.

## Acceptance and delivery gates

Each path needs its own implementation slice and runnable acceptance evidence. Closing this decision ticket completes planning only.

1. Local stdio: deterministic external fixture serves tools, resources/templates, prompts, completion, progress, and updates through a real gateway process. Verify argument-array execution, cwd/environment rules, timeout/cancellation, and owned-process cleanup.
2. Public Streamable HTTP: a fixture covers JSON/SSE, initialization, session headers, session loss, cancellation, and subscriptions. DeepWiki must separately complete a real compatible handshake, discovery, and tool call. Its documentation alone does not establish `2025-11-25` compatibility or resource/prompt coverage.
3. Static bearer: controlled HTTP server verifies correct endpoint/header placement, missing/expired token behavior, 401/403 handling, no refresh, no credential logging, and no cross-server/redirect leakage.
4. Named secret headers: a separate controlled acceptance case verifies header restrictions, combined bearer/header configuration, environment resolution, and failure/redaction behavior.
5. Aggregation: exercise duplicate upstream names and URIs, delimiter characters, long names, digest collision handling with a controlled seam, literal-scheme URI templates and reserved expansions, IPv6 authorities, explicit rejection of variable-scheme templates, resource references in results, pagination/size limits, stale cursors, list changes, and completion reverse routing. No arbitrary-string rewriting.
6. Failure/concurrency: mix healthy, unavailable, incompatible, and unauthenticated servers; retain known lists; reject unknown selected identities; exercise 8/64 limits, absolute deadlines under progress, late responses, subscription restoration, capped backoff, and no operation replay.
7. Real hosts: Codex CLI, Claude Code, and GitHub Copilot CLI each load their registration, negotiate the selected version, and call through separate gateway processes. Record exact host version/platform/results. All three target Linux/macOS; Claude/Copilot also target native Windows, Codex targets WSL2. Existing observed versions and fixture success are not live acceptance.
8. Preserve monitoring and configuration boundaries: the monitoring interface remains read-only; gateway commands do not broaden discovery credential permissions. Verify default inheritance, explicit replacements, empty selections, startup-only activation, and setup receipts remain consistent with the configuration decision.

Additional review-regression acceptance: tool-only upstream returning a directly fetchable HTTPS resource link; required versus optional task tools; a 32 MiB list split into byte-bounded downstream pages after rewriting; an individually oversized entry; notification floods and stalled stdout; cancellation followed by late upstream completion; SSE disconnection with `retry: 60000` and event-ID resumption without duplicate execution.

Required transport/auth slices must block final integration acceptance. Any live endpoint/host that cannot meet the chosen protocol scope is a visible blocker requiring a new compatibility decision, not permission to silently widen versions or claim support.

## Evidence

Review corrections follow the MCP 2025-11-25 [transport rules](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), [task negotiation](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks#tool-level-negotiation), and [resource URI semantics](https://modelcontextprotocol.io/specification/2025-11-25/server/resources#https). A live public DeepWiki initialization during review returned protocol `2025-11-25` and server version `2.14.3`; no gateway, host registration, or tool call was verified.

[Primary-source protocol research](gateway-protocol-contracts.md) documents sources and separates documented behavior from unperformed live acceptance. The protocol specification, not an implementation's version echo, defines compatibility.

## Tracker handoff

- Publish the approved answer as a resolution comment on [Resolve gateway protocol and authentication contracts](https://github.com/ducquoc97/aifuel/issues/28), then close that decision. Link this research/review artifact when it has a durable committed URL; never imply uncommitted files are published.
- Add a linked gist to [Wayfinder: centralized external MCP gateway contracts](https://github.com/ducquoc97/aifuel/issues/37). Move the explicitly deferred versions, transports, login lifecycle, and optional features into Out of scope. Remove graduated fog. Close the map only if a fresh child/dependency check confirms no decision remains.
- Link the resolution from [Spec: extensible Rust provider adapters and centralized MCP Gateway](https://github.com/ducquoc97/aifuel/issues/24), replacing its open-decision placeholders without duplicating the full contract. Preserve the implementation completion checklist.
- Keep the existing local and remote transport slices. Update their acceptance criteria to reference this contract and the agreed limits, progress, cancellation, and compatibility gates. Narrow [Use an authenticated external MCP server](https://github.com/ducquoc97/aifuel/issues/31) to the bearer-token path explicitly.
- Add **Use named secret headers with external MCP servers** under the implementation spec: exercise environment-referenced custom headers, bearer coexistence, forbidden headers, destination binding, redirects, missing credentials, and sanitized errors through a real gateway and controlled HTTP server. Depend on the remote transport slice; require bearer slice completion for coexistence acceptance.
- Add **Expose gateway resources, templates, and live subscriptions** under the implementation spec: verify resource identity transformation, fixed-scheme template expansion, IPv6/escaping, reads, pagination/size limits, list changes, outage retention, restoration, and unsupported forms over both chosen transports. Depend on local/remote transport and multi-server routing slices.
- Add **Expose gateway prompts and argument completion** under the implementation spec: verify namespaced prompt listing/get, message/content preservation, embedded resource rewriting, live prompt-list updates, and prompt/resource-template completion with correct reverse routing. Depend on multi-server routing and the resource slice.
- Wire native blocking edges from these new implementation slices to [Verify installed workflows and complete the review loop](https://github.com/ducquoc97/aifuel/issues/36). They are implementation work, not new decision children of the completed wayfinder map. Record real host/version/platform evidence separately from controlled fixtures. Do not close the implementation spec or acceptance tickets.
