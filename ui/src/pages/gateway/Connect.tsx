import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { apiGet } from "@/lib/api";
import { Card } from "@/components/ui/card";
import { CopyButton, Msg, usePageHeader } from "./shared";

// Connect: endpoints, client snippets, addressing rules. Snippets are
// built against the real origin so the pasted config works verbatim.

const ORIGIN = window.location.origin;

const CLIENT_CARDS: { name: string; sub: string; snippet: (o: string) => string }[] = [
  {
    name: "Cline / Roo Code",
    sub: 'VS Code extension - choose the "OpenAI Compatible" provider.',
    snippet: (o) =>
`API Provider:   OpenAI Compatible
Base URL:       ${o}/v1
API Key:        aifuel-gw-<your key>
Model:          auto`,
  },
  {
    name: "Codex CLI",
    sub: "Environment variables for the current shell, or pin a provider in ~/.codex/config.toml.",
    snippet: (o) =>
`export OPENAI_BASE_URL=${o}/v1
export OPENAI_API_KEY=aifuel-gw-<your key>

# or in ~/.codex/config.toml:
# model_provider = "aifuel"
# model = "auto"
# [model_providers.aifuel]
# name     = "aifuel"
# base_url = "${o}/v1"
# wire_api = "chat"
# env_key  = "OPENAI_API_KEY"`,
  },
  {
    name: "aider",
    sub: "OpenAI-compatible flags; prefix the model with openai/.",
    snippet: (o) =>
`aider --openai-api-base ${o}/v1 \\
      --openai-api-key aifuel-gw-<your key> \\
      --model openai/auto`,
  },
  {
    name: "LibreChat",
    sub: "Custom endpoint block in librechat.yaml.",
    snippet: (o) =>
`endpoints:
  custom:
    - name: "aifuel"
      baseURL: "${o}/v1"
      apiKey: "aifuel-gw-<your key>"
      models:
        default: ["auto"]
        fetch: true
      titleConvo: true
      modelDisplayLabel: "aifuel"`,
  },
  {
    name: "OpenAI SDK",
    sub: "Python - any OpenAI-compatible SDK takes the same two settings.",
    snippet: (o) =>
`from openai import OpenAI

client = OpenAI(
    base_url="${o}/v1",
    api_key="aifuel-gw-<your key>",
)
resp = client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "Say hello"}],
)`,
  },
  {
    name: "curl",
    sub: "Smoke-test the gateway directly.",
    snippet: (o) =>
`curl ${o}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer aifuel-gw-<your key>" \\
  -d '{
    "model": "auto",
    "messages": [{"role": "user", "content": "Say hello"}]
  }'`,
  },
];

