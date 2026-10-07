# CodexBar provider and capability inventory

Research date: 2026-09-14

Pinned upstream snapshot: steipete/CodexBar commit caad1ca38c237fc77426ad06cf56a2daa1dbbcb9 on main.
The commit was authored 2026-09-13 and fetched for this inventory on 2026-09-14.

This is a static source and documentation review. It did not use provider credentials, live accounts, browser sessions,
or provider APIs. Detailed credential feasibility is intentionally not decided here. The separate credential feasibility
audit owns that work.

## Classification rules

CodexBar calls every built-in entry a provider. This matrix adds an analytical role:

- agent: a coding agent, assistant, IDE, or agent-oriented product surface.
- model: an inference API, model host, model plan, or model-serving platform.
- provider: a hosted AI product whose CodexBar surface is a consumer or general product quota, where the source does
  not justify claiming a specific agent runtime or model catalog.
- auxiliary: a proxy, router, gateway, observability, or billing-only surface.

Model catalog is yes only when the source explicitly fetches a catalog or configured model set. Model IDs that appear
in quota, cost, or session records are not a catalog. unknown means the pinned source does not establish the
capability. Agent execution is no for every row: CodexBar monitors or fetches usage, but does not submit an arbitrary
prompt, run tools, or start a provider agent task.

Platform shorthand: M means the macOS app/CLI, L means the Linux CLI, and M auto / L manual means automatic browser
credential import is macOS-only while a configured manual cookie or token path can work on Linux. The upstream project
has no native Windows target. A downstream Windows wrapper is not evidence of upstream Windows support.

## Canonical 69-provider matrix

The source location column uses the prefix Sources/CodexBarCore/Providers/. Each directory contains the provider
descriptor and its fetch/parser/probe files. Shared app-only UI files are not repeated.

