#!/usr/bin/env python3
"""
aifuel — fuel gauge for your AI coding subscriptions
====================================================

Single-file dashboard that shows the *remaining* subscription quota for the AI
coding CLIs you use, ordered by whichever weekly / monthly window resets soonest.

Providers:
    - Claude Code      (live  : api.anthropic.com/api/oauth/usage)
    - Codex CLI        (live  : chatgpt.com/backend-api/codex/usage; cache fallback)
    - GitHub Copilot   (live  : api.github.com/copilot_internal/v2/token  + fallback)
    - Gemini CLI       (live  : loadCodeAssist -> retrieveUserQuota; daily-reset fallback)
    - Antigravity CLI  (live  : Code Assist quota via its own token, auto-refreshed; scan + schedule fallback)

Nothing here prints your tokens. Credential files are read locally only to
authenticate the provider's own usage endpoint, exactly like the CLIs do. For
Gemini, an expired access token is refreshed against Google's OAuth endpoint from
its stored refresh_token and written back to ~/.gemini/oauth_creds.json -- the
same exchange the CLI performs on startup.

Usage:
    python3 usage_monitor.py            # serve dashboard at http://127.0.0.1:8787
    python3 usage_monitor.py --json     # print the usage JSON and exit
    python3 usage_monitor.py --text     # print a compact colored terminal summary and exit
    python3 usage_monitor.py --port N   # use a different port
    python3 usage_monitor.py --open     # also open the browser

Stdlib only. No dependencies.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
import glob
import argparse
import threading
import webbrowser
import urllib.request
import urllib.error
import urllib.parse
from datetime import datetime, timezone, timedelta
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOME = os.path.expanduser("~")
HTTP_TIMEOUT = 12  # seconds
HTML_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "index.html")

# Live usage endpoints (read-only; authenticated with the CLI's own local token).
CODEX_USAGE_URL = "https://chatgpt.com/backend-api/codex/usage"
GEMINI_API = "https://cloudcode-pa.googleapis.com/v1internal:"  # + loadCodeAssist | retrieveUserQuota

# ---------------------------------------------------------------------------
# Public OAuth clients of the upstream CLIs (NOT our secrets, NOT a leak)
# ---------------------------------------------------------------------------
# The constants below are the *public* OAuth "installed app" (a.k.a. desktop /
# native app) client credentials that ship hardcoded inside Google's own
# gemini-cli and antigravity-cli. They are deliberately embedded in those
# distributed binaries, so they are not confidential -- the "client secret"
# here is public by design. Google's OAuth docs say so explicitly:
#
#   "The process results in a client ID and, in some cases, a client secret,
#    which you embed in the source code of your application. (In this context,
#    the client secret is obviously not treated as a secret.)"
#   https://developers.google.com/identity/protocols/oauth2/native-app
#
# Why we have them: the only credential that actually grants access is the
# per-user `refresh_token` already stored on disk (written there when the user
# logged in via the CLI). Google's token endpoint requires the *same* client_id
# + client_secret that originally minted a refresh_token in order to exchange it
# for a fresh access_token. So we reuse each CLI's public client pair to perform
# the exact same refresh exchange the CLI itself runs on startup -- nothing more.
# We never transmit the refresh_token anywhere except Google's token endpoint.
#
# Secret-scanners (GitHub push protection, gitleaks, etc.) may still flag the
# `GOCSPX-` prefix on pattern alone; that's a false positive given the above.

# gemini-cli's public installed-app client. Source: google-gemini/gemini-cli
# packages/core/src/code_assist/oauth2.ts. Refreshes the token stored in
# ~/.gemini/oauth_creds.json.
GEMINI_CLI_PUBLIC_CLIENT_ID = "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
GEMINI_CLI_PUBLIC_CLIENT_SECRET = "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"  # public, embedded in the CLI -- see note above
GOOGLE_TOKEN_URI = "https://oauth2.googleapis.com/token"

# antigravity-cli's public installed-app client (consumer login). Same deal as
# gemini-cli's above. Refreshes the token stored in
# ~/.gemini/antigravity-cli/antigravity-oauth-token.
ANTIGRAVITY_CLI_PUBLIC_CLIENT_ID = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
ANTIGRAVITY_CLI_PUBLIC_CLIENT_SECRET = "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"  # public, embedded in the CLI -- see note above

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

def now_ts() -> float:
    return time.time()


def to_epoch(value) -> float | None:
    """Normalize a reset timestamp (unix int/float or ISO-8601 string) to epoch."""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        # Heuristic: values > 1e12 are milliseconds.
        return float(value) / 1000.0 if value > 1e12 else float(value)
    if isinstance(value, str):
        s = value.strip()
        if not s:
            return None
        if s.isdigit():
            return to_epoch(int(s))
        try:
            s2 = s.replace("Z", "+00:00")
            dt = datetime.fromisoformat(s2)
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=timezone.utc)
            return dt.timestamp()
        except ValueError:
            return None
    return None


def deep_find(obj, keys):
    """Depth-first search for the first value under any of `keys`."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k in keys and v not in (None, ""):
                return v
        for v in obj.values():
            found = deep_find(v, keys)
            if found is not None:
                return found
    elif isinstance(obj, list):
        for item in obj:
            found = deep_find(item, keys)
            if found is not None:
                return found
    return None


