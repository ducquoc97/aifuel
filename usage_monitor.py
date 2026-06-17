#!/usr/bin/env python3
"""
AI CLI Usage Monitor
====================

Single-file dashboard that shows the *remaining* subscription quota for the AI
coding CLIs you use, ordered by whichever weekly / monthly window resets soonest.

Providers:
    - Claude Code      (live  : api.anthropic.com/api/oauth/usage)
    - Codex CLI        (live  : chatgpt.com/backend-api/codex/usage; cache fallback)
    - GitHub Copilot   (live  : api.github.com/copilot_internal/v2/token  + fallback)
    - Gemini CLI       (live  : loadCodeAssist -> retrieveUserQuota; daily-reset fallback)
    - Antigravity CLI  (live  : Code Assist quota via its own token; scan + schedule fallback)

Nothing here prints or transmits your tokens. Credential files are read locally
only to authenticate the provider's own usage endpoint, exactly like the CLIs do.

Usage:
    python3 usage_monitor.py            # serve dashboard at http://127.0.0.1:8787
    python3 usage_monitor.py --json     # print the usage JSON and exit
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
from datetime import datetime, timezone, timedelta
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOME = os.path.expanduser("~")
HTTP_TIMEOUT = 12  # seconds

# Live usage endpoints (read-only; authenticated with the CLI's own local token).
CODEX_USAGE_URL = "https://chatgpt.com/backend-api/codex/usage"
GEMINI_API = "https://cloudcode-pa.googleapis.com/v1internal:"  # + loadCodeAssist | retrieveUserQuota

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
    hosts = os.path.join(HOME, ".config", "gh", "hosts.yml")
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


def _quota_windows(quota):
    """Map retrieveUserQuota buckets -> per-model windows (the CLI /model bars)."""
    windows = []
    for bucket in (quota.get("buckets") or []) if isinstance(quota, dict) else []:
        if not isinstance(bucket, dict) or not bucket.get("modelId"):
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


def _gemini_schedule(plan, detail):
    """Fallback: tier name + daily reset clock (no per-model quota available)."""
    return result("gemini", "Gemini CLI", "partial", plan=plan, source="schedule",
                  detail=detail,
                  windows=[window("Daily", "daily", resets_at=next_midnight_pacific())])


def fetch_gemini():
    """Live: loadCodeAssist (tier + project) -> retrieveUserQuota (per-model bars).

    Works for both a Standard/Enterprise account (user-supplied GOOGLE_CLOUD_PROJECT)
    and a free / personal login (project auto-provisioned via onboardUser). When no
    live quota is available we fall back to the tier name + daily reset clock.
    """
    cred = os.path.join(HOME, ".gemini", "oauth_creds.json")
    if not os.path.exists(cred):
        return result("gemini", "Gemini CLI", "unavailable",
                      detail="No ~/.gemini/oauth_creds.json")
    try:
        token = deep_find(read_json(cred), {"access_token", "accessToken"})
    except Exception:
        token = None
    if not token:
        return result("gemini", "Gemini CLI", "unavailable",
                      detail="No access token in oauth_creds.json")

    # Only meaningful for the user-supplied (Standard/Enterprise) case; the helper
    # ignores it for personal logins.
    env_project = os.environ.get("GOOGLE_CLOUD_PROJECT") or _gemini_project_from_config()
    status, plan, windows, detail = _codeassist_quota(token, env_project, "gemini-cli/usage-monitor")
    if status == "live":
        return result("gemini", "Gemini CLI", "ok", plan=plan, source="live", windows=windows)
    return _gemini_schedule(plan, f"{detail}; showing daily reset window")


def _antigravity_token(path):
    """(access_token, expired) from antigravity-cli's own OAuth token file."""
    if not os.path.exists(path):
        return None, True
    try:
        data = read_json(path)
    except Exception:
        return None, True
    token = deep_find(data, {"access_token", "accessToken"})
    exp = to_epoch(deep_find(data, {"expiry", "expiry_date", "expires_at"}))
    expired = exp is not None and exp <= now_ts()
    return token, expired


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
    # gcp.project). The token can't be refreshed here, so an expired one (or any
    # endpoint mismatch) just degrades to the cache scan / schedule below.
    token, expired = _antigravity_token(os.path.join(base, "antigravity-cli", "antigravity-oauth-token"))
    live_detail = None
    if token and not expired:
        status, plan, windows, live_detail = _codeassist_quota(
            token, _antigravity_project(), "antigravity/usage-monitor")
        if status == "live":
            return result("antigravity", "Antigravity CLI", "ok", plan=plan,
                          source="live", windows=windows)

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
        detail = "OAuth token expired — run antigravity once to refresh; quota resets ~every 5h"
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

_RANK = {"weekly": 0, "monthly": 0, "daily": 1, "5h": 2, "unknown": 3}


