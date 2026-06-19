from __future__ import annotations

import glob
import json
import os
import urllib.error

from .. import shared


def _codex_window(rl_window, label_override=None):
    """Build a window() from a ChatGPT `*_window` rate-limit object."""
    if not isinstance(rl_window, dict):
        return None
    period, label = shared.period_for_seconds(rl_window.get("limit_window_seconds"))
    if label_override:
        label = label_override
    resets = rl_window.get("reset_at")
    if resets is None and rl_window.get("reset_after_seconds") is not None:
        resets = shared.now_ts() + float(rl_window["reset_after_seconds"])
    return shared.window(label, period, used_percent=rl_window.get("used_percent"),
                         resets_at=resets)


def _fetch_codex_cache():
    pattern = os.path.join(shared.HOME, ".codex", "sessions", "**", "rollout-*.jsonl")
    files = glob.glob(pattern, recursive=True)
    if not files:
        return shared.result("codex", "Codex CLI", "unavailable", detail="No codex session files")
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
            rl = shared.deep_find(obj, {"rate_limits"})
            if isinstance(rl, dict) and (rl.get("primary") or rl.get("secondary")):
                snapshot = rl
                plan = rl.get("plan_type") or plan
                break
        if snapshot:
            break

    if not snapshot:
        return shared.result("codex", "Codex CLI", "unavailable",
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
            period, label = shared.period_for_seconds(mins * 60)
        windows.append(shared.window(label, period,
                                     used_percent=w.get("used_percent"),
                                     resets_at=w.get("resets_at")))
    return shared.result("codex", "Codex CLI", "ok", plan=plan, source="local-cache",
                         windows=windows)


def fetch_codex():
    """Live: ChatGPT backend usage endpoint (same data the Codex TUI refreshes).

    Falls back to the last local `rate_limits` session snapshot if the live call
    fails (e.g. expired token).
    """
    auth_path = os.path.join(shared.HOME, ".codex", "auth.json")
    token = account = None
    if os.path.exists(auth_path):
        try:
            auth = shared.read_json(auth_path)
            token = shared.deep_find(auth, {"access_token"})
            account = shared.deep_find(auth, {"account_id"})
        except Exception:
            pass

    if token:
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
                w = _codex_window(rl.get("primary_window"), "5-hour")
                if w:
                    windows.append(w)
                w = _codex_window(rl.get("secondary_window"), "Weekly")
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
                    w = _codex_window(erl.get("secondary_window"),
                                      f"{limit_name} Weekly")
                    if w:
                        windows.append(w)
                if windows:
                    return shared.result("codex", "Codex CLI", "ok", plan=plan,
                                         source="live", windows=windows)
        except urllib.error.HTTPError as e:
            if e.code in (401, 403):
                pass  # token stale -> fall back to local cache below
        except Exception:
            pass

    return _fetch_codex_cache()