| ID | Role | Product/account type | Monitoring metrics and reset | Source location | Model catalog | Agent execution | Platform |
| --- | --- | --- | --- | --- | --- | --- | --- |
| codex | agent | ChatGPT/Codex subscription; multiple visible accounts | 5-hour/session and weekly windows, reset times, credits when exposed | Codex/ | session quota and local cost; no catalog | no | M/L CLI; web extras M |
| openai | model | OpenAI developer organization | organization spend/usage or legacy balance; no subscription quota | OpenAI/ | unknown | no | M/L |
| azureopenai | model | Azure OpenAI deployment | deployment probe only; no spend/quota history | AzureOpenAI/ | unknown | no | M/L |
| claude | agent | Claude Code subscription or Anthropic organization | 5-hour/session, weekly, model-scoped limits, extra credits when exposed | Claude/ | unknown; model-scoped quota may be shown | no | M/L CLI; web auto M, manual L |
| clinepass | agent | ClinePass subscription | 5-hour, weekly, and monthly subscription limits | ClinePass/ | unknown | no | M/L |
| cursor | agent | Cursor account | plan usage and billing-cycle reset; optional weekly Grok Bot window | Cursor/ | unknown; usage events may name models | no | M/L; browser auto M |
| opencode | agent | OpenCode account | dashboard usage; stable reset semantics unknown | OpenCode/ | unknown | no | M auto / L manual |
| opencodego | agent | OpenCode Go account or workspace | rolling 5-hour, weekly, and monthly windows; local history can be API-enriched | OpenCodeGo/ | unknown; local usage may name models | no | M/L; browser auto M |
| alibaba | model | Model Studio Coding Plan | coding-plan quota; reset semantics source-dependent | Alibaba/ | unknown | no | M auto / L manual or API |
| alibabatokenplan | model | Bailian Token Plan | 5-hour and weekly subscription windows | Alibaba/ | unknown | no | M/L CLI; browser auto M |
| qwencloud | model | Qwen Cloud Token Plan | 5-hour and weekly windows | QwenCloud/ | unknown | no | M auto / L manual |
| factory | agent | Droid/Factory account or organization | 5-hour, weekly, monthly token limits or legacy plan limits | Factory/ | unknown | no | M/L API; browser auto M |
| fireworks | model | Fireworks API account | rated spend for last 30 days; no quota/balance endpoint | Fireworks/ | unknown | no | M/L |
| gemini | agent | Gemini CLI Code Assist account | per-model quota with resetTime | Gemini/ | partial: model IDs in quota buckets, not a catalog | no | M/L CLI |
| antigravity | agent | Antigravity account or IDE | per-model/family 5-hour and weekly windows where source is rich; OAuth may only prove availability | Antigravity/ | partial: available models and quota model IDs | no | M/L CLI/local; no upstream Windows |
| copilot | agent | GitHub Copilot seat | premium/chat quota and optional budget extras; API does not supply reset dates | Copilot/ | unknown | no | M/L |
| devin | agent | Devin organization | daily and weekly quotas with reset timestamps | Devin/ | unknown | no | M auto / L manual |
| zai | model | z.ai personal or team account | quota, 5-hour, and hourly windows when returned | Zai/ | unknown | no | M/L |
| minimax | model | MiniMax Coding Plan | coding-plan usage and optional history; stable reset semantics unknown | MiniMax/ | unknown | no | M auto / L manual or API |
| manus | agent | Manus account | credits, monthly allowance, and daily refresh data when returned | Manus/ | unknown | no | M auto / L manual |
| kimi | model | Kimi Code account | 5-hour and weekly quota | Kimi/ | unknown | no | M/L; browser auto M |
| kilo | agent | Kilo Pass or organization | usage API windows; stable period unknown | Kilo/ | unknown | no | M/L |
| kiro | agent | Kiro/AWS Builder ID plan | monthly plan credits, bonus credits, and reset date from CLI/API | Kiro/ | unknown | no | M/L when kiro-cli exists |
| vertexai | model | Google Cloud project | Cloud Monitoring quota usage; reset interval is Google quota-specific | VertexAI/ | unknown | no | M/L |
| augment | agent | Augment account | credits and subscription data where available | Augment/ | unknown | no | M/L CLI; browser auto M |
| jetbrains | agent | JetBrains AI IDE account | monthly credits and next refill from IDE XML | JetBrains/ | unknown | no | M/L |
| moonshot | model | Moonshot API account | balance only; no quota/reset window | Moonshot/ | unknown | no | M/L |
| amp | agent | Amp account or workspace | Amp Free and credit balances; stable reset semantics unknown | Amp/ | unknown | no | M/L CLI/API; browser auto M |
| t3chat | provider | T3 Chat customer account | 4-hour Base bucket and monthly Overage bucket | T3Chat/ | unknown | no | M auto / L manual |
| ollama | model | Ollama Cloud account | monthly included credits or legacy session/hourly and weekly windows; reset timestamps | Ollama/ | yes: /api/tags catalog in API-key path | no | M/L API; browser auto M |
| synthetic | model | Synthetic account | rolling 5-hour, weekly token, and search-hourly lanes | Synthetic/ | unknown | no | M/L |
| openrouter | model | OpenRouter API key | credits and daily/weekly/monthly key spend; no subscription quota | OpenRouter/ | unknown | no | M/L |
| elevenlabs | model | ElevenLabs subscription | subscription usage; stable reset semantics unknown | ElevenLabs/ | unknown | no | M/L |
| warp | agent | Warp account | GraphQL request limits; reset semantics source-dependent | Warp/ | unknown | no | M/L |
| windsurf | agent | Windsurf/Devin account | daily/weekly quota from web or local cache; reset when returned | Windsurf/ | unknown | no | M/L local; browser auto M |
| zed | agent | Zed account | plan and Edit Predictions quota; billing-cycle data | Zed/ | unknown | no | M only for documented Keychain path |
| perplexity | provider | Perplexity account | recurring, bonus, and purchased credits plus renewal date when present | Perplexity/ | unknown | no | M auto / L manual |
| mimo | model | Xiaomi MiMo account | balance/token-plan endpoints; reset semantics source-dependent | MiMo/ | unknown | no | M auto / L manual |
| doubao | model | Volcengine Ark API account | chat-completions probe and rate-limit headers when present | Doubao/ | unknown; fixed probes are not a catalog | no | M/L |
| sakana | model | Sakana account | 5-hour and weekly quota plus best-effort PAYG balance | Sakana/ | unknown | no | M/L manual |
| abacus | provider | Abacus organization | compute points and billing; monthly credit gauge | Abacus/ | unknown | no | M auto / L manual |
| mistral | model | Mistral API or Vibe account | billing usage, credit balance, and Vibe monthly usage; calendar-month reset | Mistral/ | unknown; usage may name models | no | M auto / L manual |
| deepseek | model | DeepSeek API account | balance with paid/granted breakdown; no quota/reset window | DeepSeek/ | unknown | no | M/L |
| deepinfra | model | DeepInfra account | prepaid balance, billing-cycle spend, spending limit, suspension state | DeepInfra/ | unknown | no | M/L |
| codebuff | agent | Codebuff account | credit balance, weekly rate limit, reset timing | Codebuff/ | unknown | no | M/L |
| crof | model | Crof account | credit balance and optional request quota | Crof/ | unknown | no | M/L |
| venice | model | Venice API account | DIEM/USD balance and DIEM epoch allocation; not a quota window | Venice/ | unknown | no | M/L |
| commandcode | agent | Command Code account | 5-hour, weekly, monthly USD credits, and billing-cycle usage | CommandCode/ | unknown | no | M auto / L manual |
| qoder | agent | Qoder account | big-model credits; nextResetAt when returned | Qoder/ | unknown | no | M auto / L manual |
| stepfun | model | StepFun Step Plan | 5-hour and weekly rate limits | StepFun/ | unknown | no | M/L manual |
| bedrock | model | AWS account | month-to-date spend/budget and optional rolling 14-day Claude activity; no Bedrock quota | Bedrock/ | unknown | no | M/L |
| grok | agent | Grok/SuperGrok account | consumer subscription billing quota; local session fallback | Grok/ | unknown; sessions may expose a model ID | no | M/L CLI; browser auto M |
| groq | model | GroqCloud organization | request/token/cache-hit Prometheus metrics; no quota/reset | Groq/ | unknown | no | M/L |
| llmproxy | auxiliary | proxy deployment and API key | lowest quota group plus requests, tokens, and approximate cost | LLMProxy/ | unknown; provider breakdown only | no | M/L |
| litellm | auxiliary | LiteLLM key, user, or team | key/user/team budget usage when configured | LiteLLM/ | unknown; budget scope only | no | M/L |
| deepgram | model | Deepgram project | audio, agent, token, TTS, and request usage | Deepgram/ | unknown | no | M/L |
| poe | provider | Poe API account | current point balance and rolling 7/30-day history | Poe/ | unknown | no | M/L |
| chutes | model | Chutes account | subscription, rolling/monthly, and PAYG quota APIs | Chutes/ | unknown | no | M/L |
| neuralwatt | model | Neuralwatt subscription/key | subscription kWh quota, prepaid USD balance, optional key allowance | NeuralWatt/ | unknown | no | M/L |
| clawrouter | auxiliary | ClawRouter policy/key | monthly budget, spend, requests, tokens, and routed-provider breakdown | ClawRouter/ | unknown; routed rows are not a catalog | no | M/L |
| longcat | provider | LongCat account | token-pack quota and pending fuel packages; nearest expiry as reset | LongCat/ | unknown | no | M auto / L manual |
| sub2api | auxiliary | sub2api group key | key quota, 5-hour/day/week limits, subscription limits, wallet, request/token/cost totals | Sub2API/ | unknown | no | M/L |
| wayfinder | auxiliary | local router gateway | gateway health, route split, savings, decision latency; no quota | Wayfinder/ | partial: configured model metadata, not a catalog | no | M/L local gateway |
| zenmux | auxiliary | ZenMux subscription/key | rolling 5-hour and 7-day quota plus PAYG balance | ZenMux/ | unknown | no | M/L |
| aiand | auxiliary | ai& organization | last-30-day organization spend; prepaid credits are not shown as quota | AiAnd/ | unknown | no | M/L |
| zoommate | agent | ZoomMate account | credits against budget, billing-cycle reset, today/30-day history | ZoomMate/ | unknown | no | M auto / L manual |
| xai | model | xAI developer team | prepaid balance and 30-day daily spend; money is not quota | XAI/ | unknown | no | M/L |
| notion | provider | Notion workspace | rolling 6-hour and monthly workspace allowance; Custom Agents/Workers credits are not read | Notion/ | unknown | no | M auto / L manual |
| ibmbob | agent | IBM Bob team subscription | monthly Bobcoin usage, team budgets, subscription refresh date | IBMBob/ | unknown | no | M/L |

