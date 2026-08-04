from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
import urllib.error
from typing import Any

from .. import shared
from .base import BaseProvider


_APP_SERVER_TIMEOUT_SECONDS = 12


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


def _nonnegative_int(value):
    try:
        return max(0, int(value))
    except (TypeError, ValueError):
        return None


def _await_response(responses, response_id, timeout):
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        try:
            response = responses.get(timeout=remaining)
        except queue.Empty:
            return None
        if isinstance(response, dict) and response.get("id") == response_id:
            return response


def _parse_app_server_reset_credits(reset_credits):
    if not isinstance(reset_credits, dict):
        return None
    available_count = _nonnegative_int(reset_credits.get("availableCount"))
    if available_count is None:
        return None
    credits = []
    for credit in reset_credits.get("credits") or []:
        if not isinstance(credit, dict):
            continue
        credits.append({
            "reset_type": credit.get("resetType"),
            "title": credit.get("title"),
            "description": credit.get("description"),
            "expires_at": shared.to_epoch(credit.get("expiresAt")),
        })
    return {"available_count": available_count, "credits": credits}


def _app_server_reset_credits():
    """Read detailed Codex reset credits through the CLI's supported app-server API."""
    proc = None
    try:
        proc = subprocess.Popen(
            ["codex", "app-server", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        responses = queue.Queue()

        def read_stdout():
            for line in proc.stdout:
                try:
                    responses.put(json.loads(line))
                except json.JSONDecodeError:
                    pass

        threading.Thread(target=read_stdout, daemon=True).start()

        def send(message):
            proc.stdin.write(json.dumps(message) + "\n")
            proc.stdin.flush()

        send({
            "id": 0,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "aifuel",
                    "title": "aifuel",
                    "version": "1",
                },
            },
        })
        initialized = _await_response(responses, 0, _APP_SERVER_TIMEOUT_SECONDS)
        if not initialized or initialized.get("error"):
            return None
        send({"method": "initialized", "params": {}})
        send({"id": 1, "method": "account/rateLimits/read"})
        response = _await_response(responses, 1, _APP_SERVER_TIMEOUT_SECONDS)
        if not response or response.get("error"):
            return None

        result = response.get("result")
        if not isinstance(result, dict):
            return None
        return _parse_app_server_reset_credits(result.get("rateLimitResetCredits"))
    except (OSError, BrokenPipeError):
        return None
    finally:
        if proc is not None:
            if proc.stdin:
                try:
                    proc.stdin.close()
                except OSError:
                    pass
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()


def _codex_reset_credits(usage_data):
    """Use the HTTP count as a fallback when detailed CLI credit data is unavailable."""
    reported = usage_data.get("rate_limit_reset_credits")
    available_count = (_nonnegative_int(reported.get("available_count"))
                       if isinstance(reported, dict) else None)
    if available_count is None:
        return None
    return _app_server_reset_credits() or {
        "available_count": available_count,
        "credits": [],
    }


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
                    res = shared.result(self.key, self.name, "ok", plan=plan,
                                        source="live", windows=windows)
                    res["reset_credits"] = _codex_reset_credits(data)
                    return res
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
