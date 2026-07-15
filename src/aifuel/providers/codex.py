from __future__ import annotations

import os
import urllib.error
from typing import Any

from .. import shared
from .base import BaseProvider


def _credential_path():
    return os.path.join(shared.HOME, ".codex", "auth.json")


def _codex_window(rl_window, label_prefix=None):
    """Build a window() from a ChatGPT `*_window` rate-limit object."""
    if not isinstance(rl_window, dict):
        return None
    period, label = shared.period_for_seconds(rl_window.get("limit_window_seconds"))
    if label_prefix:
        label = f"{label_prefix} {label}"
    resets = rl_window.get("reset_at")
    if resets is None and rl_window.get("reset_after_seconds") is not None:
        resets = shared.now_ts() + float(rl_window["reset_after_seconds"])
    return shared.window(label, period, used_percent=rl_window.get("used_percent"),
                         resets_at=resets)


class CodexProvider(BaseProvider):
    @classmethod
    def is_discovered(cls) -> bool:
        return shared.credential_source_exists(_credential_path())

    @property
    def key(self) -> str:
        return "codex"

    @property
    def name(self) -> str:
        return "Codex CLI"

    @property
    def cache_ttl_seconds(self) -> int:
        return 30

    def retrieve_quota(self) -> dict[str, Any]:
        """Live: ChatGPT backend usage endpoint (same data the Codex TUI refreshes)."""
        auth_path = _credential_path()
        token = account = None
        if os.path.exists(auth_path):
            try:
                auth = shared.read_json(auth_path)
                token = shared.deep_find(auth, {"access_token"})
                account = shared.deep_find(auth, {"account_id"})
            except Exception as e:
                return shared.result(self.key, self.name, "error",
                                     detail=f"Failed to read Codex auth: {e}")

        if not token:
            return shared.result(
                self.key, self.name, "error",
                detail="Codex live usage unavailable: missing access_token in ~/.codex/auth.json",
            )

        try:
            data, _ = shared.http_get(shared.CODEX_USAGE_URL, headers={
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
                w = _codex_window(rl.get("primary_window"))
                if w:
                    windows.append(w)
                w = _codex_window(rl.get("secondary_window"))
                if w:
                    windows.append(w)
                # Per-model extra limits (e.g. Codex-Spark) -> primary + secondary window each.
                for extra in (data.get("additional_rate_limits") or []):
                    if not isinstance(extra, dict):
                        continue
                    erl = extra.get("rate_limit") or {}
                    limit_name = extra.get("limit_name") or "Model"
                    w = _codex_window(erl.get("primary_window"), limit_name)
                    if w:
                        windows.append(w)
                    w = _codex_window(erl.get("secondary_window"), limit_name)
                    if w:
                        windows.append(w)
                if windows:
                    return shared.result(self.key, self.name, "ok", plan=plan,
                                         source="live", windows=windows)
            return shared.result(self.key, self.name, "error",
                                 detail="Codex live usage endpoint returned no rate_limit windows")
        except urllib.error.HTTPError as e:
            if e.code == 401:
                return shared.result(self.key, self.name, "error",
                                     detail="Token expired — run the Codex CLI once to refresh")
            return shared.result(self.key, self.name, "error",
                                 detail=f"Codex live usage request failed with HTTP {e.code}")
        except Exception as e:
            return shared.result(self.key, self.name, "error",
                                 detail=f"Codex live usage request failed: {e.__class__.__name__}: {e}")


def fetch_codex():
    return CodexProvider().retrieve_quota()
