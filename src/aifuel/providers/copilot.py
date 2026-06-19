from __future__ import annotations

import json
import os
import re
import sys

from .. import shared


def _copilot_token():
    """Prefer the Copilot CLI's own account token, then gh, then env.

    The Copilot CLI / IDE may be signed into a different GitHub account than the
    `gh` CLI, so reading gh's token shows the wrong account's quota. Returns
    (token, account_label).
    """
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
    hosts_candidates = [os.path.join(shared.HOME, ".config", "gh", "hosts.yml")]
    if sys.platform == "win32":
        appdata = os.environ.get("APPDATA") or os.path.join(shared.HOME, "AppData", "Roaming")
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
    token, _account = _copilot_token()
    if not token:
        return shared.result("copilot", "GitHub Copilot", "unavailable",
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
            data, _ = shared.http_get(url, headers=headers)
            if isinstance(data, dict):
                break
        except Exception:
            continue

    if not isinstance(data, dict):
        return shared.result("copilot", "GitHub Copilot", "partial", source="schedule",
                             detail="Copilot usage endpoint unreachable; monthly reset only",
                             windows=[shared.window("Premium requests", "monthly",
                                                    resets_at=shared.next_month_first_utc())])

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
        return shared.result("copilot", "GitHub Copilot", "partial", plan=plan,
                             source="schedule",
                             detail="No quota snapshot for this account; monthly reset only",
                             windows=[shared.window("Premium requests", "monthly",
                                                    resets_at=reset_at)])
    return shared.result("copilot", "GitHub Copilot", "ok", plan=plan, source="live",
                         windows=windows)