def http_get(url, headers=None, data=None, method=None):
    headers = headers or {}
    body = None
    if data is not None:
        body = json.dumps(data).encode("utf-8")
        headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:
        raw = resp.read().decode("utf-8", "replace")
    try:
        return json.loads(raw), None
    except json.JSONDecodeError:
        return raw, None


def read_json(path):
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def write_json_atomic(path, data):
    """Overwrite `path` with `data` atomically, preserving its file mode."""
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(data, fh)
    try:
        os.chmod(tmp, os.stat(path).st_mode & 0o777)
    except OSError:
        pass
    os.replace(tmp, path)


def window(label, period, used_percent=None, remaining_percent=None,
           used=None, limit=None, resets_at=None):
    if remaining_percent is None and used_percent is not None:
        remaining_percent = round(max(0.0, 100.0 - used_percent), 1)
    if used_percent is None and remaining_percent is not None:
        used_percent = round(max(0.0, 100.0 - remaining_percent), 1)
    return {
        "label": label,
        "period": period,  # "5h" | "daily" | "weekly" | "monthly" | "unknown"
        "used_percent": used_percent,
        "remaining_percent": remaining_percent,
        "used": used,
        "limit": limit,
        "resets_at": to_epoch(resets_at),
    }


def result(key, name, status="ok", plan=None, source=None, detail=None, windows=None):
    windows = windows or []
    return {
        "key": key,
        "name": name,
        "status": status,        # "ok" | "partial" | "unavailable" | "error"
        "plan": plan,
        "source": source,        # "live" | "local-cache" | "schedule" | None
        "detail": detail,
        "windows": windows,
    }


def next_month_first_utc() -> float:
    n = datetime.now(timezone.utc)
    year, month = (n.year + 1, 1) if n.month == 12 else (n.year, n.month + 1)
    return datetime(year, month, 1, tzinfo=timezone.utc).timestamp()


def next_midnight_pacific() -> float:
    """Next 00:00 America/Los_Angeles, expressed as epoch (PST/PDT approx -8/-7)."""
    # Approximate DST: Mar 9 .. Nov 2 -> PDT(-7), else PST(-8). Good enough for a reset clock.
    n = datetime.now(timezone.utc)
    offset = -7 if (3, 9) <= (n.month, n.day) <= (11, 2) else -8
    tz = timezone(timedelta(hours=offset))
    local = n.astimezone(tz)
    nxt = (local + timedelta(days=1)).replace(hour=0, minute=0, second=0, microsecond=0)
    return nxt.timestamp()


# ---------------------------------------------------------------------------
# Providers
# ---------------------------------------------------------------------------

def fetch_claude():
    cred_path = os.path.join(HOME, ".claude", ".credentials.json")
    if not os.path.exists(cred_path):
        return result("claude", "Claude Code", "unavailable",
                      detail="No ~/.claude/.credentials.json")
    try:
        creds = read_json(cred_path)
    except Exception as e:
        return result("claude", "Claude Code", "error", detail=f"creds unreadable: {e}")

    token = deep_find(creds, {"accessToken", "access_token"})
    if not token:
        return result("claude", "Claude Code", "unavailable",
                      detail="No access token in credentials")

    headers = {
        "Authorization": f"Bearer {token}",
        "anthropic-beta": "oauth-2025-04-20",
        "anthropic-version": "2023-06-01",
        "User-Agent": "claude-cli/usage-monitor (external)",
        "Accept": "application/json",
    }
    try:
        data, _ = http_get("https://api.anthropic.com/api/oauth/usage", headers=headers)
    except urllib.error.HTTPError as e:
        hint = " (429 = polled too fast; wait a few min)" if e.code == 429 else ""
        return result("claude", "Claude Code", "error", detail=f"HTTP {e.code}{hint}")
    except Exception as e:
        return result("claude", "Claude Code", "error", detail=str(e))

    if not isinstance(data, dict):
        return result("claude", "Claude Code", "error", detail="unexpected response")

    windows = []
    for k, v in data.items():
        if not isinstance(v, dict):
            continue
        util = v.get("utilization")
        if util is None:
            util = v.get("used_percent")
        resets = v.get("resets_at") or v.get("reset_at") or v.get("resets")
        if util is None and resets is None:
            continue
        kl = k.lower()
        if "five" in kl or "5h" in kl or "5_hour" in kl or "hour" in kl:
            label, period = "5-hour", "5h"
        elif "seven" in kl or "week" in kl or "7" in kl:
            label, period = ("Weekly (Opus)" if "opus" in kl else "Weekly"), "weekly"
        elif "month" in kl:
            label, period = "Monthly", "monthly"
        else:
            label, period = k.replace("_", " ").title(), "unknown"
        up = float(util) if util is not None else None
        if up is not None and up <= 1.0:  # fraction -> percent
            up *= 100.0
        windows.append(window(label, period, used_percent=up, resets_at=resets))

    plan = deep_find(data, {"plan", "subscription", "tier"})
    if not windows:
        return result("claude", "Claude Code", "partial", plan=plan, source="live",
                      detail="connected but no usage windows in response")
    return result("claude", "Claude Code", "ok", plan=plan, source="live", windows=windows)


def _period_for_seconds(secs):
    """Map a window length in seconds to (period, default_label)."""
    if not secs:
        return "unknown", "Window"
    mins = secs / 60
    if mins <= 360:
        return "5h", f"{int(round(mins / 60))}-hour"
    if mins <= 1500:
        return "daily", "Daily"
    if mins <= 20160:
        return "weekly", "Weekly"
    return "monthly", "Monthly"