def primary_reset(res):
    """Soonest reset among weekly/monthly windows (the user's primary sort key)."""
    candidates = [w["resets_at"] for w in res["windows"]
                  if w["resets_at"] and w["period"] in ("weekly", "monthly")]
    return min(candidates) if candidates else None


def any_reset(res):
    """Soonest reset among all windows (tiebreak for providers w/o weekly/monthly)."""
    candidates = [w["resets_at"] for w in res["windows"] if w["resets_at"]]
    return min(candidates) if candidates else None


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
        res["primary_reset_at"] = primary_reset(res)
        res["any_reset_at"] = any_reset(res)
        results.append(res)

    # Weekly/monthly resets lead (sorted by soonest); providers without a
    # weekly/monthly window follow, ordered by their soonest window of any kind.
    far = float("inf")
    results.sort(key=lambda r: (r["primary_reset_at"] is None,
                                r["primary_reset_at"] if r["primary_reset_at"] else far,
                                r["any_reset_at"] if r["any_reset_at"] else far))
    return {"generated_at": now_ts(), "providers": results}


# ---------------------------------------------------------------------------
# Web server
# ---------------------------------------------------------------------------

INDEX_HTML = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>AI CLI Usage Monitor</title>
<style>
  :root{
    --bg:#0b0d12; --card:#151922; --card2:#1b212d; --line:#262d3a;
    --txt:#e7ecf3; --dim:#8a94a6; --accent:#6ea8fe;
    --good:#3ddc97; --warn:#ffb454; --bad:#ff6b6b;
  }
  *{box-sizing:border-box}
  body{margin:0;background:radial-gradient(1200px 600px at 80% -10%,#16203a 0,var(--bg) 55%);
       color:var(--txt);font:15px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Inter,sans-serif;}
  header{padding:28px 24px 8px;max-width:1100px;margin:0 auto;}
  h1{margin:0;font-size:22px;letter-spacing:.2px}
  .sub{color:var(--dim);font-size:13px;margin-top:6px}
  .grid{max-width:1100px;margin:18px auto 60px;padding:0 24px;
        display:grid;grid-template-columns:repeat(auto-fill,minmax(330px,1fr));gap:16px;}
  .card{background:linear-gradient(180deg,var(--card),var(--card2));border:1px solid var(--line);
        border-radius:16px;padding:18px 18px 16px;position:relative;overflow:hidden}
  .rank{position:absolute;top:14px;right:16px;font-size:11px;color:var(--dim)}
  .pname{font-size:16px;font-weight:650;display:flex;align-items:center;gap:9px}
  .dot{width:9px;height:9px;border-radius:50%;flex:none}
  .badges{margin-top:8px;display:flex;gap:6px;flex-wrap:wrap}
  .badge{font-size:11px;padding:2px 8px;border-radius:999px;border:1px solid var(--line);
         color:var(--dim);background:#0f131b}
  .badge.live{color:var(--good);border-color:#1e5e44}
  .badge.cache{color:var(--accent);border-color:#27496f}
  .badge.schedule{color:var(--warn);border-color:#6b4f1f}
  .detail{color:var(--dim);font-size:12px;margin-top:10px;font-style:italic}
  .win{margin-top:14px}
  .win .top{display:flex;justify-content:space-between;align-items:baseline;font-size:13px}
  .win .lbl{color:var(--txt)}
  .win .rem{font-weight:680}
  .bar{height:9px;border-radius:6px;background:#0c1017;margin-top:7px;overflow:hidden;border:1px solid #0a0e15}
  .fill{height:100%;border-radius:6px;transition:width .5s ease}
  .reset{display:flex;justify-content:space-between;margin-top:6px;font-size:11.5px;color:var(--dim)}
  .cd{font-variant-numeric:tabular-nums;color:var(--txt)}
  .pill{font-size:10px;padding:1px 7px;border-radius:999px;border:1px solid var(--line);color:var(--dim)}
  .empty{color:var(--dim);font-size:13px;margin-top:12px}
  footer{max-width:1100px;margin:0 auto;padding:0 24px 40px;color:var(--dim);font-size:12px}
  .reload{cursor:pointer;color:var(--accent);text-decoration:none}
  .err{color:var(--bad)}
</style>
</head>
<body>
<header>
  <h1>AI CLI Usage Monitor</h1>
  <div class="sub">Remaining subscription quota, ordered by nearest <b>weekly / monthly</b> reset ·
     <span id="updated">loading…</span> · <a class="reload" onclick="load(true)">refresh</a></div>
</header>
<div class="grid" id="grid"></div>
<footer>
  Auto-refreshes every 60s. <b>live</b> = pulled from the provider API ·
  <b>cache</b> = read from the CLI's local snapshot · <b>schedule</b> = reset clock only
  (provider exposes no usage number).
</footer>

<script>
const STATUS_COLOR = {ok:"var(--good)", partial:"var(--warn)", unavailable:"var(--dim)", error:"var(--bad)"};
const PERIOD_LABEL = {"5h":"5-hour","daily":"daily","weekly":"weekly","monthly":"monthly","unknown":"window"};
let DATA = null;

function fmtCountdown(sec){
  if(sec===null||sec===undefined) return "—";
  if(sec<=0) return "resetting…";
  const d=Math.floor(sec/86400), h=Math.floor(sec%86400/3600),
        m=Math.floor(sec%3600/60), s=Math.floor(sec%60);
  if(d>0) return `${d}d ${h}h ${m}m`;
  if(h>0) return `${h}h ${m}m ${s}s`;
  return `${m}m ${s}s`;
}
function fmtDate(ts){
  if(!ts) return "—";
  const dt=new Date(ts*1000);
  return dt.toLocaleString([], {month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"});
}
function barColor(rem){
  if(rem===null) return "#3a4356";
  if(rem<=10) return "var(--bad)";
  if(rem<=30) return "var(--warn)";
  return "var(--good)";
}

function render(){
  if(!DATA) return;
  const grid=document.getElementById("grid");
  grid.innerHTML="";
  DATA.providers.forEach((p,i)=>{
    const card=document.createElement("div");
    card.className="card";
    const srcClass = p.source==="live"?"live":(p.source==="local-cache"?"cache":"schedule");
    const srcText  = p.source==="live"?"live":(p.source==="local-cache"?"cache":(p.source||"—"));
    let wins="";
    if(p.windows && p.windows.length){
      p.windows.forEach(w=>{
        const rem = (w.remaining_percent!==null && w.remaining_percent!==undefined)? w.remaining_percent : null;
        const width = rem===null? 100 : Math.max(2,Math.min(100,rem));
        const col = barColor(rem);
        const remTxt = rem===null? "n/a" : rem.toFixed(0)+"%";
        const limTxt = (w.limit!=null)? ` · ${w.limit} cap` : "";
        wins += `
         <div class="win">
           <div class="top">
             <span class="lbl">${w.label} <span class="pill">${PERIOD_LABEL[w.period]||w.period}</span></span>
             <span class="rem" style="color:${col}">${remTxt} left${limTxt}</span>
           </div>
           <div class="bar"><div class="fill" style="width:${width}%;background:${col};${rem===null?'opacity:.25':''}"></div></div>
           <div class="reset">
             <span class="cd" data-reset="${w.resets_at||''}">renews in —</span>
             <span>${fmtDate(w.resets_at)}</span>
           </div>
         </div>`;
      });
    } else {
      wins = `<div class="empty">${p.detail||"No data"}</div>`;
    }
    card.innerHTML = `
      <div class="rank">#${i+1} · soonest reset</div>
      <div class="pname"><span class="dot" style="background:${STATUS_COLOR[p.status]||'#888'}"></span>${p.name}</div>
      <div class="badges">
        ${p.plan?`<span class="badge">${p.plan}</span>`:""}
        <span class="badge ${srcClass}">${srcText}</span>
        <span class="badge">${p.status}</span>
      </div>
      ${p.detail && p.windows && p.windows.length?`<div class="detail">${p.detail}</div>`:""}
      ${wins}`;
    grid.appendChild(card);
  });
  tick();
}

function tick(){
  const now=Date.now()/1000;
  document.querySelectorAll(".cd").forEach(el=>{
    const r=parseFloat(el.dataset.reset);
    el.textContent = r? ("renews in "+fmtCountdown(r-now)) : "no reset clock";
  });
}

async function load(force){
  try{
    const r=await fetch("/api/usage"+(force?"?force=1":""));
    DATA=await r.json();
    const ago=new Date(DATA.generated_at*1000).toLocaleTimeString();
    document.getElementById("updated").textContent="updated "+ago;
    render();
  }catch(e){
    document.getElementById("updated").innerHTML='<span class="err">fetch failed: '+e+'</span>';
  }
}
load();
setInterval(tick,1000);
setInterval(()=>load(false),60000);
</script>
</body>
</html>
"""


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
            self._send(200, INDEX_HTML, "text/html; charset=utf-8")
        elif path == "/api/usage":
            force = "force=1" in self.path
            payload = json.dumps(collect(force=force), default=str)
            self._send(200, payload, "application/json")
        else:
            self._send(404, "not found", "text/plain")


def main():
    ap = argparse.ArgumentParser(description="AI CLI usage monitor")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--json", action="store_true", help="print usage JSON and exit")
    ap.add_argument("--open", action="store_true", help="open browser on start")
    args = ap.parse_args()

    if args.json:
        print(json.dumps(collect(force=True), indent=2, default=str))
        return

    url = f"http://{args.host}:{args.port}"
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"AI CLI Usage Monitor → {url}")
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
