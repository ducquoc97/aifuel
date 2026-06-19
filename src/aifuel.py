#!/usr/bin/env python3
"""
aifuel — fuel gauge for your AI coding subscriptions
====================================================

Modular stdlib-only dashboard that shows the *remaining* subscription quota for
the AI coding CLIs you use, ordered by whichever weekly / monthly window resets
soonest.

Providers:
    - Claude Code      (handler: aifuel.providers.claude)
    - Codex CLI        (handler: aifuel.providers.codex)
    - GitHub Copilot   (handler: aifuel.providers.copilot)
    - Gemini CLI       (handler: aifuel.providers.gemini)
    - Antigravity CLI  (handler: aifuel.providers.antigravity)

Usage:
    python3 src/aifuel.py            # serve dashboard + open browser at http://127.0.0.1:8787
    python3 src/aifuel.py --json     # print the usage JSON and exit
    python3 src/aifuel.py --text     # print a compact colored terminal summary and exit
    python3 src/aifuel.py --port N   # use a different port
    python3 src/aifuel.py --no-browser  # serve without opening the browser
"""
from __future__ import annotations

import argparse
import json
import os
import queue
import sys
import threading
import urllib.parse
import webbrowser
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


THIS_DIR = os.path.dirname(os.path.abspath(__file__))
if THIS_DIR not in sys.path:
    sys.path.insert(0, THIS_DIR)

from aifuel import shared
from aifuel.providers import (
    fetch_antigravity,
    fetch_claude,
    fetch_codex,
    fetch_copilot,
    fetch_gemini,
)


HTML_PATH = os.path.join(THIS_DIR, "index.html")

PROVIDERS = [
    ("claude", fetch_claude, 180),       # claude oauth/usage 429s if polled fast
    ("codex", fetch_codex, 30),
    ("copilot", fetch_copilot, 120),
    ("gemini", fetch_gemini, 120),
    ("antigravity", fetch_antigravity, 60),
]

_PERIOD_RANK = {"monthly": 0, "weekly": 1, "daily": 2, "5h": 3, "unknown": 4}


def effective_remaining(res):
    """Anchor remaining percent used to decide whether a provider has fuel."""
    windows = sorted(res.get("windows", []),
                     key=lambda w: _PERIOD_RANK.get(w.get("period", "unknown"), 99))
    for w in windows:
        if w.get("remaining_percent") is not None:
            return w["remaining_percent"]
    return -1


# ---------------------------------------------------------------------------
# Caching + aggregation
# ---------------------------------------------------------------------------

_cache = {}            # key -> (expires_at, result)
_cache_lock = threading.Lock()


def cache_ttl(ttl, res):
    """Keep reset-less results short-lived so transient misses heal quickly."""
    return ttl if shared.effective_reset(res) is not None else min(ttl, 15)


def get_provider(key, fn, ttl, force=False):
    with _cache_lock:
        hit = _cache.get(key)
        if hit and not force and hit[0] > shared.now_ts():
            return hit[1]
    try:
        res = fn()
    except Exception as e:
        res = shared.result(key, key.title(), "error", detail=f"{e.__class__.__name__}: {e}")
    if shared.effective_reset(res) is not None:
        shared.write_snapshot(key, res)
    with _cache_lock:
        _cache[key] = (shared.now_ts() + cache_ttl(ttl, res), res)
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
        res["reset_at"] = shared.effective_reset(res)
        results.append(res)

    far = float("inf")
    results.sort(key=lambda r: (effective_remaining(r) <= 0,
                                r["reset_at"] if r["reset_at"] else far))
    return {"generated_at": shared.now_ts(), "providers": results}


def collect_stream(force=False):
    """Yield each provider result the moment it finishes, in ready-first order."""
    q = queue.Queue()

    def run(key, fn, ttl):
        res = get_provider(key, fn, ttl, force=force)
        res["reset_at"] = shared.effective_reset(res)
        q.put(res)

    for key, fn, ttl in PROVIDERS:
        threading.Thread(target=run, args=(key, fn, ttl), daemon=True).start()

    for _ in PROVIDERS:
        yield q.get()


# ---------------------------------------------------------------------------
# Terminal renderer (--text)
# ---------------------------------------------------------------------------

_ANSI = {
    "reset": "\033[0m", "bold": "\033[1m",
    "green": "\033[32m", "yellow": "\033[33m", "red": "\033[31m", "grey": "\033[90m",
}
_STATUS_COLOR = {"ok": "green", "error": "red"}


def _fmt_countdown(secs):
    """Human countdown to a reset, mirroring the web dashboard's fmtCountdown."""
    if secs is None:
        return "no reset"
    secs = int(secs)
    if secs <= 0:
        return "resetting…"
    d, rem = divmod(secs, 86400)
    h, rem = divmod(rem, 3600)
    m, s = divmod(rem, 60)
    if d:
        return f"{d}d {h}h {m}m"
    if h:
        return f"{h}h {m}m"
    return f"{m}m {s}s"


def _rem_color(rem):
    if rem is None:
        return "grey"
    if rem <= 10:
        return "red"
    if rem <= 30:
        return "yellow"
    return "green"