def _codex_window(rl_window, label_override=None):
    """Build a window() from a ChatGPT `*_window` rate-limit object."""
    if not isinstance(rl_window, dict):
        return None
    period, label = _period_for_seconds(rl_window.get("limit_window_seconds"))
    if label_override:
        label = label_override
    resets = rl_window.get("reset_at")
    if resets is None and rl_window.get("reset_after_seconds") is not None:
        resets = now_ts() + float(rl_window["reset_after_seconds"])
    return window(label, period, used_percent=rl_window.get("used_percent"),
                  resets_at=resets)


def fetch_codex():
    """Live: ChatGPT backend usage endpoint (same data the Codex TUI refreshes).

    Falls back to the last local `rate_limits` session snapshot if the live call
    fails (e.g. expired token).
    """
    auth_path = os.path.join(HOME, ".codex", "auth.json")
    token = account = None
    if os.path.exists(auth_path):
        try:
            auth = read_json(auth_path)
            token = deep_find(auth, {"access_token"})
            account = deep_find(auth, {"account_id"})
        except Exception:
            pass

    if token:
        try:
            data, _ = http_get(CODEX_USAGE_URL, headers={
                "Authorization": f"Bearer {token}",
                "chatgpt-account-id": account or "",
                "originator": "codex_cli_rs",
                "User-Agent": "codex_cli_rs/usage-monitor",
                "Accept": "application/json",
            })
            rl = data.get("rate_limit") if isinstance(data, dict) else None
            if isinstance(rl, dict):
                plan = data.get("plan_type")
                windows = []
                w = _codex_window(rl.get("primary_window"), "5-hour")
                if w:
                    windows.append(w)
                w = _codex_window(rl.get("secondary_window"), "Weekly")
                if w:
                    windows.append(w)
                # Per-model extra limits (e.g. Codex-Spark) -> one window each.
                for extra in (data.get("additional_rate_limits") or []):
                    if not isinstance(extra, dict):
                        continue
                    erl = extra.get("rate_limit") or {}
                    w = _codex_window(erl.get("primary_window"),
                                      extra.get("limit_name") or "Model")
                    if w:
                        windows.append(w)
                if windows:
                    return result("codex", "Codex CLI", "ok", plan=plan,
                                  source="live", windows=windows)
        except urllib.error.HTTPError as e:
            if e.code in (401, 403):
                pass  # token stale -> fall back to local cache below
        except Exception:
            pass

    return _fetch_codex_cache()


def _fetch_codex_cache():
    pattern = os.path.join(HOME, ".codex", "sessions", "**", "rollout-*.jsonl")
    files = glob.glob(pattern, recursive=True)
    if not files:
        return result("codex", "Codex CLI", "unavailable", detail="No codex session files")
    files.sort(key=os.path.getmtime, reverse=True)

    snapshot = None
    plan = None
    # Scan newest files for the most recent rate_limits snapshot.
    for path in files[:8]:
        try:
            with open(path, "r", encoding="utf-8") as fh:
                lines = fh.readlines()
        except Exception:
            continue
        for line in reversed(lines):
            if '"rate_limits"' not in line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            rl = deep_find(obj, {"rate_limits"})
            if isinstance(rl, dict) and (rl.get("primary") or rl.get("secondary")):
                snapshot = rl
                plan = rl.get("plan_type") or plan
                break
        if snapshot:
            break

    if not snapshot:
        return result("codex", "Codex CLI", "unavailable",
                      detail="No rate_limits snapshot yet (run codex once)")

    windows = []
    for slot, default_label, default_period in (
        ("primary", "5-hour", "5h"),
        ("secondary", "Weekly", "weekly"),
    ):
        w = snapshot.get(slot)
        if not isinstance(w, dict):
            continue
        mins = w.get("window_minutes")
        period, label = default_period, default_label
        if mins:
            if mins <= 360:
                period, label = "5h", f"{int(round(mins/60))}-hour"
            elif mins <= 1500:
                period, label = "daily", "Daily"
            elif mins <= 20160:
                period, label = "weekly", "Weekly"
            else:
                period, label = "monthly", "Monthly"
        windows.append(window(label, period,
                              used_percent=w.get("used_percent"),
                              resets_at=w.get("resets_at")))
    return result("codex", "Codex CLI", "ok", plan=plan, source="local-cache",
                  windows=windows)


def _copilot_token():
    """Prefer the Copilot CLI's own account token, then gh, then env.

    The Copilot CLI / IDE may be signed into a different GitHub account than the
    `gh` CLI, so reading gh's token shows the wrong account's quota. Returns
    (token, account_label).
    """
    cfg = os.path.join(HOME, ".copilot", "config.json")
    if os.path.exists(cfg):
        try:
            with open(cfg, "r", encoding="utf-8") as fh:
                # config.json has // comment lines; strip them before parsing.
                text = "\n".join(l for l in fh if not l.lstrip().startswith("//"))
            toks = json.loads(text).get("copilotTokens") or {}
            if isinstance(toks, dict) and toks:
                key, tok = next(iter(toks.items()))
                acct = key.split(":")[-1] if ":" in key else None
                return tok, acct
        except Exception:
            pass
    hosts_candidates = [os.path.join(HOME, ".config", "gh", "hosts.yml")]
    if sys.platform == "win32":
        appdata = os.environ.get("APPDATA") or os.path.join(HOME, "AppData", "Roaming")
        hosts_candidates.insert(0, os.path.join(appdata, "GitHub CLI", "hosts.yml"))
    for hosts in hosts_candidates:
        if os.path.exists(hosts):
            try:
                with open(hosts, "r", encoding="utf-8") as fh:
                    txt = fh.read()
                m = re.search(r"oauth_token:\s*(\S+)", txt)
                if m:
                    u = re.search(r"user:\s*(\S+)", txt)
                    return m.group(1), (u.group(1) if u else None)
            except Exception:
                pass
    env = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    return env, None


