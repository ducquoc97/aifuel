from __future__ import annotations

import json
import os
import urllib.error
from typing import Any

from .. import shared
from .base import BaseProvider


def _copilot_token():
    """Return the Copilot CLI's own token and account label."""
    cfg = os.path.join(shared.HOME, ".copilot", "config.json")
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
    return None, None


class CopilotProvider(BaseProvider):
    @property
    def key(self) -> str:
        return "copilot"

    @property
    def name(self) -> str:
        return "GitHub Copilot"

    @property
    def cache_ttl_seconds(self) -> int:
        return 120

    def retrieve_quota(self) -> dict[str, Any]:
        token, _account = _copilot_token()
        if not token:
            return shared.result(self.key, self.name, "error",
                                 detail="No Copilot-specific token found")

        headers = {
            "Authorization": f"token {token}",
            "User-Agent": "GithubCopilot/1.250.0",
            "Editor-Version": "vscode/1.99.0",
            "Editor-Plugin-Version": "copilot/1.250.0",
            "Accept": "application/json",
        }
        data = None
        last_err_code = None
        for url in ("https://api.github.com/copilot_internal/user",
                    "https://api.github.com/copilot_internal/v2/token"):
            try:
                data, _ = shared.http_get(url, headers=headers)
                if isinstance(data, dict):
                    break
            except urllib.error.HTTPError as e:
                last_err_code = e.code
                continue
            except Exception:
                continue

        if not isinstance(data, dict):
            if last_err_code == 401:
                return shared.result(self.key, self.name, "error",
                                     detail="Token expired/unauthorized — sign in using the Copilot CLI")
            return shared.result(self.key, self.name, "error",
                                 detail="Copilot live usage endpoint unreachable")

        plan = data.get("copilot_plan") or shared.deep_find(data, {"plan"})
        reset_at = (shared.to_epoch(data.get("quota_reset_date_utc"))
                    or shared.to_epoch(data.get("quota_reset_date"))
                    or shared.to_epoch(shared.deep_find(data, {"limited_user_reset_date", "reset_date"}))
                    or shared.next_month_first_utc())

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
                if label == "Premium Interactions":
                    label = "Premium Models"
                if snap.get("unlimited"):
                    windows.append(shared.window(label, "monthly",
                                                 remaining_percent=100.0, resets_at=reset_at))
                    continue
                pct = snap.get("percent_remaining")
                rem = snap.get("remaining")
                if rem is None:
                    rem = snap.get("quota_remaining")
                rp = float(pct) if pct is not None else (
                    round(rem / ent * 100, 1) if (rem is not None and ent) else None)
                used = round(ent - rem) if (ent is not None and rem is not None) else None
                windows.append(shared.window(label, "monthly", remaining_percent=rp,
                                             used=used, limit=ent, resets_at=reset_at))

        if not windows:
            return shared.result(self.key, self.name, "error", plan=plan,
                                 detail="Copilot live usage returned no quota snapshots")
        return shared.result(self.key, self.name, "ok", plan=plan, source="live",
                             windows=windows)


def fetch_copilot():
    return CopilotProvider().retrieve_quota()
