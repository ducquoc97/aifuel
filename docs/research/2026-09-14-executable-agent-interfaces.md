# Executable agent interfaces across the pinned provider inventory

Research date: 2026-09-14. Issue: [#19](https://github.com/ducquoc97/aifuel/issues/19).

This primary-source review did not launch inference, read credential contents, call provider APIs with personal credentials, or inspect live account state. Model APIs are separate from agent runtimes.

## Complete 69-provider matrix

The source column is the pinned CodexBar provider directory. Contract families are expanded below.

| ID | Class | Surface | Agent? | Source |
| --- | --- | --- | --- | --- |
| codex | agent-cli/app-server | `codex exec`, JSON-RPC | yes | `Codex/` |
| openai | agent-sdk/model-api | Agents SDK, Responses | SDK yes; API no | `OpenAI/` |
| azureopenai | model-api | Azure Responses/chat | no | `AzureOpenAI/` |
| claude | agent-cli/SDK/API | `claude -p`, Agent SDK | yes | `Claude/` |
| clinepass | agent-cli | Cline CLI | yes through Cline | `ClinePass/` |
| cursor | agent-cli/MCP | `cursor-agent -p` | yes | `Cursor/` |
| opencode | agent-cli/RPC | `opencode run`, server | yes | `OpenCode/` |
| opencodego | agent-cli account | OpenCode Go account | yes through OpenCode | `OpenCodeGo/` |
| alibaba | agent-cli/model-api | Qwen Code, Model Studio | yes through Qwen Code | `Alibaba/` |
| alibabatokenplan | monitoring-only/unknown | `bl usage token-plan` | no proven contract | `Alibaba/` |
| qwencloud | agent-cli/model-api | Qwen Code, Qwen API | yes through Qwen Code | `QwenCloud/` |
| factory | agent-cli/JSON-RPC | `droid exec` | yes | `Factory/` |
| fireworks | model-api | chat completions | no | `Fireworks/` |
| gemini | agent-cli/ACP/API | Gemini CLI | yes through CLI | `Gemini/` |
| antigravity | agent-cli/MCP | `agy -p`, JSON | yes | `Antigravity/` |
| copilot | agent-cli/ACP/MCP | `copilot -p` | yes | `Copilot/` |
| devin | agent-api/CLI | sessions, Devin CLI | yes | `Devin/` |
| zai | model-api | GLM API | no | `Zai/` |
| minimax | model-api | OpenAI/Anthropic API | no | `MiniMax/` |
| manus | agent-api | Manus tasks and agents | yes remote | `Manus/` |
| kimi | agent-cli/API/ACP | Kimi Code | yes through CLI | `Kimi/` |
| kilo | agent-cli/RPC/MCP | `kilo run` | yes | `Kilo/` |
| kiro | agent-cli/MCP | `kiro-cli chat` | yes | `Kiro/` |
| vertexai | model-api/SDK | Vertex model APIs | no | `VertexAI/` |
| augment | agent-cli | `auggie --print` | yes | `Augment/` |
| jetbrains | agent-cli | Junie CLI | yes | `JetBrains/` |
| moonshot | model-api | Kimi completions | no | `Moonshot/` |
| amp | agent-cli/runner | `amp -x`, orbs | yes | `Amp/` |
| t3chat | monitoring-only/unknown | no contract found | unknown | `T3Chat/` |
| ollama | local-runner/API | local `/api/*` | model only | `Ollama/` |
| synthetic | model-api | OpenAI/Anthropic API | no | `Synthetic/` |
| openrouter | agent-sdk/model-api | `@openrouter/agent` | SDK yes; API no | `OpenRouter/` |
| elevenlabs | media API | audio generation | no coding agent | `ElevenLabs/` |
| warp | agent-cli/cloud | Oz CLI | yes | `Warp/` |
| windsurf | agent-cli account | Devin CLI entitlement | yes through Devin | `Windsurf/` |
| zed | MCP/ACP host | hosts external agents | external only | `Zed/` |
| perplexity | model-api | chat/agent APIs | no local coding agent | `Perplexity/` |
| mimo | model-api | Xiaomi API | no | `MiMo/` |
| doubao | model-api | Ark API | no | `Doubao/` |
| sakana | agentic model-api | Fugu API/wrappers | API not workspace agent | `Sakana/` |
| abacus | agent-api/SDK | `executeAgent` | yes remote | `Abacus/` |
| mistral | agent-cli/ACP/API | Vibe | yes | `Mistral/` |
| deepseek | model-api | OpenAI-compatible chat | no | `DeepSeek/` |
| deepinfra | model-api | inference API | no | `DeepInfra/` |
| codebuff | agent-cli/SDK | CLI/`@codebuff/sdk` | yes | `Codebuff/` |
| crof | model-router API | OpenAI-compatible | no | `Crof/` |
| venice | model-api | stream/structured chat | no | `Venice/` |
| commandcode | agent-cli | `cmd` | yes; headless unknown | `CommandCode/` |
| qoder | agent-cli/ACP/SDK | `qoder -p` | yes | `Qoder/` |
| stepfun | model-api | chat API | no | `StepFun/` |
| bedrock | model-api/agent-api | Converse/AgentCore | AgentCore yes | `Bedrock/` |
| grok | agent-cli/ACP/API | `grok -p` | yes | `Grok/` |
| groq | model-api | chat API | no | `Groq/` |
| llmproxy | gateway | proxy routes | no contract | `LLMProxy/` |
| litellm | gateway/SDK | proxy/routing | no contract | `LiteLLM/` |
| deepgram | voice-agent API | WebSocket agent | voice only | `Deepgram/` |
| poe | model proxy API | API Bots proxy | no coding agent | `Poe/` |
| chutes | model-api/SDK | stream/tools | no | `Chutes/` |
| neuralwatt | monitoring-only/unknown | balance/quota only | unknown | `NeuralWatt/` |
| clawrouter | gateway | routing policy | no contract | `ClawRouter/` |
| longcat | model-api | OpenAI/Anthropic API | no | `LongCat/` |
| sub2api | gateway | subscription proxy | no contract | `Sub2API/` |
| wayfinder | gateway | local router | no | `Wayfinder/` |
| zenmux | model-router API | routed chat | no | `ZenMux/` |
| aiand | model-api/SDK | chat/Responses | no | `AiAnd/` |
| zoommate | monitoring-only/unknown | no surface found | unknown | `ZoomMate/` |
| xai | model-api | xAI API; Grok separate | no API agent | `XAI/` |
| notion | MCP host | workspace MCP | no coding agent | `Notion/` |
| ibmbob | agent-cli/ACP/MCP | Bob Shell | yes | `IBMBob/` |

## Contract families

Provider versions are not pinned by CodexBar. The launcher must feature-detect installed versions and required flags.

| Surface family | Model and continuation | Output and permissions | Auth, platform, failure |
| --- | --- | --- | --- |
| Codex | `codex exec -m`; resume by ID/last | JSONL, output schema, interrupt; read-only uses `--sandbox read-only --ask-for-approval never` | ChatGPT/API key scopes differ; macOS/Linux; missing flags and turn errors fail closed. [Docs](https://learn.chatgpt.com/docs/non-interactive-mode) |
| Claude Code | `claude -p --model`; continue/resume | JSON/stream JSON/JSON Schema; plan plus tool restrictions and MCP deny | OAuth/Console differ; SIGTERM 143; external timeout. [Docs](https://code.claude.com/docs/en/headless) |
| OpenAI Agents SDK | Agent/runner model; SDK or server session | streamed events, structured output, run state | caller tools and sandbox; API key/org scope; Python/TypeScript; run results/exceptions. [Docs](https://openai.github.io/openai-agents-python/) |
| Cline, Cursor | model/provider/session flags; resume where documented | JSON or stream events; plan, tool and command rules | ClinePass/provider key or login/API key; no universal OS sandbox; task/nonzero errors. [Cline](https://docs.cline.bot/cli/cli-reference), [Cursor](https://docs.cursor.com/en/cli/reference/parameters) |
| OpenCode, Qwen, Gemini | provider/model or model config; session continuation varies | JSON/stream JSON; Qwen has schema, budgets and sandbox; OpenCode/Gemini use config | installed CLI auth; local platform; unsupported flags and tool/provider errors must be explicit. [OpenCode](https://opencode.ai/v2/docs/cli/commands/), [Qwen](https://qwenlm.github.io/qwen-code-docs/en/users/features/headless/), [Gemini](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/headless.md) |
| Antigravity, Grok | model/agent; continue or conversation/session ID | JSON/stream JSON; sandbox or tool approval; Grok ACP JSON-RPC | cached/API auth; platform installers vary; explicit error status, RPC errors and timeout. [Antigravity](https://antigravity.google/docs/cli/headless/), [Grok](https://docs.x.ai/build/cli/headless-scripting) |
| Factory, Kiro, Kilo, Qoder | model/agent plus session or remote IDs | JSON/stream JSON/RPC; read-only, sandbox, tool allow/deny or trust modes | account/API key; local/cloud platforms; session, budget, permission and RPC errors. [Factory](https://docs.factory.ai/droid-exec/overview), [Kiro](https://kiro.dev/docs/cli/headless/), [Kilo](https://kilo.ai/docs/code-with-ai/platforms/cli-reference), [Qoder](https://docs.qoder.com/cli/cli-reference) |
| Kimi, Mistral Vibe, Auggie, Junie | model picker/flag and session history vary | Kimi/Vibe stream output; Vibe has tool filters and plan; Auggie/Junie headless schema or sandbox fields need probing | account/API/BYOK; platform support varies; parse/auth/permission errors must be surfaced. [Kimi](https://www.kimi.com/code/docs/en/kimi-code-cli/reference/kimi-command), [Vibe](https://docs.mistral.ai/vibe/code/cli/work-with-cli), [Auggie](https://docs.augmentcode.com/cli/overview), [Junie](https://junie.jetbrains.com/docs/junie-cli.html) |
| Copilot, Warp, IBM Bob | model/profile/environment flags vary; sessions are provider-managed | Copilot local/cloud sandbox; Warp runner policy; Bob trust/auto-approve/ACP | GitHub/Warp/IBM account or service key; OS support varies; tool/session errors explicit. [Copilot](https://docs.github.com/en/copilot/concepts/agents/copilot-cli/about-copilot-cli), [Warp](https://docs.warp.dev/reference/cli), [Bob ACP](https://bob.ibm.com/docs/shell/features/acp) |
| Devin, Manus, Abacus, Bedrock AgentCore | remote task/session IDs, same ID continues context | provider stream/events and structured output where documented; remote runtime owns tools | org/project/IAM/deployment auth; cloud permissions; HTTP status and lifecycle states are first-class. [Devin](https://docs.devin.ai/api-reference/v1/sessions/create-a-new-devin-session), [Manus](https://open.manus.ai/docs/v2/task-lifecycle), [Abacus](https://abacus.ai/help/api/ref/ai_agents/executeAgent), [AgentCore](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-invoke-agent.html) |
| OpenRouter Agent, Codebuff SDK | model/agent ID and SDK-managed state | streaming, tool calls, structured tool data; caller executes tools | API key; Node/TypeScript SDKs; caller sandbox and tool errors. [OpenRouter](https://openrouter.ai/docs/agent-sdk/overview), [Codebuff](https://www.codebuff.com/docs/advanced/sdk) |
| Ollama | local model and caller message history | JSON/JSON Schema and NDJSON; no repository agent or filesystem sandbox | local/cloud auth and host policy; HTTP/process errors. [Ollama](https://docs.ollama.com/api/generate) |
| Zed and Notion MCP | external client owns model/session | MCP tool results over ACP or HTTP/SSE; tools can write | OAuth or client auth and user permissions; transport/auth/tool errors. [Zed](https://zed.dev/docs/ai/external-agents), [Notion](https://developers.notion.com/guides/mcp/overview) |

## Model and gateway boundaries

OpenAI-compatible providers including Fireworks, Z.AI, MiniMax, Moonshot, Synthetic, Perplexity, MiMo, Doubao, DeepSeek, DeepInfra, Crof, Venice, StepFun, Groq, Poe, Chutes, LongCat, ZenMux, ai&, and xAI document explicit models, messages, tool calls and usually SSE. Structured output varies by model. Continuation is caller message history, not a shared coding-agent session. Representative sources: [Fireworks](https://docs.fireworks.ai/api-reference/post-chatcompletions), [DeepSeek](https://api-docs.deepseek.com/api/create-chat-completion/), [Venice](https://docs.venice.ai/api-reference/endpoint/chat/completions), [MiMo](https://mimo.mi.com/docs/en-US/api/chat/openai-api), [LongCat](https://longcat.chat/platform/docs/api/chat.html), [ai&](https://docs.aiand.com/sdks/aiand/).

Vertex AI and Alibaba Model Studio provide model APIs and SDKs but do not own the local coding-agent loop. [Vertex](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/samples/googlegenaisdk-textgen-with-txt-stream), [Alibaba](https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-responses).

Ollama is a local model runner with `/api/generate`, `/api/chat`, NDJSON streaming and JSON or JSON Schema output. It has no built-in repository agent or filesystem sandbox. [Ollama](https://docs.ollama.com/api/generate).

Sakana Fugu and Namazu expose agentic model APIs and official Codex/Claude wrappers, but the caller still owns local tools and workspace permissions. [Sakana](https://console.sakana.ai/get-started).

LiteLLM, LLMProxy, ClawRouter, sub2api, and Wayfinder are routing or accounting surfaces. They do not establish an agent loop or workspace permission contract. [LiteLLM](https://docs.litellm.ai/docs/proxy/quick_start), [Wayfinder pinned docs](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/wayfinder.md), [sub2api pinned docs](https://github.com/steipete/CodexBar/blob/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/docs/sub2api.md).

## Boundaries and implementation consequences

- CodexBar observes usage and existing Codex, Claude Code, pi, and omp sessions. It does not submit arbitrary prompts. Pi and omp are monitoring surfaces only.
- `bl usage token-plan` is a quota command, not evidence of `bl` agent execution.
- A credential row does not prove that an executable is installed, authenticated, entitled to a model, or able to run headlessly.
- A model API can back an external agent, but it remains a model interface, not the provider's Agent Integration.
- `unknown`, unavailable, authentication failure, model rejection, permission denial, timeout, cancellation, rate limit, and missing terminal stream result must remain separate states.

All pinned source paths and all 69 provider IDs are reconciled against issue 8's canonical artifact. The implementation target is a provider-neutral launcher registry with provider-specific capability probes for executable/protocol version, model, continuation, output, permissions, auth scope, platform, and terminal failure state.

Every `Source` cell resolves under the pinned [CodexBar provider tree](https://github.com/steipete/CodexBar/tree/caad1ca38c237fc77426ad06cf56a2daa1dbbcb9/Sources/CodexBarCore/Providers/). The canonical issue 8 artifact supplies the exact provider file paths, monitoring metrics, and evidence links for all rows; this report adds the execution-interface classification and primary execution contracts.