def fetch_copilot():
    token, account = _copilot_token()
    if not token:
        return result("copilot", "GitHub Copilot", "unavailable",
                      detail="No GitHub/Copilot token found")

    headers = {
        "Authorization": f"token {token}",
        "User-Agent": "GithubCopilot/1.250.0",
        "Editor-Version": "vscode/1.99.0",
        "Editor-Plugin-Version": "copilot/1.250.0",
        "Accept": "application/json",
    }
    data = None
    for url in ("https://api.github.com/copilot_internal/user",
                "https://api.github.com/copilot_internal/v2/token"):
        try:
            data, _ = http_get(url, headers=headers)
            if isinstance(data, dict):
                break
        except Exception:
            continue

    if not isinstance(data, dict):
        return result("copilot", "GitHub Copilot", "partial", source="schedule",
                      detail="Copilot usage endpoint unreachable; monthly reset only",
                      windows=[window("Premium requests", "monthly",
                                      resets_at=next_month_first_utc())])

    plan = data.get("copilot_plan") or deep_find(data, {"plan"})
    if account:
        plan = f"{plan} · {account}" if plan else account
    reset_at = (to_epoch(data.get("quota_reset_date_utc"))
                or to_epoch(data.get("quota_reset_date"))
                or to_epoch(deep_find(data, {"limited_user_reset_date", "reset_date"}))
                or next_month_first_utc())

    windows = []
    snaps = data.get("quota_snapshots")
    if isinstance(snaps, dict):
        for name, snap in snaps.items():
            if not isinstance(snap, dict):
                continue
            ent = snap.get("entitlement")
            has_quota = snap.get("has_quota", True)
            # Skip buckets this account doesn't actually have (e.g. premium on free).
            if not has_quota and not ent:
                continue
            label = name.replace("_", " ").title()
            if snap.get("unlimited"):
                windows.append(window(label + " (unlimited)", "monthly",
                                      remaining_percent=100.0, resets_at=reset_at))
                continue
            pct = snap.get("percent_remaining")
            rem = snap.get("remaining")
            if rem is None:
                rem = snap.get("quota_remaining")
            rp = float(pct) if pct is not None else (
                round(rem / ent * 100, 1) if (rem is not None and ent) else None)
            used = round(ent - rem) if (ent is not None and rem is not None) else None
            windows.append(window(label, "monthly", remaining_percent=rp,
                                  used=used, limit=ent, resets_at=reset_at))

    if not windows:
        return result("copilot", "GitHub Copilot", "partial", plan=plan,
                      source="schedule",
                      detail="No quota snapshot for this account; monthly reset only",
                      windows=[window("Premium requests", "monthly", resets_at=reset_at)])
    return result("copilot", "GitHub Copilot", "ok", plan=plan, source="live",
                  windows=windows)


def _gemini_project_from_config():
    """GOOGLE_CLOUD_PROJECT as gemini-cli resolves it: env first, then .env files."""
    for path in (os.path.join(HOME, ".gemini", ".env"), os.path.join(HOME, ".env")):
        if not os.path.exists(path):
            continue
        try:
            for line in open(path, encoding="utf-8"):
                line = line.strip()
                if line.startswith("GOOGLE_CLOUD_PROJECT") and "=" in line:
                    val = line.split("=", 1)[1].strip().strip('"').strip("'")
                    if val:
                        return val
        except Exception:
            pass
    return None


def _gemini_post(token, method, body, ua="gemini-cli/usage-monitor"):
    return http_get(GEMINI_API + method,
                    headers={"Authorization": f"Bearer {token}", "User-Agent": ua},
                    data=body, method="POST")


def _proj_id(value):
    """A cloudaicompanionProject can be a bare id or a {id|name} object."""
    if isinstance(value, dict):
        return value.get("id") or value.get("name")
    if isinstance(value, str):
        return value or None
    return None


def _resolve_tier(lca):
    """The account's effective tier: currentTier when present, else the default."""
    if not isinstance(lca, dict):
        return {}
    tier = lca.get("currentTier")
    if not isinstance(tier, dict):
        for t in lca.get("allowedTiers") or []:
            if isinstance(t, dict) and t.get("isDefault"):
                tier = t
                break
    return tier if isinstance(tier, dict) else {}


def _onboard_project(token, tier_id, ua):
    """Provision/return the auto-managed project for a free / personal login.

    onboardUser is a long-running op; poll briefly until it reports done.
    """
    body = {"tierId": tier_id, "metadata": {"pluginType": "GEMINI"}}
    for _ in range(3):
        try:
            lro, _ = _gemini_post(token, "onboardUser", body, ua)
        except Exception:
            return None
        if isinstance(lro, dict) and lro.get("done"):
            return _proj_id(deep_find(lro.get("response") or lro, {"cloudaicompanionProject"}))
        time.sleep(2)
    return None