def render_text(data, color=True):
    """Compact, colored one-screen summary of `collect()` for the terminal."""
    def paint(code, text):
        return f"{_ANSI[code]}{text}{_ANSI['reset']}" if (color and code in _ANSI) else text

    providers = data.get("providers", [])
    labels = [w["label"] for p in providers for w in p["windows"]]
    width = min(max((len(l) for l in labels), default=10), 28)

    now = shared.now_ts()
    updated = datetime.fromtimestamp(data["generated_at"]).strftime("%H:%M:%S")
    out = [paint("bold", "aifuel")
           + paint("grey", f"   updated {updated} · ranked by soonest reset")]

    for i, p in enumerate(providers, 1):
        src = "live" if p["source"] == "live" else None
        meta = " · ".join(x for x in (p.get("plan"), src, p["status"]) if x)
        dot = paint(_STATUS_COLOR.get(p["status"], "grey"), "●")
        out.append("")
        out.append(f"{i}. {dot} {paint('bold', p['name'])}  {paint('grey', meta)}")

        if not p["windows"]:
            out.append(f"     {paint('grey', p.get('detail') or 'no usage data')}")
            continue
        if p.get("detail"):
            out.append(f"     {paint('grey', p['detail'])}")

        for w in p["windows"]:
            rem = w["remaining_percent"]
            rc = _rem_color(rem)
            filled = 12 if rem is None else max(0, min(12, round(rem / 100 * 12)))
            bar = paint(rc, "█" * filled) + paint("grey", "░" * (12 - filled))
            rem_txt = " n/a" if rem is None else f"{round(rem):3d}%"
            label = w["label"]
            if len(label) > width:
                label = label[: width - 1] + "…"
            secs = (w["resets_at"] - now) if w["resets_at"] else None
            tail = f"↻ {_fmt_countdown(secs)} {w['period']}"
            out.append(f"     {label:<{width}}  {bar}  "
                       f"{paint(rc, rem_txt)}  {paint('grey', tail)}")
    return "\n".join(out)


# ---------------------------------------------------------------------------
# Web server
# ---------------------------------------------------------------------------


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

    def _request_is_local(self):
        """Refuse cross-origin / DNS-rebinding requests."""
        bound_host = self.server.server_address[0]
        if bound_host in ("127.0.0.1", "::1", "localhost"):
            raw = (self.headers.get("Host") or "").strip()
            if raw.startswith("["):           # [::1]:port
                host = raw[1:].split("]", 1)[0]
            elif raw.count(":") == 1:        # 127.0.0.1:port / localhost:port
                host = raw.rsplit(":", 1)[0]
            else:                            # bare host, or unbracketed ::1
                host = raw
            if host and host not in ("127.0.0.1", "::1", "localhost"):
                return False
        site = (self.headers.get("Sec-Fetch-Site") or "").lower()
        mode = (self.headers.get("Sec-Fetch-Mode") or "").lower()
        if site in ("cross-site", "same-site") and mode != "navigate":
            return False
        return True

    def do_GET(self):
        if not self._request_is_local():
            self._send(403, "forbidden: cross-origin request rejected", "text/plain")
            return
        parts = urllib.parse.urlsplit(self.path)
        path = parts.path
        if path == "/":
            try:
                with open(HTML_PATH, "r", encoding="utf-8") as fh:
                    body = fh.read()
                self._send(200, body, "text/html; charset=utf-8")
            except OSError as e:
                self._send(500, f"index.html not found: {e}", "text/plain")
        elif path == "/api/usage":
            force = urllib.parse.parse_qs(parts.query).get("force", [""])[0] == "1"
            payload = json.dumps(collect(force=force), default=str)
            self._send(200, payload, "application/json")
        elif path == "/api/usage/stream":
            self._stream_usage(parts)
        else:
            self._send(404, "not found", "text/plain")

    def _stream_usage(self, parts):
        """Stream provider results as newline-delimited JSON."""
        force = urllib.parse.parse_qs(parts.query).get("force", [""])[0] == "1"
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Accel-Buffering", "no")  # don't let a proxy buffer the stream
        self.end_headers()

        def emit(obj):
            self.wfile.write((json.dumps(obj, default=str) + "\n").encode("utf-8"))
            self.wfile.flush()

        try:
            emit({"providers_expected": [key for key, _, _ in PROVIDERS]})
            for res in collect_stream(force=force):
                emit({"provider": res})
            emit({"done": True, "generated_at": shared.now_ts()})
        except (BrokenPipeError, ConnectionResetError):
            pass  # client navigated away mid-stream; provider threads still warm the cache


def main():
    ap = argparse.ArgumentParser(description="aifuel — fuel gauge for your AI coding subscriptions")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--json", action="store_true", help="print usage JSON and exit")
    ap.add_argument("--text", action="store_true",
                    help="print a compact colored summary to the terminal and exit")
    ap.add_argument("--no-color", action="store_true", help="disable --text colors")
    ap.add_argument("--no-browser", action="store_true",
                    help="serve dashboard without opening a browser")
    args = ap.parse_args()

    if args.json:
        print(json.dumps(collect(force=True), indent=2, default=str))
        return

    if args.text:
        color = (not args.no_color
                 and sys.stdout.isatty()
                 and os.environ.get("NO_COLOR") is None)
        print(render_text(collect(force=True), color=color))
        return

    url = f"http://{args.host}:{args.port}"
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"aifuel → {url}")
    print("Ordered by nearest weekly/monthly reset.  Ctrl-C to stop.")
    if args.host not in ("127.0.0.1", "::1", "localhost"):
        print(f"warning: bound to {args.host} — the dashboard and force-refresh "
              "are reachable by other machines with no authentication.")
    if not args.no_browser:
        threading.Timer(0.6, lambda: webbrowser.open(url)).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nbye")
        server.shutdown()


if __name__ == "__main__":
    main()
