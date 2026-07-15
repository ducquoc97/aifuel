from __future__ import annotations

import json
import os
import sqlite3
import sys
from datetime import datetime, timedelta, timezone
from typing import Any

from .. import shared
from . import gemini
from .base import BaseProvider


def _antigravity_token(path):
    """(creds, access_token, expired) from antigravity-cli's own OAuth token file."""
    if not os.path.exists(path):
        return None, None, True
    try:
        creds = shared.read_json(path)
    except Exception:
        return None, None, True
    token = shared.deep_find(creds, {"access_token", "accessToken"})
    exp = shared.to_epoch(shared.deep_find(creds, {"expiry", "expiry_date", "expires_at"}))
    expired = exp is not None and exp <= shared.now_ts()
    return creds, token, expired


def _refresh_antigravity_token(creds, path):
    """Mint a fresh access token for antigravity-cli from its stored refresh_token."""
    tokobj = creds.get("token") if isinstance(creds, dict) else None
    refresh = tokobj.get("refresh_token") if isinstance(tokobj, dict) else None
    tok = gemini._google_oauth_refresh(
        shared.ANTIGRAVITY_CLI_PUBLIC_CLIENT_ID,
        shared.ANTIGRAVITY_CLI_PUBLIC_CLIENT_SECRET,
        refresh,
    )
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
        if callable(path):
            path(creds)
        elif path:
            shared.write_json_atomic(path, creds)
    except Exception:
        pass  # the in-memory token is still usable even if we can't persist it
    return access


def _antigravity_project():
    """gcp.project from antigravity-cli/settings.json (absent for a personal login)."""
    path = os.path.join(shared.HOME, ".gemini", "antigravity-cli", "settings.json")
    try:
        gcp = shared.read_json(path).get("gcp")
    except Exception:
        return None
    return gcp.get("project") if isinstance(gcp, dict) else None


def _antigravity_keychain_creds():
    """Current agy auth from the macOS keychain (service=gemini, acct=antigravity)."""
    raw = shared.read_keychain_secret(
        shared.ANTIGRAVITY_KEYCHAIN_SERVICE,
        shared.ANTIGRAVITY_KEYCHAIN_ACCOUNT,
    )
    if not raw:
        return None, None, True
    try:
        creds = shared.decode_go_keyring_secret(raw)
    except (ValueError, TypeError, json.JSONDecodeError, OSError):
        return None, None, True
    token = shared.deep_find(creds, {"access_token", "accessToken"})
    exp = shared.to_epoch(shared.deep_find(creds, {"expiry", "expiry_date", "expires_at"}))
    expired = exp is not None and exp <= shared.now_ts()
    return creds, token, expired


def _write_antigravity_keychain_creds(creds):
    secret = shared.encode_go_keyring_secret(creds)
    if not shared.write_keychain_secret(
        shared.ANTIGRAVITY_KEYCHAIN_SERVICE,
        shared.ANTIGRAVITY_KEYCHAIN_ACCOUNT,
        secret,
    ):
        raise OSError("failed to update antigravity keychain token")


def _antigravity_app_state_dbs():
    """Current Antigravity desktop-app state stores that may hold auth on disk."""
    paths = []
    if sys.platform == "darwin":
        paths.append(os.path.join(
            shared.HOME, "Library", "Application Support", "Antigravity",
            "User", "globalStorage", "state.vscdb"))
    elif sys.platform == "win32":
        appdata = os.environ.get("APPDATA") or os.path.join(shared.HOME, "AppData", "Roaming")
        paths.append(os.path.join(appdata, "Antigravity", "User", "globalStorage", "state.vscdb"))
    else:
        paths.extend([
            os.path.join(shared.HOME, ".config", "Antigravity", "User", "globalStorage", "state.vscdb"),
            os.path.join(shared.HOME, ".config", "antigravity", "User", "globalStorage", "state.vscdb"),
        ])
    return paths


def _antigravity_app_token():
    """Fallback live token from the desktop app's persisted auth status."""
    for path in _antigravity_app_state_dbs():
        if not os.path.exists(path):
            continue
        try:
            raw = shared.read_sqlite_item(path, "antigravityAuthStatus")
            status = json.loads(raw) if raw else None
        except (OSError, sqlite3.Error, json.JSONDecodeError, TypeError, ValueError):
            continue
        token = status.get("apiKey") if isinstance(status, dict) else None
        if isinstance(token, str) and token.strip():
            return token.strip()
    return None