# Internal Code Assist buckets the official quota UI hides: tab-completion models
# (tab_*) and numbered experimental chat models (chat_<digits>). Skipping them
# keeps the dashboard in sync with the CLI's "Gemini" / "Claude & GPT" groups.
_HIDDEN_MODEL_RE = re.compile(r"^tab_|^chat_\d+$")


def _quota_windows(quota):
    """Map retrieveUserQuota buckets -> per-model windows (the CLI /model bars)."""
    windows = []
    for bucket in (quota.get("buckets") or []) if isinstance(quota, dict) else []:
        if not isinstance(bucket, dict) or not bucket.get("modelId"):
            continue
        if _HIDDEN_MODEL_RE.match(bucket["modelId"]):
            continue
        frac = bucket.get("remainingFraction")
        amount = bucket.get("remainingAmount")
        resets = bucket.get("resetTime")
        if frac is not None:
            windows.append(window(bucket["modelId"], "daily",
                                  remaining_percent=round(float(frac) * 100, 1),
                                  resets_at=resets))
        elif amount is not None:
            # No total exposed -> show remaining count without a percentage bar.
            windows.append(window(bucket["modelId"], "daily",
                                  used=None, limit=None, resets_at=resets))
    return windows


def _rank_models(windows):
    """Per-model gauges sorted tightest fuel first (nulls last), then soonest
    reset. The card previews the first few models and reveals the rest behind a
    "Show all models" toggle, so the most-depleted model always leads."""
    return sorted(windows, key=lambda m: (
        m["remaining_percent"] is None,
        m["remaining_percent"] if m["remaining_percent"] is not None else 0.0,
        m["resets_at"] is None, m["resets_at"] or 0))


def _codeassist_quota(token, hint_project, ua):
    """Shared loadCodeAssist -> retrieveUserQuota flow for Code Assist OAuth tokens.

    Returns ``(status, plan, windows, detail)`` where status is "live" (windows
    populated) or "fallback" (caller shows a reset-clock schedule with `detail`).

    The account's tier decides how the project is resolved, which is what makes
    account switching work:
      * Standard/Enterprise (``userDefinedCloudaicompanionProject``) -> the
        *user-supplied* project (GOOGLE_CLOUD_PROJECT / antigravity gcp.project).
      * Free / personal login -> the project is auto-provisioned; the user-supplied
        one belongs to a *different* account, so we ignore it and onboard instead.

    We deliberately call loadCodeAssist with no project first: passing a stale
    work project while signed into a personal account 403s, which is exactly the
    case we need to support.
    """
    try:
        lca, _ = _gemini_post(token, "loadCodeAssist", {"metadata": {"pluginType": "GEMINI"}}, ua)
    except urllib.error.HTTPError as e:
        if e.code == 401:
            return ("fallback", None, [], "OAuth token expired — run the CLI once to refresh")
        return ("fallback", None, [], f"loadCodeAssist HTTP {e.code}")
    except Exception as e:
        return ("fallback", None, [], f"loadCodeAssist unreachable ({e.__class__.__name__})")

    tier = _resolve_tier(lca)
    plan = tier.get("name") or tier.get("id")
    resp_project = _proj_id(deep_find(lca, {"cloudaicompanionProject"}))

    if tier.get("userDefinedCloudaicompanionProject"):
        project = hint_project or resp_project
        if not project:
            return ("fallback", plan, [],
                    "Standard/Enterprise tier needs a project (set GOOGLE_CLOUD_PROJECT)")
    else:
        # Personal / free account: ignore any user-supplied project (it's another
        # account's) and use the auto-provisioned one.
        project = resp_project or _onboard_project(token, tier.get("id"), ua)
        if not project:
            return ("fallback", plan, [], "Could not resolve free-tier project (onboardUser)")

    try:
        quota, _ = _gemini_post(token, "retrieveUserQuota", {"project": project}, ua)
    except urllib.error.HTTPError as e:
        note = "no Code Assist license" if e.code == 403 else f"HTTP {e.code}"
        return ("fallback", plan, [], f"retrieveUserQuota {note}")
    except Exception as e:
        return ("fallback", plan, [], f"retrieveUserQuota unreachable ({e.__class__.__name__})")

    windows = _quota_windows(quota)
    if not windows:
        return ("fallback", plan, [], "Quota returned no model buckets")
    return ("live", plan, windows, None)


