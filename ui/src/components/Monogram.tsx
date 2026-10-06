// Provider monogram tiles stand in for brand icons: the dashboard
// never fetches third-party icon assets, so each provider gets a
// hue-keyed letter tile.

const PROVIDER_HUE: Record<string, string> = {
  anthropic: "#b04a2a", claude: "#b04a2a", antigravity: "#b04a2a",
  openai: "#1d1d1f", codex: "#1d1d1f",
  copilot: "#56349e", github: "#56349e",
  gemini: "#1a73e8", google: "#1a73e8",
  groq: "#d63c23", mistral: "#d9660a", deepseek: "#3d5bf5",
  cohere: "#39594d", openrouter: "#5151d6", together: "#0f6fff",
  fireworks: "#8a2be2", perplexity: "#12808d", xai: "#1d1d1f",
  nvidia: "#5c8f00", huggingface: "#c47c07", cerebras: "#7c3aed",
  deepinfra: "#0e8a5f", moonshot: "#8a5d00", siliconflow: "#0e7490",
  zai: "#b02348", cursor: "#1d1d1f", devin: "#0066cc",
  opencode: "#0e7490", auto: "#0066cc", typesafe: "#0e7490",
};

// A selector's provider part: "groq:api-key/llama" -> "groq",
// "openai/gpt-5" -> "openai", "codex" -> "codex".
export function providerHue(name: string): string {
  const key = String(name || "").toLowerCase().split(/[:/]/)[0];
  return PROVIDER_HUE[key] || "#6b6b70";
}

export function Monogram({ name }: { name: string }) {
  const lead = String(name || "").toLowerCase().match(/[a-z0-9]/);
  return (
    <span
      className="provider-icon"
      style={{ ["--ph" as string]: providerHue(name) }}
      aria-hidden="true"
    >
      {lead ? lead[0].toUpperCase() : "?"}
    </span>
  );
}