class AntigravityProvider(BaseProvider):
    @classmethod
    def is_discovered(cls) -> bool:
        base = os.path.join(shared.HOME, ".gemini")
        provider_dirs = (
            os.path.join(base, "antigravity"),
            os.path.join(base, "antigravity-cli"),
        )
        if any(shared.credential_source_exists(path, directory=True)
               for path in provider_dirs):
            return True
        if any(shared.credential_source_exists(path)
               for path in _antigravity_app_state_dbs()):
            return True
        return shared.read_keychain_secret(
            shared.ANTIGRAVITY_KEYCHAIN_SERVICE,
            shared.ANTIGRAVITY_KEYCHAIN_ACCOUNT,
            strict=True,
        ) is not None

    @property
    def key(self) -> str:
        return "antigravity"

    @property
    def name(self) -> str:
        return "Antigravity CLI"

    @property
    def cache_ttl_seconds(self) -> int:
        return 60

    def retrieve_quota(self) -> dict[str, Any]:
        base = os.path.join(shared.HOME, ".gemini")
        dirs = [os.path.join(base, "antigravity"), os.path.join(base, "antigravity-cli")]
        keychain_creds, keychain_token, keychain_expired = _antigravity_keychain_creds()
        desktop_token = _antigravity_app_token()
        present = (any(os.path.isdir(d) for d in dirs)
                   or keychain_creds is not None
                   or desktop_token is not None)
        if not present:
            return shared.result(self.key, self.name, "error",
                                 detail="No ~/.gemini/antigravity* directory")

        tok_path = os.path.join(base, "antigravity-cli", "antigravity-oauth-token")
        creds, token, expired = _antigravity_token(tok_path)

        refreshed = False
        if creds and (expired or not token):
            new_token = _refresh_antigravity_token(creds, tok_path)
            if new_token:
                token, expired, refreshed = new_token, False, True

        plan_names = {"antigravity": "Antigravity Starter Quota"}

        live_detail = None
        plan = None
        project = _antigravity_project()
        if keychain_creds and (keychain_expired or not keychain_token):
            new_token = _refresh_antigravity_token(keychain_creds, _write_antigravity_keychain_creds)
            if new_token:
                keychain_token, keychain_expired = new_token, False

        tokens_to_try = []
        if token and not expired:
            tokens_to_try.append(("file", token, creds, lambda: _refresh_antigravity_token(creds, tok_path)))
        if keychain_token and not keychain_expired and keychain_token != token:
            tokens_to_try.append((
                "keychain",
                keychain_token,
                keychain_creds,
                lambda: _refresh_antigravity_token(
                    keychain_creds, _write_antigravity_keychain_creds),
            ))
        if desktop_token and desktop_token != token:
            tokens_to_try.append(("desktop", desktop_token, None, None))
        for source_name, auth_token, source_creds, refresh in tokens_to_try:
            status, plan, windows, live_detail = gemini._codeassist_quota(
                auth_token, project, "antigravity/usage-monitor")
            plan = plan_names.get((plan or "").lower(), plan)
            if (status != "live" and live_detail and "OAuth token expired" in live_detail
                    and refresh
                    and ((source_name != "file") or (not refreshed and source_creds))):
                new_token = refresh()
                if new_token and new_token != token:
                    if source_name == "file":
                        refreshed = True
                    status, plan, windows, live_detail = gemini._codeassist_quota(
                        new_token, project, "antigravity/usage-monitor")
                    plan = plan_names.get((plan or "").lower(), plan)
            if status == "live":
                return shared.result(self.key, self.name, "ok", plan=plan,
                                     source="live",
                                     windows=gemini._rank_models(windows))

        if token and expired:
            detail = "OAuth token expired and auto-refresh failed — run antigravity once"
        elif live_detail:
            detail = live_detail
        else:
            detail = "Antigravity live usage unavailable"
        return shared.result(self.key, self.name, "error", plan=plan, detail=detail)


def fetch_antigravity():
    return AntigravityProvider().retrieve_quota()