## Agent-session coverage

The agent-session model is smaller than the provider catalog:

| Surface | Monitoring support | Execution support | Platform |
| --- | --- | --- | --- |
| Codex | live local/remote session discovery; existing-window focus | no prompt submission or task start | M/L listing; macOS focus |
| Claude Code | live local/remote session discovery; existing-window focus | no prompt submission or task start | M/L listing; macOS focus |
| pi | plain Pi-family process and bounded session metadata | no Pi launch or task execution | M/L listing |
| omp | OMP dialect through the Pi-family scanner | no OMP launch or task execution | M/L listing |

The CLI legacy JSON session protocol contains Codex and Claude. JSON v2 adds Pi-family rows. The focus command activates
an existing macOS window; it is not agent execution. Remote discovery uses SSH. The scanner intentionally avoids
prompt/tool transcript bodies and full target-process environments.

## Other upstream integration surfaces

- Local JavaScript/TypeScript provider plugins can fetch usage through narrow host APIs. They cannot use Node, browser
  globals, subprocesses, local files, databases, OAuth, WebViews, or arbitrary local I/O. They support the macOS app
  and macOS/Linux CLIs, not widgets or built-in-provider-only surfaces.
- The models.dev integration supplies additive pricing metadata for local cost estimates. It is not a live model
  catalog or an execution API.
