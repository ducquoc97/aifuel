# Provider credential and data-source feasibility

Status: **Resolved findings for issue #11**. Research date: **2026-09-14**.

## Scope and canonical input

This audit consumes the completed [issue #8 provider and capability inventory](https://github.com/ducquoc97/aifuel/blob/a43fd99/docs/research/2026-09-14-codexbar-provider-inventory.md). That artifact owns the provider identifiers, product/account types, monitoring metrics, pinned source locations, model-catalog observations, agent-session scope, and upstream platform notes. This file does not replace that inventory or choose a smaller parity target.

The audit covers the 69 provider IDs in that matrix. `pi` and `omp` are agent-session surfaces in the inventory, not provider IDs, and are assessed separately below. The pinned upstream source is [CodexBar commit `caad1ca`](https://github.com/steipete/CodexBar/commit/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9). Windows source evidence uses [Win-CodexBar commit `c65fd78`](https://github.com/nesszer/Win-CodexBar/commit/c65fd783bc3f1a4b50a8b2b45856ae60c9113872).

This is a static source and documentation review. It did not read personal credential stores, inspect browser profiles, launch provider CLIs, refresh tokens, or send authenticated provider requests. Source code and public documentation were inspected from the pinned checkouts and provider-owned documentation. A source implementation proves that a strategy exists in that revision; it does not prove that a private endpoint works for every account.

Evidence labels:

- **D**: provider or platform owner documents the interface.
- **C**: pinned CodexBar documentation or source implements the strategy.
- **W**: pinned Win-CodexBar source implements the Windows behavior.
- **I**: feasibility or stability judgment inferred from the documented implementation boundary.
- **U**: unknown because the available evidence does not establish the claim.

The AI Fuel terminology from `CONTEXT.md` is used throughout. A **Supported Provider** is a static catalog entry. A **Discovered Provider** has a provider-specific local credential source present. **Provider Discovery** is local, side-effect-free, and does not call endpoints, refresh tokens, or write credentials. A **Discovery Failure** is reported separately. A later authentication or quota failure must not hide a Discovered Provider.

## Decision summary

Broad monitoring coverage is feasible, but there is no single portable credential strategy. The 69-provider matrix contains at least eight materially different source families:

1. Explicit API or management keys plus reporting endpoints are the most portable. They still require the correct product, account, region, team, project, and reporting scope.
2. Provider OAuth files and owner RPCs are feasible when the provider documents the store or CLI. Token refresh commonly belongs to the owner and can write the source file.
3. OS keychains provide strong local ownership but require platform APIs, ACLs, unlocked sessions, and sometimes user prompts.
4. Browser cookies and localStorage can unlock web-only quota surfaces, but decryption, profile selection, origin scoping, and browser security make them conditional.
5. Local SQLite, XML, and JSONL can provide cached quota or history without network access, but schemas are private and values may be stale or incomplete.
6. Process and CLI probes can expose usage through an existing local agent, but executable presence is not credential presence and a probe can refresh or mutate owner state.
7. Provider endpoints range from documented reporting APIs to private dashboard APIs and HTML parsers. The latter are useful integrations but fragile contracts.
8. Local gateways such as Wayfinder expose routing metadata, not a provider subscription quota. Their health and model configuration must remain separate from provider account state.

For AI Fuel, “many agents available in CodexBar” must not become one boolean. CodexBar monitors 69 provider surfaces, while its agent-session monitor covers existing Codex, Claude Code, pi, and omp sessions and does not start arbitrary agent tasks. A model ID in quota or history is an observation, not a model catalog, and a model catalog is not proof of account entitlement or remaining quota.

## Source-class audit

`R` means local read, `W` means a write or mutation, `N` means a network request, and `P` means process or CLI execution. The proposed AI Fuel collection path is read-only even when the owner application or helper can write.

| Source | What it can establish | Read/write behavior | Platform permissions | Stability and honest result |
| --- | --- | --- | --- | --- |
| Provider files: JSON, TOML, XML, JSONL | A provider-specific store is present; sometimes account, project, expiry, plan, or cached usage | Discovery should be `R` only. Owner login and token refresh can be `W`; a stale file is not proof that refresh is safe | User-home access, file mode, path conventions, and provider profile selection differ by OS | **Moderate** when documented, otherwise private-schema fragile. A readable file can make presence known while live usage remains unavailable. [OpenAI auth](https://developers.openai.com/codex/auth), [pinned strategy overview](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/providers.md) |
| OS keychains and credential stores | A provider session or secret exists when the item is readable | `R` can prompt or fail. Login, repair, migration, and cache persistence can be `W` | macOS Keychain ACL and unlock state; Windows DPAPI or Credential Manager; Linux wallet/session availability. A denied read is not absence | **Moderate** for provider-owned documented stores, otherwise platform-specific. Report permission failure or unknown, not absent. [Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services), [CodexBar keychain policy](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/keychain-prompts.md) |
| Browser cookies | A web session for a particular provider domain and browser profile | Browser DB `R`; cookie-to-token bootstrap is `N` and may cache `W`; manual headers avoid DB access | macOS Safari may need Full Disk Access; Chromium import may need Safe Storage Keychain access; Windows DPAPI and Chromium App-Bound Encryption can block decryption; Linux wallet support is not universal | **Fragile to conditional**. Browser installed is not provider presence. ABE, locked DB, expired session, or denied Keychain should be unavailable for that source, not absent. [Win cookie guide](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/docs/COOKIES.md), [Chrome cookie security](https://security.googleblog.com/2024/07/improving-security-of-chrome-cookies-on.html) |
| Browser localStorage and LevelDB | Origin-specific bearer/session bundles, organization IDs, or workspace selectors | Extraction is `R`; importing or normalizing a bundle can be `W`; using it is `N` | Profile paths, browser encryption, origin, and account consistency matter. Windows ABE and browser profile locking still apply | **Fragile** because schemas are internal. A partial bundle must not be merged with a different profile. [CodexBar browser storage source](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/ChromiumLocalStorageDiscovery.swift) |
| Local SQLite databases | Provider auth, cached quota, local cost history, or browser cookies, depending on schema | Read with `R`; SQLite WAL/SHM and concurrent writers must be handled without checkpointing or rewriting. App-owned cache is separate `W` | File locks, WAL sidecars, path permissions, and provider version determine visibility. A live main-file copy can miss committed WAL data | **Moderate for bounded read-only adapters; fragile for private schemas**. Stale or partial local data must keep its source timestamp and units. [SQLite WAL](https://www.sqlite.org/wal.html), [Win cookie reader](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/browser/cookies.rs) |
| API or management keys | Explicit provider configuration and, after a request, the identity and scope accepted by the endpoint | Read config or inherited environment with `R`; saving a key is `W`; usage query is `N` | File permissions, environment inheritance, endpoint TLS, key scope, region, project, team, and account selection | **Strongest portable family**, but ordinary inference access does not prove reporting access. Never search arbitrary files for secrets. [Anthropic usage API](https://platform.claude.com/docs/en/manage-claude/usage-cost-api), [Warp API keys](https://docs.warp.dev/reference/cli/api-keys) |
| OAuth refresh and token endpoints | A provider account can authenticate and a token has a scope and expiry | Reading an access token is `R`; refresh is `N` and often writes the new access token and expiry to the owner file, so `W*` | OAuth client, redirect/device flow, scope, platform secure store, and concurrent owner writes matter | **Moderate when provider-owned and documented; private Code Assist flows are fragile**. Discovery must not refresh. [Google OAuth credential storage](https://github.com/google-gemini/gemini-cli/blob/9c1b0a610534d6f8120964cf2672c07807d8fc90/packages/core/src/code_assist/oauth-credential-storage.ts), [Win Gemini refresh/write path](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/providers/gemini/api.rs) |
| Process, PTY, CLI, or local RPC | A running service, installed executable, owner CLI status, usage response, or local session metadata | `P`; the child can perform `N` and `W` outside AI Fuel's control. Structured RPC is safer than PTY text | Executable path, version, PATH, working directory, process visibility, local ports, CSRF tokens, and OS prompts differ by platform | **Moderate for documented owner commands; fragile for ANSI/text and internal RPC**. Process presence is not credential presence. [Codex app-server](https://developers.openai.com/codex/app-server), [Kiro strategy](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/kiro.md) |
| Provider endpoints and dashboard APIs | A scoped account response for quota, balance, spend, or a model catalog | `N`; POST is not automatically a business mutation, but auth refresh and side effects must be classified | Network, TLS, rate limits, roles, account scope, regional hosts, Cloudflare/challenges, and endpoint availability | **Documented APIs: moderate to strong. Private APIs, gRPC-web, and HTML parsers: fragile.** An endpoint failure is unavailable for that metric, not proof of no account. [OpenAI usage API](https://platform.openai.com/docs/api-reference/usage), [GitHub Copilot metrics](https://docs.github.com/rest/copilot/copilot-usage-metrics) |

## Coverage audit against the 69-provider matrix

The following table classifies each canonical ID by its first useful source family and fallback boundary. It intentionally does not repeat issue #8's product and metric inventory. `C` means the pinned CodexBar strategy/source is the evidence; `D` means a provider-owned contract also exists; `W` means the pinned Windows implementation is evidence. Platform shorthand is explained after the table.

| ID | Credential and data-source path | Read/write and platform consequence | Stability and honest reporting |
| --- | --- | --- | --- |
| codex | OAuth file or secure store; app-server CLI; optional web cookies | `R+N`; owner CLI recovery can `W*`; M/L CLI, web M automatic or L manual, W unknown | `D/C`; match account and workspace; reset-credit inventory is separate from quota |
| openai | Admin or legacy API key in config/env | `R+N`; no AI Fuel write; M/L and endpoint-portable W | `D`; organization reporting is not Codex subscription quota |
| azureopenai | API key, endpoint, deployment in config/env | `R+N`; no write; M/L and endpoint-portable W | `C/I`; deployment probe is not spend or quota |
| claude | Admin key, OAuth file/keychain, CLI, or browser session | `R+N/P`; owner CLI/keychain repair can `W*`; M/L, browser M automatic or L manual, W conditional | `D/C`; keep Admin API spend separate from Claude subscription windows and match account |
| clinepass | API key in config/env | `R+N`; no write; M/L and endpoint-portable W | `C/I`; subscription scope and reset fields are provider-dependent |
| cursor | Browser session, stored session, or Cursor app SQLite auth | `RDB/Rcookie+N`; cache/import may `W`; browser M automatic, L manual, W DPAPI/ABE conditional | `C`; match browser session, account identity, and database profile; private API is fragile |
| opencode | Browser cookies and web dashboard | `Rcookie+N`; cache may `W`; M automatic, L manual, W conditional | `C`; web session scope is known only after account response |
| opencodego | Local SQLite history, API key, or web cookies | `RDB/Rkey/Rcookie+N`; app cache may `W`; M/L local, browser M automatic, L manual, W conditional | `C`; local cost history is not account-wide quota; workspace and key scopes stay separate |
| alibaba | OneConsole browser cookies, manual cookie, or coding-plan key | `Rcookie/Rkey+N`; cookie cache may `W`; M automatic, L manual/API, W conditional | `C`; region, plan type, and dashboard/API origin are identity fields |
| alibabatokenplan | Bailian CLI, then browser/manual cookies | `P/Rcookie+N`; CLI auth can `W*`; M/L CLI, browser M automatic, L manual, W CLI-dependent | `C`; CLI text and private console APIs are fragile |
| qwencloud | OneConsole browser cookies or manual cookie | `Rcookie+N`; cache may `W`; M automatic, L manual, W conditional | `C`; preserve region, tier, and dashboard/API cookie routing |
| factory | API key/config, browser/WorkOS session, local storage | `Rkey/Rcookie+N`; token/cache refresh may `W*`; API endpoint portable, browser conditional | `C`; API and web sessions may be different accounts |
| fireworks | API key and account slug | `Rkey+N`; no write; M/L and endpoint-portable W | `C/I`; spend reporting is not a remaining quota |
| gemini | Gemini CLI OAuth file and Code Assist endpoints | `Rfile+N`; refresh rewrites owner file `W*`; M/L, W file/endpoint conditional | `C/W`; model buckets are quota observations, not a public catalog; tier shutdown can make credentials unavailable |
| antigravity | Local language-server or `agy` process, local state, OAuth fallback | `R/P+N`; owner process/store may `W*`; M/L local, W unknown | `C`; process availability and quota availability are separate; private protocol fragile |
| copilot | Device-flow/config/env token and Copilot API | `Rkey+N`; device login/refresh may `W*`; M/L and endpoint-portable W | `D/C`; generic GitHub CLI auth is not Copilot credential discovery; org metrics differ from personal quota |
| devin | Chrome localStorage or manual bearer token | `Rcookie+N`; no automatic AI Fuel write; browser M automatic, L manual, W conditional | `C`; organization ID must be tied to the session; private endpoint fragile |
| zai | API token, region host, team/project selectors | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; global/CN and personal/team pools must not be combined |
| minimax | Coding-plan key, browser session, or local storage | `Rkey/Rcookie+N`; cache may `W`; browser M automatic, L manual, W conditional | `C`; region and group ID determine meaning; web history may be unavailable while quota is known |
| manus | `session_id` cookie, manual cookie, or environment token | `Rcookie+N`; no write; browser conditional on all platforms | `C`; session validity and credit response are distinct |
| kimi | Kimi Code key, `kimi-auth` cookie, or manual token | `Rkey/Rcookie+N`; no write; API portable, browser conditional | `C`; key and web session may expose different products and scopes |
| kilo | API token/config/env, CLI auth fallback | `Rkey/P+N`; CLI login/refresh can `W*`; API portable, CLI platform-dependent | `C`; API auth and CLI session are separate sources |
| kiro | `kiro-cli` `/usage` plus local/provider state | `P/R+N`; owner CLI may `W*`; M/L/W only where CLI works | `C`; ANSI/text and private enrichment are fragile; missing CLI is unavailable, not zero |
| vertexai | Google ADC/gcloud credentials and Cloud Monitoring | `Rfile/P+N`; gcloud refresh/cache can `W*`; M/L, W CLI/ADC-dependent | `D/C`; project and IAM scope are required; general Google login is not enough |
| augment | `auggie` CLI, then browser session | `P/Rcookie+N`; keepalive or CLI auth may `W*`; M/L CLI, browser conditional on all platforms | `C`; credits/account data can be known independently of model entitlement |
| jetbrains | IDE XML quota file | `Rfile`; IDE owns writes; M/L path conventions, W unknown | `C`; cached quota can be stale and is not a credential source by itself |
| moonshot | API key in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; balance is not a rate-limit window |
| amp | `amp` CLI, access token, or browser session | `P/Rkey/Rcookie+N`; CLI/session may `W*`; M/L CLI, browser conditional | `C`; private balance API and PTY fallback are fragile |
| t3chat | Browser cookies and tRPC customer endpoint | `Rcookie+N`; cache may `W`; browser conditional | `C`; account response is required before interpreting buckets |
| ollama | Local server/API key for catalog, browser session for Cloud quota | `R/P+N`; no owner write; local API portable, browser conditional | `D/C`; `/api/tags` is a catalog, Cloud quota is a different source, `/api/ps` is running state |
| synthetic | API key in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `D/C`; each returned window has its own unit and reset semantics |
| openrouter | API token in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; key spend/credits are not a provider subscription pool |
| elevenlabs | API key in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; character and voice metrics are product-specific |
| warp | API token in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `D/C`; management/request key scope matters |
| windsurf | Browser localStorage bundle or `state.vscdb` cache | `Rcookie/RDB+N`; AI Fuel should not write; browser conditional, local cache M/L, W unknown | `C/W`; cached plan may be stale and private schema can change |
| zed | macOS Keychain session and Zed endpoint | `Rkeychain+N`; no write; M only established, L/W unknown | `C`; Keychain denial is unavailable; hosted usage and BYOK usage stay separate |
| perplexity | Browser/manual cookie or session environment value | `Rcookie+N`; no write; browser conditional | `C`; session and credit response must be matched |
| mimo | Browser cookies and token-plan endpoint | `Rcookie+N`; cache may `W`; browser conditional | `C`; balance and plan allowance are different metrics |
| doubao | API key and Ark chat-completions probe | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; a successful inference probe is not a quota catalog |
| sakana | Manual Cookie header and billing HTML | `Rcookie+N`; no write; manual on M/L/W | `C/I`; HTML parsing is fragile and PAYG balance is best effort |
| abacus | Browser cookies and billing API | `Rcookie+N`; cache may `W`; browser conditional | `C`; compute credits need account scope |
| mistral | Browser cookies for console/Vibe billing | `Rcookie+N`; cache may `W`; browser conditional | `C`; API spend, credit balance, and Vibe allowance are separate pools |
| deepseek | API key or token account | `Rkey+N`; no write; M/L and endpoint-portable W | `D/C`; balance paid/granted fields are not subscription quota |
| deepinfra | API key or token account | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; prepaid balance, spend, and limit are distinct |
| codebuff | API token or provider credentials file | `Rkey/Rfile+N`; owner login may `W*`; M/L and endpoint-portable W | `C`; credit balance and weekly rate limit need separate fields |
| crof | API key in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; optional request quota must stay absent when not returned |
| venice | API key in config/env | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; DIEM/USD balance is not a generic quota window |
| commandcode | Browser session cookies and billing API | `Rcookie+N`; cache may `W`; browser conditional | `C`; monthly USD credits are a billing metric |
| qoder | Browser/manual cookies and usage API | `Rcookie+N`; cache may `W`; browser conditional | `C`; missing `nextResetAt` is unknown, not an invented reset |
| stepfun | Username/password login or manual Oasis token | `Rcookie/Rkey+N`; login may `W*`; browser/manual conditional | `C`; credential type and plan scope are sensitive and unstable |
| bedrock | AWS keys, profile, SSO, and AWS CLI resolution | `R/P+N`; CLI/SSO cache can `W*`; M/L, W AWS CLI-dependent | `D/C`; IAM and account/region scope are required; generic AWS profile is not Bedrock discovery |
| grok | `grok agent stdio`, browser session, local sessions | `P/Rcookie/Rfile+N`; CLI/session may `W*`; M/L CLI, browser conditional | `C`; billing RPC and local session signals are different evidence |
| groq | API key and Prometheus metrics | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; request/token/cache metrics are not a remaining quota |
| llmproxy | API key and user-configured HTTPS base URL | `Rkey+N`; no write; all platforms if URL/TLS works | `C/I`; proxy-reported pool is not upstream provider quota |
| litellm | API key and user-configured proxy URL | `Rkey+N`; no write; all platforms if URL/TLS works | `C`; key, user, and team budgets must remain distinct |
| deepgram | API key and usage API | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; audio, agent, token, and TTS units are not interchangeable |
| poe | API key and points API | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; point balance/history is not model entitlement |
| chutes | API key and quota endpoints | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; subscription, rolling, monthly, and PAYG fields stay separate |
| neuralwatt | API key and quota endpoint | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; kWh usage and prepaid money have different units |
| clawrouter | API key and optional base URL | `Rkey+N`; no write; all platforms if URL/TLS works | `C/I`; routed-provider rows are observations, not a model catalog |
| longcat | Browser or manual cookie header | `Rcookie+N`; cache may `W`; browser conditional, manual all platforms | `C`; token packs and pending packages need explicit expiry evidence |
| sub2api | Group API key and self-hosted HTTPS/loopback URL | `Rkey+N`; no write; all platforms if URL/TLS works | `C`; key quota, subscription group, wallet, and request totals have different scopes |
| wayfinder | Local gateway URL and read-only loopback endpoints | `Rlocal+N`; no credential write; all platforms where gateway runs | `D/C`; health, route metadata, and savings are known only when gateway responds; not provider quota |
| zenmux | Management API key | `Rkey+N`; no write; M/L and endpoint-portable W | `D/C`; management key is required; PAYG balance is optional enrichment |
| aiand | API key and request-log API | `Rkey+N`; no write; M/L and endpoint-portable W | `D/C`; organization spend can be partial because the documented retention/window is bounded |
| zoommate | Chrome cookies, cookie-to-token bootstrap, or manual capture | `Rcookie+N`; validated cookie cache `W*`, bearer memory-only; M automatic, L manual, W DPAPI/ABE conditional | `C`; host-scoped cookies and short-lived bearer must not be combined with another account |
| xai | Management key and team ID | `Rkey+N`; no write; M/L and endpoint-portable W | `C/I`; management spend/balance is not inference quota |
| notion | Browser cookies and workspace allowance API | `Rcookie+N`; cache may `W`; browser conditional | `C`; workspace identity is required; unsupported credit categories remain unknown |
| ibmbob | API key and team/profile APIs | `Rkey+N`; no write; M/L and endpoint-portable W | `C`; team Bobcoin budgets are not a generic token quota |

Platform shorthand:

- **M/L** means the pinned upstream project has macOS/Linux evidence. It does not establish native Windows application parity.
- **W endpoint-portable** means an API key and HTTPS request can be implemented on Windows in principle; it is not evidence that the provider grants the same account or endpoint on Windows.
- **Browser conditional** means automatic extraction depends on browser profile encryption and platform permissions. Manual cookie input is a separate source and should be labeled as such.
- **CLI-dependent** means the owner executable, its login state, and its platform support are required. A missing executable is unavailable for that source.
- **M only** means the inspected source uses a macOS-specific store, such as the Zed Keychain path. Linux or Windows support remains unknown or unsupported by that evidence.

## Account identity and shared quota pools

Never merge values simply because two sources use the same email, provider name, or model label. Preserve the strongest identity fields exposed by each source:

| Identity or pool boundary | Evidence and consequence |
| --- | --- |
| Codex account and workspace | OAuth claims, app-server identity, and optional web email matching are separate checks. Workspace balances and reset credits must not be attached to a different account or workspace. [Codex source notes](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/codex.md) |
| Claude account, organization, and Admin API scope | Admin reports describe an organization, while subscription OAuth/web sources describe a user account. Keep them as separate source identities. [Anthropic usage and cost API](https://platform.claude.com/docs/en/manage-claude/usage-cost-api) |
| Project, team, region, and deployment | Azure OpenAI, Vertex AI, Bedrock, z.ai, xAI, Fireworks, MiniMax, Alibaba, Qwen, and proxy providers attach meaning to endpoint, project, team, region, group, or deployment. A key without that selector is not enough to merge records. |
| Browser profile and origin | A cookie or localStorage token is scoped to a browser profile and web origin. A valid session for one account must not be combined with an API key or cookie from another profile. [Pinned browser source](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/BrowserLocalStorageAPI.swift) |
| Shared subscription groups | sub2api subscription counters can be shared across group keys while requests and cost remain key-scoped. LiteLLM, LLM Proxy, ClawRouter, and gateway rows similarly need key/user/team distinction. A shared pool is displayed once, not summed once per model or credential. [sub2api source notes](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/sub2api.md) |
| Model-specific or family windows | Gemini, Antigravity, Claude, Cursor, and Codex can return model or family labels. A family allowance may be shared. Missing fractions, absent reset times, and model observations remain unknown rather than zero or independent pools. [Gemini source notes](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/gemini.md) |

## Models and agent availability

The canonical inventory records model catalog support only where the pinned source establishes it. The practical model contract is:

- A documented model-list endpoint can establish a public or account-scoped catalog. OpenAI documents `models.list`, Google documents `models.list`, and Ollama documents `/api/tags`. These lists do not establish remaining subscription quota or that the local agent can invoke each model. [OpenAI models](https://platform.openai.com/docs/api-reference/models/list), [Gemini models](https://ai.google.dev/api/models), [Ollama tags](https://docs.ollama.com/api/tags)
- Ollama `/api/tags` means installed model artifacts; `/api/ps` means currently running models. Those are different local states and neither is Cloud quota. [Ollama running models](https://docs.ollama.com/api/ps)
- Gemini, Antigravity, Claude, Cursor, and OpenCode usage records may contain model IDs. These are observed usage or quota labels, not a complete callable-model catalog.
- Wayfinder's `/router/models` is configured gateway metadata. It does not prove that the upstream model key is configured, healthy, entitled, or currently reachable.
- Agent execution is **not established** for any of the 69 provider rows. CodexBar's agent-session monitor covers existing Codex, Claude Code, pi, and omp activity; it focuses existing sessions and does not submit prompts, run tools, or start arbitrary provider agents. [Agent Sessions design](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/agent-sessions-design.md)

Recommended capability names are separate: `catalog`, `configured_model`, `observed_model`, `quota_pool`, `agent_process_present`, `agent_session_present`, and `agent_execution_supported`. Do not collapse them into `model_available` or `agent_available`.

## Honest result states

Use status per source and per metric, not one provider-wide boolean:

| State | Meaning | Examples |
| --- | --- | --- |
| **Known** | A source returned a valid value with identifiable scope, units, and observation time | Explicit remaining `0`; a known balance; a catalog returned by a documented model endpoint; an identified running local model |
| **Unavailable** | A supported source exists but cannot currently be used, and the reason is established | Keychain denied, Chromium ABE, expired cookie, missing CLI executable, 401/403, reporting role missing, documented tier shutdown, provider endpoint timeout |
| **Unknown** | No authoritative value or capability can be established safely | Missing field, changed private schema, ambiguous account, no denominator, unverified platform behavior, model ID seen only in history |
| **Absent** | A side-effect-free discovery check found no provider-specific source | No configured key, no provider file, no matching manual cookie, no local gateway at the configured address |
| **Discovery Failure** | Discovery could not determine presence because the local check failed | Permission error, unreadable keychain, locked database, malformed source, or inaccessible profile. Exclude from the discovered set and report the failure separately per `CONTEXT.md`. |
| **Unsupported** | The selected adapter or platform has no evidence-backed implementation | The pinned upstream macOS Keychain adapter on Windows; an unimplemented provider-owned model catalog; arbitrary agent execution |

Specific rules:

- Explicit zero is known zero. Null, absent, invalid, or unparseable stays unknown. “Unlimited” requires explicit source evidence.
- A known balance with unavailable history is a partial result. Do not replace missing history with zero.
- A stale cache is known at its original observation time and must be labeled stale. It is not a current successful refresh.
- A discovered provider remains visible when authentication or quota retrieval fails. A discovery failure is different from a later collection failure.
- A successful model catalog does not prove subscription entitlement. A process or executable does not prove authentication. A quota response does not prove agent execution.
- Codex rate-limit reset credits are an inventory. A missing count is unknown, while an explicit count of zero is known zero. Consuming a credit is a write operation and is outside this audit.

## Platform and dependency consequences

| Platform | Feasible source classes | Main boundary |
| --- | --- | --- |
| macOS | Files, API keys, provider CLI/RPC, browser cookies, Keychain, local databases, local gateways | Safari/Chromium access can need Full Disk Access or Keychain ACL approval. A launched owner CLI controls its own credential behavior. |
| Linux | Files, API keys, manual cookies, provider CLIs, local databases, local gateways; some desktop wallet paths | Browser auto-import and system-wallet behavior are desktop/session-specific. The pinned upstream project has Linux CLI/desktop evidence, not universal provider parity. |
| Windows | Files, API keys, owner CLIs that support Windows, provider endpoints, browser cookies when decryption succeeds, local databases | Chromium ABE can block automatic cookie extraction even for a signed-in user. DPAPI scope and file ACLs matter. The pinned upstream CodexBar project has no native Windows target; Win-CodexBar is separate evidence. |
| WSL, containers, remote sessions | Explicit keys, manual cookies, reachable endpoints, selected files, and selected local gateways | Host browser/keychain/DPAPI access does not follow automatically into the guest. Do not infer presence from a mounted path. |

The Windows implementation demonstrates why Rust is not the same as zero dependency. Its `Cargo.toml` uses Tokio, Reqwest, Serde, Rusqlite, AES-GCM, Base64, Keyring, Windows APIs, and PTY support. Those dependencies cover asynchronous HTTP/TLS, JSON, SQLite, browser-cookie decryption, secure storage, native APIs, and CLI interaction. A Rust `std`-only build would need to omit or reimplement these capabilities, invoke external tools, or narrow provider scope. That is a distribution decision, not evidence that the data sources do not exist. [Win dependency manifest](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/Cargo.toml), [Win browser implementation](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/browser/cookies.rs), [Win secure-file implementation](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/secure_file.rs)

## Evidence anchors

These are the primary sources used for the audit. The canonical inventory remains the source of truth for the complete provider list.

- [Issue #8 canonical inventory artifact](https://github.com/ducquoc97/aifuel/blob/a43fd99/docs/research/2026-09-14-codexbar-provider-inventory.md)
- [Pinned CodexBar provider manifest](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/ProviderManifest.swift)
- [Pinned CodexBar provider enum](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/Providers.swift)
- [Pinned CodexBar provider strategies](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/providers.md)
- [Pinned CodexBar browser and Keychain access sources](https://github.com/steipete/CodexBar/tree/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore)
- [Pinned CodexBar Codex OAuth source](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/Codex/CodexOAuth/CodexOAuthCredentials.swift)
- [Pinned CodexBar Cursor local auth source](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/Cursor/CursorAppAuth.swift)
- [Pinned CodexBar agent-session design](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/agent-sessions-design.md)
- [Win-CodexBar browser cookie guide](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/docs/COOKIES.md)
- [Win-CodexBar browser cookie source](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/browser/cookies.rs)
- [Win-CodexBar Gemini source](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/providers/gemini/api.rs)
- [Win-CodexBar secure-file source](https://github.com/nesszer/Win-CodexBar/blob/c65fd783bc3f1a4b50a8b2b45856ae60c9113872/rust/src/secure_file.rs)
- [OpenAI Codex auth](https://developers.openai.com/codex/auth), [Codex app-server](https://developers.openai.com/codex/app-server), and [OpenAI models](https://platform.openai.com/docs/api-reference/models/list)
- [Anthropic usage and cost API](https://platform.claude.com/docs/en/manage-claude/usage-cost-api)
- [Google Gemini model API](https://ai.google.dev/api/models) and [Code Assist consumer-tier deprecation](https://developers.google.com/gemini-code-assist/docs/deprecations/code-assist-individuals)
- [GitHub Copilot usage metrics](https://docs.github.com/rest/copilot/copilot-usage-metrics)
- [Ollama model tags](https://docs.ollama.com/api/tags) and [running models](https://docs.ollama.com/api/ps)
- [AWS Cost Explorer API](https://docs.aws.amazon.com/cost-management/latest/userguide/ce-api.html), [Google ADC](https://cloud.google.com/docs/authentication/application-default-credentials), [Chrome cookie security](https://security.googleblog.com/2024/07/improving-security-of-chrome-cookies-on.html), [Microsoft DPAPI](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata), and [SQLite WAL](https://www.sqlite.org/wal.html)

No live provider validation was performed. The next implementation work should test one adapter per source class on each target OS, with explicit present, absent, denied, expired, stale, mismatched-account, partial-response, and known-zero fixtures. That validation must not broaden the canonical 69-provider scope silently.
