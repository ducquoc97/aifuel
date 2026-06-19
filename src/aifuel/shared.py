from __future__ import annotations

import base64
import json
import os
import sqlite3
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone


HOME = os.path.expanduser("~")
HTTP_TIMEOUT = 12  # seconds
SNAPSHOT_DIR = os.path.join(HOME, ".cache", "aifuel")
_TLS_FALLBACK_CONTEXT = False

# Live usage endpoints (read-only; authenticated with the CLI's own local token).
CODEX_USAGE_URL = "https://chatgpt.com/backend-api/codex/usage"
GEMINI_API = "https://cloudcode-pa.googleapis.com/v1internal:"  # + loadCodeAssist | retrieveUserQuota
CLAUDE_OAUTH_TOKEN_URI = "https://platform.claude.com/v1/oauth/token"

# gemini-cli's public installed-app client. Source: google-gemini/gemini-cli
# packages/core/src/code_assist/oauth2.ts. Refreshes the token stored in
# ~/.gemini/oauth_creds.json.
GEMINI_CLI_PUBLIC_CLIENT_ID = "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
GEMINI_CLI_PUBLIC_CLIENT_SECRET = "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"  # public, embedded in the upstream CLI
GOOGLE_TOKEN_URI = "https://oauth2.googleapis.com/token"

# antigravity-cli's public installed-app client (consumer login).
ANTIGRAVITY_CLI_PUBLIC_CLIENT_ID = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
ANTIGRAVITY_CLI_PUBLIC_CLIENT_SECRET = "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"  # public, embedded in the upstream CLI
ANTIGRAVITY_KEYCHAIN_SERVICE = "gemini"
ANTIGRAVITY_KEYCHAIN_ACCOUNT = "antigravity"

# Claude Code's public OAuth client id, embedded in the distributed CLI binary.
CLAUDE_CLI_PUBLIC_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"


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


def _default_ca_paths():
    paths = ssl.get_default_verify_paths()
    out = []
    if paths.openssl_cafile_env:
        env_file = os.environ.get(paths.openssl_cafile_env)
        if env_file:
            out.append(env_file)
    if paths.cafile:
        out.append(paths.cafile)
    return {os.path.realpath(p) for p in out if p}


def _default_ca_available():
    paths = ssl.get_default_verify_paths()
    if paths.openssl_cafile_env:
        env_file = os.environ.get(paths.openssl_cafile_env)
        if env_file and os.path.isfile(env_file):
            return True
    if paths.openssl_capath_env:
        env_dir = os.environ.get(paths.openssl_capath_env)
        if env_dir and os.path.isdir(env_dir):
            return True
    if paths.cafile and os.path.isfile(paths.cafile):
        return True
    if paths.capath and os.path.isdir(paths.capath):
        return True
    return False


def _ca_bundle_candidates():
    candidates = []
    if sys.platform == "darwin":
        candidates.extend([
            "/etc/ssl/cert.pem",
            "/private/etc/ssl/cert.pem",
            "/usr/local/etc/openssl@3/cert.pem",
            "/opt/homebrew/etc/openssl@3/cert.pem",
        ])
    elif sys.platform.startswith("linux"):
        candidates.extend([
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/cert.pem",
            "/etc/pki/tls/cert.pem",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        ])
    seen = _default_ca_paths()
    for path in candidates:
        real = os.path.realpath(path)
        if real in seen or real in (None, ""):
            continue
        seen.add(real)
        if os.path.isfile(path):
            yield path


def _fallback_ssl_context():
    global _TLS_FALLBACK_CONTEXT
    if _TLS_FALLBACK_CONTEXT is not False:
        return _TLS_FALLBACK_CONTEXT
    for path in _ca_bundle_candidates():
        try:
            _TLS_FALLBACK_CONTEXT = ssl.create_default_context(cafile=path)
            return _TLS_FALLBACK_CONTEXT
        except ssl.SSLError:
            continue
    _TLS_FALLBACK_CONTEXT = None
    return None


