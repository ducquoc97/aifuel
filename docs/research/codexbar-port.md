# CodexBar portability research

Date: 2026-09-22

CodexBar snapshot: [`b99a916`](https://github.com/steipete/CodexBar/tree/b99a91694d7878202ee0b6d0bc725952bbd8c328), inspected from the upstream repository on 2026-09-22.

External sources below are first-party provider documentation or upstream source code. This note follows the vocabulary in [`CONTEXT.md`](../../CONTEXT.md): a model advertised by a provider is not automatically an account entitlement or execution availability.

## Executive result

CodexBar is a usage and quota monitor, not a cross-provider agent launcher. Its provider architecture is descriptor-driven: a provider owns metadata and an ordered set of fetch strategies, while the app and CLI share the same usage pipeline. The upstream authoring guide describes CLI, OAuth, API, local-probe, web-cookie, and web-dashboard strategies, but does not define a universal model-selection or reasoning-effort interface. See the [provider authoring guide](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/docs/provider.md) and the [CLI command registry](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCLI/CLIEntry.swift).

The portable part is the boundary and the observation model:

- define provider-owned descriptors and ordered fetch strategies;
- preserve source, fallback, timeout, identity, and diagnostic information;
- retain raw model IDs and model-scoped quota windows as observations;
- normalize only for display, and keep provider-specific mappings local;
- keep advertised model, account entitlement, and execution availability separate.

The non-portable part is the macOS and provider-specific implementation: Keychain, WebKit, browser-cookie stores, PTY capture, local language servers, private quota endpoints, and provider payloads. Those should not become a shared Rust abstraction or a direct dependency.

## What CodexBar discovers

CodexBar's CLI exposes usage, cost, sessions, dashboard, configuration, diagnostics, and quota-gating commands. It does not expose a general `models` or `run` command. Its model data is therefore observed while collecting quota, local history, or cost information, rather than discovered as a universal runnable-model catalog.

Examples from the inspected source:

- Codex usage responses preserve `additional_rate_limits` as model-specific usage windows, including Spark limits. Local cost scanning also records model names and reasoning-token counts.
- Claude usage responses preserve model-scoped weekly limits and local history contains model breakdowns. This is historical usage attribution, not a complete model picker.
- Copilot's monitor fetches quota and credit information from `copilot_internal/user`; its normalized usage snapshot has no model catalog.
- Gemini's quota parser keeps one row per returned `modelId`, then the UI groups rows into Pro, Flash, and Flash Lite tiers.
- Antigravity parses model names and quota data from local or remote payloads, then groups the display into Gemini and Claude/GPT families. The family grouping is a display rule, not a provider model-selection rule.

Relevant upstream sources are [Codex provider notes](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/docs/codex.md), [Claude usage parsing](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift), [Copilot usage fetching](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Copilot/CopilotUsageFetcher.swift), [Gemini quota parsing](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Gemini/GeminiStatusProbe.swift), and [Antigravity quota fetching](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Antigravity/AntigravityRemoteUsageFetcher.swift).

CodexBar does record reasoning token usage for local cost history, especially Codex, Claude, and Antigravity. That is a measurement of what happened, not a setting that controls the next run.

## Provider findings

### Codex

CodexBar's monitor has OAuth, web-dashboard, and `codex app-server` paths, plus local cost scanning. Its documented RPC usage path calls `account/read` and `account/rateLimits/read`; `additional_rate_limits` is retained as model-specific quota data. This is useful for AI Fuel monitoring, but it is not a model catalog.

The official Codex CLI supports model selection with `--model` and `/model`. The official configuration supports a default `model`, `model_catalog_json`, and `model_reasoning_effort`; the documented effort values are model-dependent and include `minimal`, `low`, `medium`, `high`, and `xhigh`. Arbitrary one-off configuration values can be passed with `--config` or `-c`. Sources: [Codex CLI](https://learn.chatgpt.com/docs/codex/cli), [advanced configuration](https://learn.chatgpt.com/docs/codex/config-file/config-advanced), and [sample configuration](https://learn.chatgpt.com/docs/codex/config-file/config-sample).

Portable to AI Fuel:

- keep Codex quota windows separate from the model catalog;
- read a user-provided Codex model catalog only as advertised-model evidence;
- map a future effort request to Codex's provider-owned `model_reasoning_effort` configuration;
- parse Codex JSONL or App Server events when available to populate `effective_model`.

Do not infer that a model in `additional_rate_limits` is selectable, or that a model in a catalog is entitled to the signed-in account.

### Claude Code

CodexBar reads Claude usage through OAuth, CLI PTY, web, or Admin API strategies. The OAuth and web payloads can expose model-scoped weekly limits, while Admin API and local history can expose model breakdowns. Those are account usage observations. See the [Claude strategy table](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/docs/providers.md) and [model-scoped window parser](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Claude/ClaudeWeb/ClaudeWebExtraRateWindowParser.swift).

The official Claude Code CLI supports `--model` with aliases or full model names and `--effort` with `low`, `medium`, `high`, `xhigh`, `max`, and `ultracode`, subject to the selected model. `ultracode` is a Claude Code workflow setting that requests `xhigh`, not a generic model effort value. `CLAUDE_CODE_EFFORT_LEVEL` has higher precedence than `--effort`, `/effort`, and saved effort settings. Claude Code can also populate a model picker from an LLM gateway `/v1/models` endpoint when gateway discovery is enabled. Sources: [CLI reference](https://code.claude.com/docs/en/cli-reference), [model configuration](https://code.claude.com/docs/en/model-config), and [environment variables](https://code.claude.com/docs/en/env-vars).

Portable to AI Fuel:

- pass `--model` and `--effort` directly in the Claude adapter;
- use machine-readable output and the provider-reported model usage to fill `effective_model`;
- treat gateway-discovered names as advertised models and validate them only when Claude Code accepts the run;
- keep `ultracode` as a provider-specific option rather than putting it in a universal effort enum.

### GitHub Copilot CLI

CodexBar's Copilot adapter is quota-only. It calls the Copilot internal user endpoint and normalizes premium/chat windows and credit information. It does not discover Copilot models or reasoning effort. See [CodexBar's provider table](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/docs/providers.md) and [Copilot usage code](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Copilot/CopilotUsageFetcher.swift).

The official Copilot CLI supports `--model`, `COPILOT_MODEL`, `/model`, `--effort`, and the alias `--reasoning-effort`. Its documented effort values are `low`, `medium`, `high`, `xhigh`, and `max`; the available values are model-dependent. Copilot persists `model` and `effortLevel` in `~/.copilot/settings.json`, and its documentation tells users to use `/model` to see all models available to the current account. Sources: [CLI command reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference), [programmatic reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-programmatic-reference), [configuration directory reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference), and [supported AI models](https://docs.github.com/en/copilot/reference/ai-models/supported-models).

Portable to AI Fuel:

- pass `--model` and `--effort` in the Copilot adapter;
- preserve `auto` as an explicit provider model mode, not as an AI Fuel universal model ID;
- parse the model shown by non-silent machine-readable output when the CLI reports it;
- never use the static supported-model table as proof of account entitlement.

### Gemini CLI

CodexBar calls Gemini's OAuth-backed quota API and keeps the returned `modelId` rows. It reduces those rows into Pro, Flash, and Flash Lite display lanes by choosing the tightest remaining quota in each tier. The [Gemini provider notes](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/docs/gemini.md) and [quota parser](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Gemini/GeminiStatusProbe.swift) show that this is quota normalization, not model selection.

The official Gemini CLI exposes `--model` and `/model`, with `auto`, `pro`, `flash`, and `flash-lite` aliases plus concrete model IDs. Its configuration has `model.name`, model definitions, model ID resolution rules, aliases, and fallback chains. Reasoning is not exposed as a generic `--effort` flag in the CLI reference. Advanced model configuration uses provider-native `thinkingConfig`, including `thinkingBudget` or `thinkingLevel`, and warns that incompatible combinations can fail at runtime. Sources: [Gemini model selection](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/model.md), [CLI reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md), [configuration reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md), and [advanced generation settings](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/generation-settings.md).

Portable to AI Fuel:

- pass `--model` directly;
- expose Gemini's native thinking configuration only as a Gemini-specific setting;
- preserve alias-to-concrete-model resolution as an observation when the CLI reports it;
- do not translate a universal `high` effort request into a guessed thinking budget.

### Antigravity CLI

CodexBar supports local app/IDE probes, the `agy` CLI's local HTTPS source, and Google OAuth. It parses available model payloads and quota buckets, and its display groups usage into Gemini and Claude/GPT families. The [Antigravity provider notes](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/docs/antigravity.md), [remote fetcher](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Antigravity/AntigravityRemoteUsageFetcher.swift), and [family visibility filter](https://github.com/steipete/CodexBar/blob/b99a91694d7878202ee0b6d0bc725952bbd8c328/Sources/CodexBarCore/Providers/Antigravity/AntigravityQuotaFamilyVisibility.swift) show the distinction between raw model data and display families.

The official Antigravity CLI has the cleanest discovery contract for this project: `agy models` lists account-visible model slugs, `--model` pins a slug, and `--effort` accepts `low`, `medium`, or `high`. The headless docs say an unknown pinned model fails instead of silently falling back. The result formats also expose model and token-usage metadata. Sources: [Antigravity headless mode](https://antigravity.google/docs/cli/headless/), [Antigravity models](https://antigravity.google/docs/models), and [CLI reference](https://antigravity.google/docs/cli/reference/).

Portable to AI Fuel:

- implement `agy models` parsing as an advertised-model discovery adapter;
- pass `--model` and `--effort` directly;
- parse JSON or stream JSON for the effective model and thinking-token usage;
- preserve the provider's fail-loud unknown-model behavior.

This is the strongest candidate for the first live model-discovery adapter. The model list remains account- and plan-dependent, so it still does not prove entitlement beyond the CLI's own selection check.

## Comparison with the current Rust repository

AI Fuel already has the right high-level separation:

- [`CONTEXT.md`](../../CONTEXT.md) distinguishes Advertised Model, Account Entitlement, Agent Integration, and Execution Availability.
- [`ProviderRegistry`](../../crates/aifuel-providers/src/registry.rs) separates Catalog Provider discovery from monitoring adapters.
- [`agent_run_registry`](../../crates/aifuel-providers/src/agent_run_registry.rs) separately registers five execution adapters: Codex, Claude, Copilot, Gemini, and Antigravity.
- [`DiscoveryContext`](../../crates/aifuel-providers/src/discovery.rs) performs side-effect-free source-presence discovery, matching the repository glossary.
- [`RunRequest`](../../crates/aifuel-core/src/execution.rs) already carries an optional model, but no reasoning-effort field.
- The provider adapters forward `--model`, but currently do not forward effort and do not populate `RunResult.effective_model`; see the [Codex adapter](../../crates/aifuel-providers/src/codex/agent_run.rs), [Claude adapter](../../crates/aifuel-providers/src/claude/agent_run.rs), [Copilot adapter](../../crates/aifuel-providers/src/copilot/agent_run.rs), [Gemini adapter](../../crates/aifuel-providers/src/gemini/agent_run.rs), and [Antigravity adapter](../../crates/aifuel-providers/src/antigravity/agent_run.rs).

The missing concept is not another provider-specific quota parser. It is a separate, optional model capability for an Agent Integration:

```text
Catalog Provider
  -> Provider Discovery (credential/source presence)
  -> Monitoring (quota and usage observations)
  -> Model Discovery (advertised models and provider options)
  -> Agent Run (requested model/effort and effective model)
```

Model Discovery must not run as part of read-only quota collection unless a provider contract explicitly makes that safe. It should report source, timestamp, model ID, display name, advertised capabilities, and uncertainty. It should not claim account entitlement merely because a static catalog or quota payload contains an ID.

## Recommended port boundary

Do not port CodexBar as a library and do not copy its private provider requests wholesale. Port the following design ideas into Rust:

1. Keep provider descriptors and ordered strategies provider-owned.
2. Add an optional model-discovery capability beside monitoring and Agent Run, not inside `Provider Discovery`.
3. Represent reasoning effort as a provider-validated request. A common label such as `high` is only a user-facing convenience; the adapter must map or reject it for the selected provider and model.
4. Add provider-owned model capability metadata, including whether effort is supported, the accepted values, and whether a model ID is an alias, a concrete ID, or an observed runtime ID.
5. Preserve the effective model and effective effort from machine-readable provider output whenever the provider reports them. Never silently replace an invalid requested model or effort with a different one.
6. Keep the MCP status server read-only. Expose model observations and quota evidence through MCP only; model selection remains part of an explicit Agent Run request.
7. Verify with fake executables and a live WSL matrix. The live tests should cover `agy models`, Codex config/catalog behavior, Claude gateway or built-in model behavior, Copilot `/model` or CLI model behavior, and Gemini model configuration. Test the exact translation prompt separately from model discovery.

Suggested first implementation order is Antigravity, Claude, Copilot, Codex, then Gemini. Antigravity has a documented non-interactive list and effort flags. Claude and Copilot have direct effort flags. Codex has model and config-based effort. Gemini requires provider-native thinking configuration rather than a shared effort flag.

## Decision

CodexBar is a strong reference for provider monitoring boundaries, source fallback, conservative quota normalization, and model-observation storage. It is not a drop-in solution for AI Fuel's command and MCP execution goal. AI Fuel should port the architecture and the provider-specific execution contracts, while keeping monitoring, model discovery, account entitlement, and Agent Run as separate capabilities.