export default function Connect() {
  const [hasKeys, setHasKeys] = useState<boolean | null>(null);
  const [error, setError] = useState("");

  const load = useCallback(async () => {
    try {
      const data = await apiGet<{ keys?: unknown[] }>("/api/gateway/keys");
      setHasKeys((data.keys || []).length > 0);
      setError("");
    } catch (e) {
      setError(`Could not check gateway keys: ${(e as Error).message}`);
    }
  }, []);

  usePageHeader(load);
  useEffect(() => {
    load();
  }, [load]);

  return (
    <section aria-label="Connect">
      <p className="m-0 max-w-[66ch] text-[13px] leading-[1.45] tracking-[-0.006em] text-ink-48">
        Point any OpenAI- or Anthropic-compatible client at this gateway. Requests run as read-only
        Agent Runs against your local integrations - one port fronts every configured provider.
      </p>

      {hasKeys === false && (
        <div role="status" className="mt-3.5 flex flex-wrap items-center justify-between gap-4 rounded-[11px] border-[0.5px] border-line bg-canvas px-[18px] py-3.5 shadow-[0_1px_2px_rgba(29,29,31,0.05)]">
          <div>
            <div className="text-[13px] font-semibold">Authentication is currently open</div>
            <div className="mt-1 max-w-[64ch] text-xs leading-[1.4] text-ink-48">
              No gateway keys exist yet, so every <code className="rounded bg-ink/[0.04] px-1 text-[11px]">/v1</code>{" "}
              request runs as <code className="rounded bg-ink/[0.04] px-1 text-[11px]">anonymous</code> - any local
              client can call the gateway. Create a key on the API Keys page, then send it as{" "}
              <code className="rounded bg-ink/[0.04] px-1 text-[11px]">Authorization: Bearer aifuel-gw-…</code>.
            </div>
          </div>
          <Link
            to="/gateway/keys"
            className="inline-flex items-center rounded-full bg-ink/[0.04] px-3.5 py-[7px] text-[12.5px] font-semibold text-accent hover:bg-ink/[0.06]"
          >
            Create a key
          </Link>
        </div>
      )}

      <Card className="mt-3.5 px-[18px] pb-3 pt-2.5">
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-[240px] flex-1">
            <div className="text-[13px] font-semibold tracking-[-0.006em]">OpenAI-compatible base URL</div>
            <code className="mt-1.5 block select-all break-all rounded-lg border-[0.5px] border-line bg-pearl px-3 py-2 font-mono text-[12.5px] text-ink">
              {ORIGIN}/v1
            </code>
            <div className="mt-1.5 max-w-[64ch] text-xs leading-[1.4] text-ink-48">
              Use as the <code className="rounded bg-ink/[0.04] px-1">base_url</code> of any OpenAI SDK or
              compatible app. Serves <code className="rounded bg-ink/[0.04] px-1">chat/completions</code>,{" "}
              <code className="rounded bg-ink/[0.04] px-1">completions</code>,{" "}
              <code className="rounded bg-ink/[0.04] px-1">embeddings</code>,{" "}
              <code className="rounded bg-ink/[0.04] px-1">responses</code>,{" "}
              <code className="rounded bg-ink/[0.04] px-1">decisions</code>,{" "}
              <code className="rounded bg-ink/[0.04] px-1">audio</code>, and{" "}
              <code className="rounded bg-ink/[0.04] px-1">models</code>.
            </div>
          </div>
          <CopyButton text={`${ORIGIN}/v1`} />
        </div>
        <hr className="my-3 h-px border-none bg-hairline" />
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-[240px] flex-1">
            <div className="text-[13px] font-semibold tracking-[-0.006em]">Anthropic Messages endpoint</div>
            <code className="mt-1.5 block select-all break-all rounded-lg border-[0.5px] border-line bg-pearl px-3 py-2 font-mono text-[12.5px] text-ink">
              {ORIGIN}/v1/messages
            </code>
            <div className="mt-1.5 max-w-[64ch] text-xs leading-[1.4] text-ink-48">
              Anthropic SDK clients post here - point <code className="rounded bg-ink/[0.04] px-1">base_url</code>{" "}
              (or <code className="rounded bg-ink/[0.04] px-1">ANTHROPIC_BASE_URL</code>) at the base URL above
              and the SDK appends <code className="rounded bg-ink/[0.04] px-1">/v1/messages</code>.
            </div>
          </div>
          <CopyButton text={`${ORIGIN}/v1/messages`} />
        </div>
      </Card>

      <h3 className="mb-0 mt-5 text-sm font-semibold tracking-[-0.008em]">Client setup</h3>
      <div className="mt-3 grid grid-cols-[repeat(auto-fill,minmax(310px,1fr))] gap-3">
        {CLIENT_CARDS.map((spec) => (
          <Card key={spec.name} className="flex min-w-0 flex-col px-[18px] py-3.5">
            <div className="flex items-center justify-between gap-2">
              <span className="text-sm font-semibold tracking-[-0.008em]">{spec.name}</span>
              <CopyButton text={spec.snippet(ORIGIN)} />
            </div>
            <div className="mt-1 text-xs leading-[1.4] text-ink-48">{spec.sub}</div>
            <pre className="mt-2.5 flex-1 overflow-x-auto whitespace-pre rounded-lg border-[0.5px] border-line bg-pearl px-3 py-2.5 font-mono text-[11.5px] leading-[1.5] text-ink">
              <code>{spec.snippet(ORIGIN)}</code>
            </pre>
          </Card>
        ))}
      </div>

      <h3 className="mb-0 mt-5 text-sm font-semibold tracking-[-0.008em]">How model addressing works</h3>
      <Card className="mt-3 px-[18px] pb-3 pt-2.5">
        <ul className="m-0 list-disc pl-[18px] text-[13px] leading-[1.5] tracking-[-0.006em] text-ink-48 [&>li+li]:mt-1.5">
          <li>
            A name from the <b className="text-ink">Routes</b> page resolves first: an <b className="text-ink">alias</b>{" "}
            rewrites the model string to another selector; a <b className="text-ink">combo</b> expands to an
            ordered failover list.
          </li>
          <li>
            <code className="rounded bg-ink/[0.04] px-1 text-xs">auto</code> (or{" "}
            <code className="rounded bg-ink/[0.04] px-1 text-xs">auto/&lt;model&gt;</code>) plans a route through the
            shared route planner - quota-headroom providers first, then free-tier and paid API-key integrations,
            then the unmeasured tail. <code className="rounded bg-ink/[0.04] px-1 text-xs">decide</code> ranks the
            same way, then a decision model picks which candidate leads.
          </li>
          <li>
            <code className="rounded bg-ink/[0.04] px-1 text-xs">&lt;integration&gt;/&lt;model&gt;</code> pins one
            integration and passes the remainder as the provider-native model id - split on the first{" "}
            <code className="rounded bg-ink/[0.04] px-1 text-xs">/</code>, so model ids containing slashes like{" "}
            <code className="rounded bg-ink/[0.04] px-1 text-xs">openai/gpt-5</code> still work. A trailing{" "}
            <code className="rounded bg-ink/[0.04] px-1 text-xs">@effort</code> pins a reasoning level.
          </li>
          <li>
            A bare <code className="rounded bg-ink/[0.04] px-1 text-xs">&lt;integration&gt;</code> or unique{" "}
            <code className="rounded bg-ink/[0.04] px-1 text-xs">&lt;provider&gt;</code> runs that target with the
            provider's default model.
          </li>
          <li>
            Any other bare string is treated as a catalog model id and routes to the ranked candidate whose
            provider advertises it.
          </li>
        </ul>
      </Card>
      <Msg>{error && <span className="text-err">{error}</span>}</Msg>
    </section>
  );
}
