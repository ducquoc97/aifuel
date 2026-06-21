from __future__ import annotations

import json
import os
import re
import time
import urllib.error
import urllib.parse
import urllib.request

from .. import shared


def _gemini_project_from_config():
    """GOOGLE_CLOUD_PROJECT as gemini-cli resolves it: env first, then .env files."""
    for path in (os.path.join(shared.HOME, ".gemini", ".env"), os.path.join(shared.HOME, ".env")):
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
    return shared.http_get(
        shared.GEMINI_API + method,
        headers={"Authorization": f"Bearer {token}", "User-Agent": ua},
        data=body,
        method="POST",
    )


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
    """Provision/return the auto-managed project for a free / personal login."""
    body = {"tierId": tier_id, "metadata": {"pluginType": "GEMINI"}}
    for _ in range(3):
        try:
            lro, _ = _gemini_post(token, "onboardUser", body, ua)
        except Exception:
            return None
        if isinstance(lro, dict) and lro.get("done"):
            return _proj_id(shared.deep_find(lro.get("response") or lro, {"cloudaicompanionProject"}))
        time.sleep(2)
    return None


# Internal Code Assist buckets the official quota UI hides: tab-completion models
# (tab_*) and numbered experimental chat models (chat_<digits>). Skipping them
# keeps the dashboard in sync with the CLI's "Gemini" / "Claude & GPT" groups.
_HIDDEN_MODEL_RE = re.compile(r"^tab_|^chat_\d+$")

# Model ID translations from CCPA/Code Assist names to canonical Gemini CLI display names.
_MODEL_MAPPING = {
    "gemini-3-flash": "gemini-3.5-flash",
    "gemini-3.1-pro-preview-customtools": "gemini-3.1-pro-preview",
}


def _quota_windows(quota, period_override=None):
    """Map retrieveUserQuota buckets -> per-model windows (the CLI /model bars)."""
    windows = []
    for bucket in (quota.get("buckets") or []) if isinstance(quota, dict) else []:
        if not isinstance(bucket, dict) or not bucket.get("modelId"):
            continue
        if _HIDDEN_MODEL_RE.match(bucket["modelId"]):
            continue
        model_id = bucket["modelId"]
        display_id = _MODEL_MAPPING.get(model_id, model_id)
        frac = bucket.get("remainingFraction")
        amount = bucket.get("remainingAmount")
        resets = bucket.get("resetTime")
        reset_epoch = shared.to_epoch(resets)
        if period_override:
            period = period_override
        else:
            secs_until = (reset_epoch - shared.now_ts()) if reset_epoch else None
            period, _ = (
                shared.period_for_seconds(secs_until)
                if secs_until and secs_until > 0
                else ("daily", "Daily")
            )
        if frac is not None:
            windows.append(shared.window(display_id, period,
                                         remaining_percent=round(float(frac) * 100, 1),
                                         resets_at=resets))
        elif amount is not None:
            # No total exposed -> show remaining count without a percentage bar.
            windows.append(shared.window(display_id, period,
                                         used=None, limit=None, resets_at=resets))
    return windows


def _rank_models(windows):
    """Per-model gauges sorted tightest fuel first (nulls last), then soonest reset."""
    return sorted(windows, key=lambda m: (
        m["remaining_percent"] is None,
        m["remaining_percent"] if m["remaining_percent"] is not None else 0.0,
        m["resets_at"] is None, m["resets_at"] or 0,
    ))


def _codeassist_quota(token, hint_project, ua, period_override=None):
    """Shared loadCodeAssist -> retrieveUserQuota flow for Code Assist OAuth tokens."""
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
    resp_project = _proj_id(shared.deep_find(lca, {"cloudaicompanionProject"}))

    if tier.get("userDefinedCloudaicompanionProject"):
        project = hint_project or resp_project
        if not project:
            return ("fallback", plan, [],
                    "Standard/Enterprise tier needs a project (set GOOGLE_CLOUD_PROJECT)")
    else:
        # Personal / free account: ignore any user-supplied project and use the
        # auto-provisioned one.
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

    windows = _quota_windows(quota, period_override=period_override)
    if not windows:
        return ("fallback", plan, [], "Quota returned no model buckets")
    return ("live", plan, windows, None)


def _google_oauth_refresh(client_id, client_secret, refresh_token):
    """Exchange a Google refresh_token for a fresh token response."""
    if not refresh_token:
        return None
    body = urllib.parse.urlencode({
        "client_id": client_id,
        "client_secret": client_secret,
        "refresh_token": refresh_token,
        "grant_type": "refresh_token",
    }).encode("utf-8")
    req = urllib.request.Request(
        shared.GOOGLE_TOKEN_URI, data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"}, method="POST")
    try:
        with shared._urlopen(req, timeout=shared.HTTP_TIMEOUT) as resp:
            return json.loads(resp.read().decode("utf-8", "replace"))
    except Exception:
        return None


def _refresh_gemini_token(creds, path):
    """Mint a fresh access token from the stored refresh_token and write it back."""
    refresh = creds.get("refresh_token") if isinstance(creds, dict) else None
    tok = _google_oauth_refresh(
        shared.GEMINI_CLI_PUBLIC_CLIENT_ID,
        shared.GEMINI_CLI_PUBLIC_CLIENT_SECRET,
        refresh,
    )
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
        creds["expiry_date"] = int(shared.now_ts() * 1000) + int(tok["expires_in"]) * 1000
    try:
        shared.write_json_atomic(path, creds)
    except Exception:
        pass  # the in-memory token is still usable even if we can't persist it
    return access


def fetch_gemini():
    """Live: loadCodeAssist (tier + project) -> retrieveUserQuota (per-model bars)."""
    cred = os.path.join(shared.HOME, ".gemini", "oauth_creds.json")
    if not os.path.exists(cred):
        return shared.result("gemini", "Gemini CLI", "error",
                             detail="No ~/.gemini/oauth_creds.json")
    try:
        creds = shared.read_json(cred)
    except Exception:
        creds = None
    token = shared.deep_find(creds, {"access_token", "accessToken"}) if creds else None
    if not token:
        return shared.result("gemini", "Gemini CLI", "error",
                             detail="No access token in oauth_creds.json")

    # Proactively refresh the cached token when it's expired (or within a minute
    # of it) instead of letting loadCodeAssist 401 us into the fallback.
    refreshed = False
    expiry = shared.to_epoch(creds.get("expiry_date"))
    if expiry is not None and expiry <= shared.now_ts() + 60:
        new_token = _refresh_gemini_token(creds, cred)
        if new_token:
            token, refreshed = new_token, True

    env_project = (
        os.environ.get("GOOGLE_CLOUD_PROJECT")
        or _gemini_project_from_config()
    )
    status, plan, windows, detail = _codeassist_quota(
        token, env_project, "gemini-cli/usage-monitor", period_override="daily")

    if status != "live" and not refreshed and creds.get("refresh_token"):
        new_token = _refresh_gemini_token(creds, cred)
        if new_token and new_token != token:
            status, plan, windows, detail = _codeassist_quota(
                new_token, env_project, "gemini-cli/usage-monitor", period_override="daily")

    if status == "live":
        return shared.result("gemini", "Gemini CLI", "ok", plan=plan, source="live",
                             windows=_rank_models(windows))
    return shared.result("gemini", "Gemini CLI", "error", plan=plan, detail=detail)
