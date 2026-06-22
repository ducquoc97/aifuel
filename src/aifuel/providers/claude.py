from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

from .. import shared
from .base import BaseProvider


_CLAUDE_RATE_LIMIT_CLAIMS = {
    "five_hour": ("Current session", "5h"),
    "5_hour": ("Current session", "5h"),
    "5h": ("Current session", "5h"),
    "seven_day": ("Current week (all models)", "weekly"),
    "weekly": ("Current week (all models)", "weekly"),
    "7d": ("Current week (all models)", "weekly"),
    "seven_day_opus": ("Current week (Opus only)", "weekly"),
    "seven_day_sonnet": ("Current week (Sonnet only)", "weekly"),
    "overage": ("Usage credits", "monthly"),
}


def _claude_oauth_refresh(refresh_token, client_id=None):
    """Exchange Claude Code's stored refresh token for a fresh OAuth token."""
    if not refresh_token:
        return None
    body = urllib.parse.urlencode({
        "client_id": client_id or shared.CLAUDE_CLI_PUBLIC_CLIENT_ID,
        "refresh_token": refresh_token,
        "grant_type": "refresh_token",
    }).encode("utf-8")
    req = urllib.request.Request(
        shared.CLAUDE_OAUTH_TOKEN_URI, data=body,
        headers={
            "Content-Type": "application/x-www-form-urlencoded",
            "Accept": "application/json",
            "User-Agent": "claude-cli/usage-monitor",
        },
        method="POST",
    )
    try:
        with shared._urlopen(req, timeout=shared.HTTP_TIMEOUT) as resp:
            return json.loads(resp.read().decode("utf-8", "replace"))
    except Exception:
        return None


def _refresh_claude_token(creds, path):
    """Refresh Claude Code's OAuth token and persist the rotated token set."""
    oauth = creds.get("claudeAiOauth") if isinstance(creds, dict) else None
    if not isinstance(oauth, dict):
        return None
    tok = _claude_oauth_refresh(
        oauth.get("refreshToken"),
        oauth.get("clientId") or shared.CLAUDE_CLI_PUBLIC_CLIENT_ID,
    )
    access = tok.get("access_token") if isinstance(tok, dict) else None
    refresh = tok.get("refresh_token") if isinstance(tok, dict) else None
    expires_in = tok.get("expires_in") if isinstance(tok, dict) else None
    if not access or not refresh or not expires_in:
        return None
    oauth["accessToken"] = access
    oauth["refreshToken"] = refresh
    oauth["expiresAt"] = int(shared.now_ts() * 1000) + int(expires_in) * 1000
    if tok.get("scope"):
        oauth["scopes"] = str(tok["scope"]).split()
    try:
        shared.write_json_atomic(path, creds)
    except Exception:
        pass  # the in-memory token is still usable even if we can't persist it
    return access


class ClaudeProvider(BaseProvider):
    @property
    def key(self) -> str:
        return "claude"

    @property
    def name(self) -> str:
        return "Claude Code"

    @property
    def cache_ttl_seconds(self) -> int:
        return 180  # claude oauth/usage 429s if polled fast

    def retrieve_quota(self) -> dict[str, Any]:
        cred_path = os.path.join(shared.HOME, ".claude", ".credentials.json")
        if not os.path.exists(cred_path):
            return shared.result(self.key, self.name, "error",
                                 detail=f"No ~/.claude/.credentials.json")
        try:
            creds = shared.read_json(cred_path)
        except Exception as e:
            return shared.result(self.key, self.name, "error", detail=f"creds unreadable: {e}")

        token = shared.deep_find(creds, {"accessToken", "access_token"})
        expiry = shared.to_epoch(shared.deep_find(creds, {"expiresAt", "expires_at", "expiry_date"}))
        refreshed = False
        if expiry is not None and expiry <= shared.now_ts() + 60:
            new_token = _refresh_claude_token(creds, cred_path)
            if new_token:
                token, refreshed = new_token, True
        if not token:
            return shared.result(self.key, self.name, "error",
                                 detail="No access token in credentials")

        def usage_headers(access_token):
            return {
                "Authorization": f"Bearer {access_token}",
                "anthropic-beta": "oauth-2025-04-20",
                "anthropic-version": "2023-06-01",
                "User-Agent": "claude-cli/usage-monitor (external)",
                "Accept": "application/json",
            }

        try:
            data, _ = shared.http_get(
                "https://api.anthropic.com/api/oauth/usage",
                headers=usage_headers(token),
            )
        except urllib.error.HTTPError as e:
            if e.code == 401 and not refreshed:
                new_token = _refresh_claude_token(creds, cred_path)
                if new_token and new_token != token:
                    token, refreshed = new_token, True
                    try:
                        data, _ = shared.http_get(
                            "https://api.anthropic.com/api/oauth/usage",
                            headers=usage_headers(token),
                        )
                    except urllib.error.HTTPError as retry:
                        if retry.code == 401:
                            return shared.result(self.key, self.name, "error",
                                                 detail="OAuth token expired and auto-refresh failed")
                        hint = " (429 = polled too fast; wait a few min)" if retry.code == 429 else ""
                        return shared.result(self.key, self.name, "error",
                                             detail=f"HTTP {retry.code}{hint}")
                    except Exception as retry:
                        return shared.result(self.key, self.name, "error", detail=str(retry))
                else:
                    return shared.result(self.key, self.name, "error",
                                         detail="OAuth token expired and auto-refresh failed")
            else:
                if e.code == 401:
                    return shared.result(self.key, self.name, "error",
                                         detail="OAuth token expired and auto-refresh failed")
                hint = " (429 = polled too fast; wait a few min)" if e.code == 429 else ""
                return shared.result(self.key, self.name, "error", detail=f"HTTP {e.code}{hint}")
        except Exception as e:
            return shared.result(self.key, self.name, "error", detail=str(e))

        if not isinstance(data, dict):
            return shared.result(self.key, self.name, "error", detail="unexpected response")

        windows = []
        for k, v in data.items():
            if not isinstance(v, dict):
                continue
            claim = k.lower()
            if claim not in _CLAUDE_RATE_LIMIT_CLAIMS:
                continue
            used = shared.percent_value(v.get("used_percentage"))
            if used is None:
                used = shared.percent_value(v.get("used_percent"))
            if used is None:
                used = shared.percent_value(v.get("utilization"))
            remaining = shared.percent_value(v.get("remaining_percentage"))
            if remaining is None:
                remaining = shared.percent_value(v.get("remaining_percent"))
            if remaining is None:
                remaining = shared.percent_value(v.get("remaining"))
            resets = (v.get("resets_at") or v.get("resetsAt") or v.get("reset_at")
                      or v.get("resetAt") or v.get("resets"))
            if used is None and remaining is None and resets is None:
                continue
            label, period = _CLAUDE_RATE_LIMIT_CLAIMS[claim]
            windows.append(shared.window(label, period, used_percent=used,
                                         remaining_percent=remaining, resets_at=resets))

        plan = (shared.deep_find(data, {"plan", "subscription", "tier", "subscriptionType", "subscription_type"})
                or shared.deep_find(creds, {"subscriptionType", "subscription_type"}))
        if not windows:
            return shared.result(self.key, self.name, "error", plan=plan,
                                 detail="connected but no usage windows in response")
        return shared.result(self.key, self.name, "ok", plan=plan, source="live", windows=windows)


def fetch_claude():
    return ClaudeProvider().retrieve_quota()
