# Project Memory

## Provider discovery

- Treat provider discovery as a local, side-effect-free presence check. It must not call provider APIs, refresh tokens, or write credentials.
- Recompute the discovered-provider set before every collection and use that set consistently across browser, text, and JSON output.
- Instantiate and render only discovered providers. Keep a discovered provider visible when later authentication or quota retrieval fails.
- Keep GitHub Copilot credentials separate from GitHub CLI authentication and `GH_TOKEN` or `GITHUB_TOKEN`.
- Hide a provider when its discovery check fails, but report the failure separately in the dashboard, stderr, and JSON. One-shot modes return a nonzero exit code while preserving successful partial results.
- Treat an empty discovered-provider set as a successful result with an intentional empty state in every output mode.
- Keep a static supported-provider catalog. Each provider class owns its provider-specific discovery knowledge.

## Rust Provider Discovery

- Keep discovery local, side-effect-free, and metadata-only. Do not parse credentials, refresh tokens, call provider APIs, spawn subprocesses, or write user state.
- Resolve platform home directories at the executable boundary; inject an explicit discovery context into library code and tests.
- Let each provider definition own its provider-specific source markers. A present marker initializes an identity-only adapter without validating its contents.
- Recompute discovery for each collection. Initialize only present providers; report inspection failures separately with safe, path-free diagnostics.
- Keep the static provider catalog separate from the discovered set. Preserve empty discovery as a successful empty result and keep provider-owned authentication boundaries explicit.

## MCP Gateway

- Before adopting an MCP SDK transport, check its default framing, shutdown, process-tree, and feature-selected MSRV behavior against the approved contract. An available limit or process wrapper does not mean the default transport uses it.
- A descendant-process cleanup test must prove the child reached its armed state, then wait past its marker deadline while the marker directory still exists. An immediate absent-marker assertion can pass even when the process survives.