- Local cost history may display model names for sources that expose token logs. That is historical observation, not a
  supported list of callable models.
- The CLI and local serve/dashboard endpoints are presentation and transport surfaces. The Linux Qt desktop owns
  polling, settings, notifications, and a same-user local socket; the Swift CLI owns provider fetching and
  authentication. Neither is an additional provider or agent.

## AI Fuel coverage reconciliation

AI Fuel currently declares five provider classes: Claude, Codex, Copilot, Gemini, and Antigravity. Each has a matching
upstream ID in this matrix:

| AI Fuel integration | Upstream ID | Inventory result |
| --- | --- | --- |
| ClaudeProvider | claude | present |
| CodexProvider | codex | present |
| CopilotProvider | copilot | present |
| GeminiProvider | gemini | present |
| AntigravityProvider | antigravity | present |

There are no AI Fuel-only provider IDs missing from the pinned CodexBar manifest as of this research date. This is a
coverage reconciliation, not a launch-target or acceptance decision. Issue 18 owns the human parity acceptance matrix.
No smaller baseline is selected here.

## Evidence and boundaries

The canonical provider count and IDs come from the pinned generated manifest and UsageProvider enum. The monitoring
strategy and per-provider metric descriptions come from the pinned providers overview and provider notes. The model
catalog, agent-session, plugin, and platform conclusions are included only where those sources state them. Otherwise
the matrix says unknown.

This ticket does not decide credential feasibility, Windows parity acceptance, implementation order, or agent execution
scope. It records monitoring coverage and evidence pointers only.

## Sources

All upstream URLs are pinned to caad1ca38c237fc77426ad06cf56a2daa1dbbcb9 and were accessed on 2026-09-14.

- Snapshot commit: https://github.com/steipete/CodexBar/commit/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9
- Generated provider manifest: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/ProviderManifest.swift
- UsageProvider enum: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/Providers.swift
- Provider strategy overview: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/providers.md
- CLI source modes and output contract: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/cli.md
- Agent Sessions design: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/agent-sessions-design.md
- Local plugin contract: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/plugins.md
- README platform overview: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/README.md
- Linux integration and platform comparison: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Integrations/Linux/README.md and https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Integrations/Linux/MAC_COMPARISON.md
- Model pricing boundary: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/model-pricing.md
- Antigravity model/quota evidence: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/antigravity.md
- Gemini quota evidence: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/gemini.md
- Ollama catalog evidence: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/ollama.md
- Wayfinder gateway evidence: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/wayfinder.md
- Copilot reset limitation: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/copilot.md
- Fireworks spend-only evidence: https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/fireworks.md
- AI Fuel provider catalog: ../../src/aifuel/providers/__init__.py, inspected 2026-09-14.
- AI Fuel product contract: ../../README.md, inspected 2026-09-14.

