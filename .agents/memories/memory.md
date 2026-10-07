# Project Memory

## Gemini CLI removal (2026-10-07)

- Remove Gemini CLI from the shipped Rust Supported Provider, Agent Integration, MCP Host, and declared integration catalogs.
- Preserve Antigravity's `.gemini` credential/config paths, Gemini model IDs offered by other integrations, and `AIFUEL_GEMINI_API_URL`, which configures the shared Code Assist endpoint.
- Legacy Python code and historical research, specifications, acceptance records, and ADRs remain outside this removal's scope.

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
- Verify host-facing and upstream MCP version negotiation separately. Codex CLI 0.155.0 requests 2025-06-18, and a compatible Gateway must answer with that supported version instead of rejecting every version older than 2025-11-25.
- When compatibility behavior changes, update the canonical Wayfinder decision and implementation acceptance criteria; merged code and acceptance docs do not revise a closed contract by themselves.

## Agent Runs

- Keep Agent Run execution independent from Provider Discovery, quota monitoring, and Agent MCP Registration. A user-selected provider runs only through its matching registered execution adapter; unsupported capability requests stay explicit.
- The execution adapter owns any child process it starts. Cancellation must stop and reap the process, and completed results retain captured output without inventing provider session, account, or effective-model identity.
- Verify the real `aifuel run` process with controlled provider executables and a temporary home. A credential file marker establishes local presence only; authenticated prompt acceptance requires a separate live result, recorded without changing saved user configuration.

## Dashboard

- The dashboard mutates credentials through the same Credential Store and helper semantics as `aifuel auth` (`describe_source`, `resolve_credential_ref`, `removal_warnings`) - share them rather than duplicating status wording.
- tiny_http can RST the connection when a response is sent while request body bytes remain unread; drain the body (bounded) before answering a rejected or unmatched request.
- `AIFUEL_HOME` steers only the DiscoveryContext home; the Credential Store follows `XDG_CONFIG_HOME`/`HOME`, so dashboard tests and manual runs must override those to isolate the store.
- `cargo run` under a sandboxed HOME breaks rustup's toolchain resolution; pin `RUSTUP_HOME`/`CARGO_HOME` to the real dotdirs when exercising the dashboard with a fake HOME.
- Requiring `application/json` bodies on mutation endpoints doubles as CSRF hardening - a cross-site HTML form cannot produce that content type, complementing the loopback Host/Origin/Sec-Fetch-Site check.

## OpenAI Gateway (Tier 2/3)

- The per-key model allowlist gates the verbatim inbound `model` string, not the resolved integration/model - `model` lives in the request body, so enforcement belongs inside each handler after parsing, not at dispatch. `tiny_http::Request` has no test constructor; test `authorize`/`permits` at the `KeyStore` level and rely on live E2E for the HTTP wrapper.
- `routes::resolve` reads `gateway.json` fresh on every call by design - a PUT that validates and atomically rewrites (temp + rename, 0600) takes effect on the next request with no cache invalidation. Alias shadows a same-named combo; PUT mirrors that precedence rather than rejecting the collision.
- Embeddings cannot ride `AgentExecutionAdapter` (prompt-in/text-out). Route `ExecutionConfig::Http` + `WireApi::OpenAiChat` integrations to `{base_url}/embeddings` through a providers-crate seam that applies the instance `AuthBinding` and scrubs credential echoes from upstream error bodies.
- Inline OAuth refresh belongs inside `execute` when the access token is expired or inside a ~60s margin and a `refresh_token` exists - never refresh a valid token (rotation is consuming), write back atomically preserving unknown fields, serialize with a per-adapter `OnceLock<tokio::Mutex>` (tokio `Mutex::new` is not const, so a static adapter cannot hold one inline).
- GitHub Copilot Business seats reject `POST /copilot_internal/v2/token` with 403, not 404 - auth-class failures (401/403/404/410) must all reconcile through `/copilot_internal/user`, which reports the seat's real `endpoints.api` (e.g. `api.business.githubcopilot.com`) where the `gho_` token works as bearer directly.
- A branch whose base commits entered `main` via squash merge rebases cleanly with `git rebase --onto origin/main <last-merged-commit>` - a plain rebase tries to re-apply already-merged commits and conflicts.

## CI and releases

- The user prefers release-only CI (Option A): workflows trigger on `v*` tags and manual `workflow_dispatch`, not on every commit, because commit frequency is high. `.github/workflows/release.yml` runs verify (cargo test on ubuntu) -> 5-target build matrix -> GitHub Release with archives and sha256 files.
- `crates/aifuel/build.rs` auto-runs `pnpm install --frozen-lockfile` + `pnpm build` when `ui/dist` is missing, so CI only needs pnpm+node on PATH; no separate UI job.
- `cargo fmt --check` is not currently clean (43 diffs) and `src/`+`tests/` Python is the legacy implementation, not the shipped artifact - keep both out of release gates.
- Repo is a bare-worktree layout (`.bare/` + worktree dirs under `/home/willnguyen/Developer/aifuel/`): use the bare-repo-worktree skill, never edit `main/` directly.