def _is_cert_verify_error(err):
    reason = getattr(err, "reason", None)
    if isinstance(reason, ssl.SSLCertVerificationError):
        return True
    if isinstance(reason, ssl.SSLError):
        text = str(reason).lower()
        return ("certificate verify failed" in text
                or "unable to get local issuer certificate" in text)
    return False


def _urlopen(req, timeout=HTTP_TIMEOUT):
    ctx = _fallback_ssl_context()
    if ctx is not None and not _default_ca_available():
        return urllib.request.urlopen(req, timeout=timeout, context=ctx)
    try:
        return urllib.request.urlopen(req, timeout=timeout)
    except urllib.error.URLError as e:
        if ctx is not None and _is_cert_verify_error(e):
            return urllib.request.urlopen(req, timeout=timeout, context=ctx)
        raise


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
    with _urlopen(req, timeout=HTTP_TIMEOUT) as resp:
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
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(data, fh)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    try:
        os.chmod(tmp, os.stat(path).st_mode & 0o777)
    except OSError:
        pass
    os.replace(tmp, path)


def read_sqlite_item(path, key):
    """Read a VS Code/Electron ItemTable value as text."""
    con = sqlite3.connect(path)
    try:
        row = con.execute(
            "SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?",
            (key,),
        ).fetchone()
        return row[0] if row else None
    finally:
        con.close()


def read_keychain_secret(service, account):
    """Read a macOS generic-password secret as a string."""
    if sys.platform != "darwin":
        return None
    try:
        return subprocess.check_output(
            ["security", "find-generic-password", "-s", service, "-a", account, "-w"],
            text=True, stderr=subprocess.DEVNULL,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def decode_go_keyring_secret(raw):
    """Decode go-keyring's macOS payload wrapper to a JSON object."""
    if not raw:
        return None
    if raw.startswith("go-keyring-base64:"):
        raw = base64.b64decode(raw.split(":", 1)[1]).decode("utf-8")
    return json.loads(raw)


def encode_go_keyring_secret(data):
    raw = json.dumps(data, separators=(",", ":")).encode("utf-8")
    return "go-keyring-base64:" + base64.b64encode(raw).decode("ascii")


def write_keychain_secret(service, account, secret):
    """Write a macOS generic-password secret in place."""
    if sys.platform != "darwin":
        return False
    try:
        subprocess.check_output(
            ["security", "add-generic-password", "-U", "-s", service,
             "-a", account, "-w", secret],
            text=True, stderr=subprocess.DEVNULL,
        )
        return True
    except (OSError, subprocess.CalledProcessError):
        return False


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
        "status": status,        # "ok" | "error"
        "plan": plan,
        "source": source,        # "live" | None
        "detail": detail,
        "windows": windows,
    }


def percent_value(value) -> float | None:
    """Normalize a provider percentage or 0..1 fraction to a percent."""
    if value is None:
        return None
    try:
        pct = float(value)
    except (TypeError, ValueError):
        return None
    if pct <= 1.0:
        pct *= 100.0
    return pct


def period_for_seconds(secs):
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


def effective_reset(res):
    """Reset timestamp used as the secondary provider sort key."""
    weekly_monthly = [w["resets_at"] for w in res["windows"]
                      if w["resets_at"] and w["period"] in ("weekly", "monthly")]
    if weekly_monthly:
        return min(weekly_monthly)
    any_window = [w["resets_at"] for w in res["windows"] if w["resets_at"]]
    return min(any_window) if any_window else None


def has_fresh_reset(res):
    reset = effective_reset(res) if isinstance(res, dict) else None
    return reset is not None and reset > now_ts() - 300


def read_snapshot(key):
    try:
        res = read_json(os.path.join(SNAPSHOT_DIR, f"{key}.json"))
    except Exception:
        return None
    return res if has_fresh_reset(res) else None


def write_snapshot(key, res):
    if effective_reset(res) is None:
        return
    try:
        os.makedirs(SNAPSHOT_DIR, exist_ok=True)
        write_json_atomic(os.path.join(SNAPSHOT_DIR, f"{key}.json"), res)
    except Exception:
        pass