def _google_oauth_refresh(client_id, client_secret, refresh_token):
    """Exchange a Google refresh_token for a fresh token (the startup exchange the
    CLIs run). Returns the parsed token response dict, or None on failure.

    Google returns a new access_token/id_token/expires_in but reuses the existing
    refresh_token, so callers merge the result into their stored creds.
    """
    if not refresh_token:
        return None
    body = urllib.parse.urlencode({
        "client_id": client_id,
        "client_secret": client_secret,
        "refresh_token": refresh_token,
        "grant_type": "refresh_token",
    }).encode("utf-8")
    req = urllib.request.Request(
        GOOGLE_TOKEN_URI, data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:
            return json.loads(resp.read().decode("utf-8", "replace"))
    except Exception:
        return None


def _refresh_gemini_token(creds, path):
    """Mint a fresh access token from the stored refresh_token and write it back
    to `path` -- the same OAuth exchange gemini-cli does on startup. Returns the
    new access token, or None if there's no refresh_token or the exchange fails.
    """
    refresh = creds.get("refresh_token") if isinstance(creds, dict) else None
    tok = _google_oauth_refresh(GEMINI_CLI_PUBLIC_CLIENT_ID, GEMINI_CLI_PUBLIC_CLIENT_SECRET, refresh)
    if not tok:
        return None
    access = tok.get("access_token")
    if not access:
        return None
    creds["access_token"] = access
    for src, dst in (("id_token", "id_token"), ("token_type", "token_type"), ("scope", "scope")):
        if tok.get(src):
            creds[dst] = tok[src]
    if tok.get("expires_in"):
        creds["expiry_date"] = int(now_ts() * 1000) + int(tok["expires_in"]) * 1000
    try:
        write_json_atomic(path, creds)
    except Exception:
        pass  # the in-memory token is still usable even if we can't persist it
    return access


def _gemini_schedule(plan, detail):
    """Fallback: tier name + daily reset clock (no per-model quota available)."""
    return result("gemini", "Gemini CLI", "partial", plan=plan, source="schedule",
                  detail=detail,
                  windows=[window("Daily", "daily", resets_at=next_midnight_pacific())])


def fetch_gemini():
    """Live: loadCodeAssist (tier + project) -> retrieveUserQuota (per-model bars).

    Works for both a Standard/Enterprise account (user-supplied GOOGLE_CLOUD_PROJECT)
    and a free / personal login (project auto-provisioned via onboardUser). The
    ~1h access token is refreshed in-place from its refresh_token (like the CLI)
    so an idle CLI no longer drops us to the reset-clock fallback.
    """
    cred = os.path.join(HOME, ".gemini", "oauth_creds.json")
    if not os.path.exists(cred):
        return result("gemini", "Gemini CLI", "unavailable",
                      detail="No ~/.gemini/oauth_creds.json")
    try:
        creds = read_json(cred)
    except Exception:
        creds = None
    token = deep_find(creds, {"access_token", "accessToken"}) if creds else None
    if not token:
        return result("gemini", "Gemini CLI", "unavailable",
                      detail="No access token in oauth_creds.json")

    # Proactively refresh the cached token when it's expired (or within a minute
    # of it) instead of letting loadCodeAssist 401 us into the fallback.
    refreshed = False
    expiry = to_epoch(creds.get("expiry_date"))
    if expiry is not None and expiry <= now_ts() + 60:
        new_token = _refresh_gemini_token(creds, cred)
        if new_token:
            token, refreshed = new_token, True

    # Only meaningful for the user-supplied (Standard/Enterprise) case; the helper
    # ignores it for personal logins.
    env_project = os.environ.get("GOOGLE_CLOUD_PROJECT") or _gemini_project_from_config()
    status, plan, windows, detail = _codeassist_quota(token, env_project, "gemini-cli/usage-monitor")

    # If we didn't already refresh and the token was rejected (stale expiry_date),
    # force one refresh and retry before degrading to the reset clock.
    if status != "live" and not refreshed and creds.get("refresh_token"):
        new_token = _refresh_gemini_token(creds, cred)
        if new_token and new_token != token:
            status, plan, windows, detail = _codeassist_quota(
                new_token, env_project, "gemini-cli/usage-monitor")

    if status == "live":
        # Rank the per-model buckets tightest-first; the card previews the top
        # few and reveals the rest behind "Show all models".
        return result("gemini", "Gemini CLI", "ok", plan=plan, source="live",
                      windows=_rank_models(windows))
    return _gemini_schedule(plan, f"{detail}; showing daily reset window")


def _antigravity_token(path):
    """(creds, access_token, expired) from antigravity-cli's own OAuth token file.

    The token is nested under `token` ({access_token, refresh_token, expiry});
    the whole `creds` dict is returned so the caller can refresh + write it back.
    """
    if not os.path.exists(path):
        return None, None, True
    try:
        creds = read_json(path)
    except Exception:
        return None, None, True
    token = deep_find(creds, {"access_token", "accessToken"})
    exp = to_epoch(deep_find(creds, {"expiry", "expiry_date", "expires_at"}))
    expired = exp is not None and exp <= now_ts()
    return creds, token, expired


def _refresh_antigravity_token(creds, path):
    """Mint a fresh access token for antigravity-cli from its stored refresh_token
    and write it back to `path` -- the same OAuth exchange `agy` runs on startup.

    Preserves the file's nested {token:{...}} shape and writes expiry back as the
    RFC3339 string the CLI uses (not Gemini's ms epoch). Returns the new access
    token, or None if there's no refresh_token or the exchange fails.
    """
    tokobj = creds.get("token") if isinstance(creds, dict) else None
    refresh = tokobj.get("refresh_token") if isinstance(tokobj, dict) else None
    tok = _google_oauth_refresh(ANTIGRAVITY_CLI_PUBLIC_CLIENT_ID,
                                ANTIGRAVITY_CLI_PUBLIC_CLIENT_SECRET, refresh)
    if not tok:
        return None
    access = tok.get("access_token")
    if not access:
        return None
    tokobj["access_token"] = access
    if tok.get("token_type"):
        tokobj["token_type"] = tok["token_type"]
    if tok.get("expires_in"):
        exp = datetime.now(timezone.utc) + timedelta(seconds=int(tok["expires_in"]))
        tokobj["expiry"] = exp.isoformat()
    try:
        write_json_atomic(path, creds)
    except Exception:
        pass  # the in-memory token is still usable even if we can't persist it
    return access


def _antigravity_project():
    """gcp.project from antigravity-cli/settings.json (absent for a personal login)."""
    path = os.path.join(HOME, ".gemini", "antigravity-cli", "settings.json")
    try:
        gcp = read_json(path).get("gcp")
    except Exception:
        return None
    return gcp.get("project") if isinstance(gcp, dict) else None


def fetch_antigravity():
    base = os.path.join(HOME, ".gemini")
    dirs = [os.path.join(base, "antigravity"), os.path.join(base, "antigravity-cli")]
    present = any(os.path.isdir(d) for d in dirs)
    if not present:
        return result("antigravity", "Antigravity CLI", "unavailable",
                      detail="No ~/.gemini/antigravity* directory")

    # Prefer a live read with antigravity-cli's own OAuth token. Same Code Assist
    # flow as Gemini, so it handles a GCP/work account *and* a personal login (no
    # gcp.project). The ~1h token is refreshed in-place from its refresh_token
    # (like `agy` on startup) so an idle CLI no longer drops to the schedule below.
    tok_path = os.path.join(base, "antigravity-cli", "antigravity-oauth-token")
    creds, token, expired = _antigravity_token(tok_path)

    # Proactively refresh an expired (or unparseable-but-missing) token before it
    # 401s us into the fallback.
    refreshed = False
    if creds and (expired or not token):
        new_token = _refresh_antigravity_token(creds, tok_path)
        if new_token:
            token, expired, refreshed = new_token, False, True

    live_detail = None
    if token and not expired:
        status, plan, windows, live_detail = _codeassist_quota(
            token, _antigravity_project(), "antigravity/usage-monitor")
        # Token rejected despite a fresh-looking expiry -> force one refresh + retry.
        if status != "live" and not refreshed and creds:
            new_token = _refresh_antigravity_token(creds, tok_path)
            if new_token and new_token != token:
                refreshed = True
                status, plan, windows, live_detail = _codeassist_quota(
                    new_token, _antigravity_project(), "antigravity/usage-monitor")
        if status == "live":
            # agy exposes one bucket per model (Gemini, Claude, GPT, ...); rank
            # them tightest-first so the card can preview one per family.
            return result("antigravity", "Antigravity CLI", "ok", plan=plan,
                          source="live", windows=_rank_models(windows))

    # Best-effort: scan small json caches for any usage/quota snapshot.
    for d in dirs:
        for path in glob.glob(os.path.join(d, "**", "*.json"), recursive=True)[:400]:
            try:
                if os.path.getsize(path) > 200_000:
                    continue
                obj = read_json(path)
            except Exception:
                continue
            up = deep_find(obj, {"used_percent", "usedPercent"})
            resets = deep_find(obj, {"resets_at", "reset_at", "quota_reset_date"})
            if up is not None or resets is not None:
                return result("antigravity", "Antigravity CLI", "ok",
                              source="local-cache",
                              windows=[window("Usage", "weekly",
                                              used_percent=float(up) if up is not None else None,
                                              resets_at=resets)])

    if token and expired:
        detail = "OAuth token expired and auto-refresh failed — run antigravity once; quota resets ~every 5h"
    elif live_detail:
        detail = f"{live_detail}; quota resets ~every 5h"
    else:
        detail = "No public/local usage data yet; quota resets ~every 5h"
    return result("antigravity", "Antigravity CLI", "partial", source="schedule",
                  detail=detail,
                  windows=[window("Model quota", "5h",
                                  resets_at=now_ts() + 5 * 3600)])


PROVIDERS = [
    ("claude", fetch_claude, 180),       # claude oauth/usage 429s if polled fast
    ("codex", fetch_codex, 30),
    ("copilot", fetch_copilot, 120),
    ("gemini", fetch_gemini, 120),
    ("antigravity", fetch_antigravity, 60),
]

def effective_reset(res):
    """The single reset time a provider is ranked by.

    Priority is the weekly/monthly window: when a provider has one we sort by its
    soonest weekly/monthly reset, even if a shorter 5h/daily window resets sooner.
    A provider with no weekly/monthly window (e.g. Gemini, which only resets daily)
    falls back to its soonest reset of any kind, so it still ranks by when it
    actually frees up instead of sinking below far-off weekly resets.
    """
    weekly_monthly = [w["resets_at"] for w in res["windows"]
                      if w["resets_at"] and w["period"] in ("weekly", "monthly")]
    if weekly_monthly:
        return min(weekly_monthly)
    any_window = [w["resets_at"] for w in res["windows"] if w["resets_at"]]
    return min(any_window) if any_window else None


# ---------------------------------------------------------------------------
# Caching + aggregation
# ---------------------------------------------------------------------------

_cache = {}            # key -> (expires_at, result)
_cache_lock = threading.Lock()


def get_provider(key, fn, ttl, force=False):
    with _cache_lock:
        hit = _cache.get(key)
        if hit and not force and hit[0] > now_ts():
            return hit[1]
    try:
        res = fn()
    except Exception as e:
        res = result(key, key.title(), "error", detail=f"{e.__class__.__name__}: {e}")
    with _cache_lock:
        _cache[key] = (now_ts() + ttl, res)
    return res


def collect(force=False):
    results = []
    threads = []
    out = {}

    def run(key, fn, ttl):
        out[key] = get_provider(key, fn, ttl, force=force)

    for key, fn, ttl in PROVIDERS:
        t = threading.Thread(target=run, args=(key, fn, ttl))
        t.start()
        threads.append(t)
    for t in threads:
        t.join()

    for key, _, _ in PROVIDERS:
        res = out[key]
        res["reset_at"] = effective_reset(res)
        results.append(res)

    # Rank by soonest reset: the weekly/monthly window when a provider has one,
    # otherwise its soonest window of any kind (e.g. Gemini's daily reset).
    # Providers with no reset clock at all sink to the bottom.
    far = float("inf")
    results.sort(key=lambda r: r["reset_at"] if r["reset_at"] else far)
    return {"generated_at": now_ts(), "providers": results}


# ---------------------------------------------------------------------------
# Terminal renderer (--text)
# ---------------------------------------------------------------------------

_ANSI = {
    "reset": "\033[0m", "bold": "\033[1m",
    "green": "\033[32m", "yellow": "\033[33m", "red": "\033[31m", "grey": "\033[90m",
}
_STATUS_COLOR = {"ok": "green", "partial": "yellow", "unavailable": "grey", "error": "red"}


def _fmt_countdown(secs):
    """Human countdown to a reset, mirroring the web dashboard's fmtCountdown."""
    if secs is None:
        return "no reset"
    secs = int(secs)
    if secs <= 0:
        return "resetting…"
    d, rem = divmod(secs, 86400)
    h, rem = divmod(rem, 3600)
    m, s = divmod(rem, 60)
    if d:
        return f"{d}d {h}h {m}m"
    if h:
        return f"{h}h {m}m"
    return f"{m}m {s}s"


def _rem_color(rem):
    if rem is None:
        return "grey"
    if rem <= 10:
        return "red"
    if rem <= 30:
        return "yellow"
    return "green"


def render_text(data, color=True):
    """Compact, colored one-screen summary of `collect()` for the terminal."""
    def paint(code, text):
        return f"{_ANSI[code]}{text}{_ANSI['reset']}" if (color and code in _ANSI) else text

    providers = data.get("providers", [])
    # Align model labels to the widest one, capped so a long preview id can't blow
    # out the column.
    labels = [w["label"] for p in providers for w in p["windows"]]
    width = min(max((len(l) for l in labels), default=10), 28)

    now = now_ts()
    updated = datetime.fromtimestamp(data["generated_at"]).strftime("%H:%M:%S")
    out = [paint("bold", "aifuel")
           + paint("grey", f"   updated {updated} · ranked by soonest reset")]

    for i, p in enumerate(providers, 1):
        src = ("live" if p["source"] == "live"
               else "cache" if p["source"] == "local-cache"
               else p["source"] or "—")
        meta = " · ".join(x for x in (p.get("plan"), src, p["status"]) if x)
        dot = paint(_STATUS_COLOR.get(p["status"], "grey"), "●")
        out.append("")
        out.append(f"{i}. {dot} {paint('bold', p['name'])}  {paint('grey', meta)}")

        if not p["windows"]:
            out.append(f"     {paint('grey', p.get('detail') or 'no usage data')}")
            continue
        if p.get("detail"):
            out.append(f"     {paint('grey', p['detail'])}")

        for w in p["windows"]:
            rem = w["remaining_percent"]
            rc = _rem_color(rem)
            filled = 12 if rem is None else max(0, min(12, round(rem / 100 * 12)))
            bar = paint(rc, "█" * filled) + paint("grey", "░" * (12 - filled))
            rem_txt = " n/a" if rem is None else f"{round(rem):3d}%"
            label = w["label"]
            if len(label) > width:
                label = label[: width - 1] + "…"
            cap = f" · {w['limit']} cap" if w.get("limit") is not None else ""
            secs = (w["resets_at"] - now) if w["resets_at"] else None
            tail = f"↻ {_fmt_countdown(secs)} {w['period']}"
            out.append(f"     {label:<{width}}  {bar}  "
                       f"{paint(rc, rem_txt)}{paint('grey', cap)}  {paint('grey', tail)}")
    return "\n".join(out)


# ---------------------------------------------------------------------------
# Web server
# ---------------------------------------------------------------------------


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # quiet
        pass

    def _send(self, code, body, ctype):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/":
            try:
                with open(HTML_PATH, "r", encoding="utf-8") as fh:
                    body = fh.read()
                self._send(200, body, "text/html; charset=utf-8")
            except OSError as e:
                self._send(500, f"index.html not found: {e}", "text/plain")
        elif path == "/api/usage":
            force = "force=1" in self.path
            payload = json.dumps(collect(force=force), default=str)
            self._send(200, payload, "application/json")
        else:
            self._send(404, "not found", "text/plain")


def main():
    ap = argparse.ArgumentParser(description="aifuel — fuel gauge for your AI coding subscriptions")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--json", action="store_true", help="print usage JSON and exit")
    ap.add_argument("--text", action="store_true",
                    help="print a compact colored summary to the terminal and exit")
    ap.add_argument("--no-color", action="store_true", help="disable --text colors")
    ap.add_argument("--open", action="store_true", help="open browser on start")
    args = ap.parse_args()

    if args.json:
        print(json.dumps(collect(force=True), indent=2, default=str))
        return

    if args.text:
        color = (not args.no_color
                 and sys.stdout.isatty()
                 and os.environ.get("NO_COLOR") is None)
        print(render_text(collect(force=True), color=color))
        return

    url = f"http://{args.host}:{args.port}"
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"aifuel → {url}")
    print("Ordered by nearest weekly/monthly reset.  Ctrl-C to stop.")
    if args.open:
        threading.Timer(0.6, lambda: webbrowser.open(url)).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nbye")
        server.shutdown()


if __name__ == "__main__":
    main()
