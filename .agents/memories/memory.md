# Project Memory

## Provider discovery

- Treat provider discovery as a local, side-effect-free presence check. It must not call provider APIs, refresh tokens, or write credentials.
- Recompute the discovered-provider set before every collection and use that set consistently across browser, text, and JSON output.
- Instantiate and render only discovered providers. Keep a discovered provider visible when later authentication or quota retrieval fails.
- Keep GitHub Copilot credentials separate from GitHub CLI authentication and `GH_TOKEN` or `GITHUB_TOKEN`.
- Hide a provider when its discovery check fails, but report the failure separately in the dashboard, stderr, and JSON. One-shot modes return a nonzero exit code while preserving successful partial results.
- Treat an empty discovered-provider set as a successful result with an intentional empty state in every output mode.
- Keep a static supported-provider catalog. Each provider class owns its provider-specific discovery knowledge.
