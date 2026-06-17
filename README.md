# AI CLI Usage Monitor

One file, no dependencies. Shows the **remaining** subscription quota for your AI
coding CLIs, ordered by whichever **weekly / monthly** window resets soonest, with
a live countdown and renew date per window.

```bash
python3 usage_monitor.py            # dashboard at http://127.0.0.1:8787
python3 usage_monitor.py --open     # + open browser
python3 usage_monitor.py --json     # print the raw usage JSON and exit
python3 usage_monitor.py --port 9000
```

## What it reads

| Provider          | Source        | How                                                                 |
|-------------------|---------------|---------------------------------------------------------------------|
| Claude Code       | **live**      | `GET api.anthropic.com/api/oauth/usage` (token from `~/.claude/.credentials.json`) |
| Codex CLI         | **live** / cache | `GET chatgpt.com/backend-api/codex/usage` (token from `~/.codex/auth.json`); falls back to last `rate_limits` session snapshot |
| GitHub Copilot    | **live** / schedule | `api.github.com/copilot_internal/v2/token` quota snapshots (token from `~/.config/gh/hosts.yml`) |
| Gemini CLI        | **live** / schedule | `:loadCodeAssist` → `:retrieveUserQuota` for real per-model bars (needs `GOOGLE_CLOUD_PROJECT` for Standard/Enterprise); falls back to daily reset clock if no project |
| Antigravity CLI   | schedule      | scans `~/.gemini/antigravity*`; falls back to a ~5h reset clock      |

**Source legend:** `live` = pulled from the provider API · `cache` = read from the
CLI's own local snapshot · `schedule` = reset countdown only (the provider exposes
no usage number for individual plans yet).

## Notes

- Credentials are read **locally only**, to authenticate each provider's own usage
  endpoint — exactly like the CLIs do. Tokens are never printed or sent anywhere else.
- Claude's `oauth/usage` endpoint rate-limits aggressively, so it's cached for 180s.
- The dashboard auto-refreshes every 60s; countdowns tick every second client-side.
- Ordering: providers **with** a weekly/monthly window come first (soonest reset on
  top); providers that only expose shorter windows (Gemini daily, Antigravity 5h)
  follow.
