import importlib.util
import ssl
import sys
import tempfile
import urllib.error
from pathlib import Path
from unittest import TestCase, main, mock


ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from aifuel import shared
from aifuel.providers import (
    BaseProvider,
    ClaudeProvider,
    CodexProvider,
    CopilotProvider,
    GeminiProvider,
    AntigravityProvider,
    ACTIVE_PROVIDERS,
    gemini,
)


SPEC = importlib.util.spec_from_file_location("aifuel_cli", SRC / "aifuel.py")
aifuel_cli = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(aifuel_cli)


class QuotaWindowTests(TestCase):
    def test_period_requires_explicit_provider_semantics(self):
        quota = {
            "buckets": [{
                "modelId": "gemini-2.5-pro",
                "remainingFraction": 0.5,
                "resetTime": 1_000 + 5 * 3600,
            }]
        }

        with mock.patch.object(shared, "now_ts", return_value=1_000):
            unknown = gemini._quota_windows(quota)[0]
            daily = gemini._quota_windows(quota, period_override="daily")[0]

        self.assertEqual(unknown["period"], "unknown")
        self.assertEqual(daily["period"], "daily")

    def test_gemini_model_id_mapping(self):
        quota = {
            "buckets": [
                {
                    "modelId": "gemini-3-flash",
                    "remainingFraction": 0.8,
                    "resetTime": 1000,
                },
                {
                    "modelId": "gemini-3.1-pro-preview-customtools",
                    "remainingFraction": 0.5,
                    "resetTime": 1000,
                },
                {
                    "modelId": "gemini-2.5-pro",
                    "remainingFraction": 0.9,
                    "resetTime": 1000,
                }
            ]
        }

        with mock.patch.object(shared, "now_ts", return_value=100):
            windows = gemini._quota_windows(quota)

        self.assertEqual(len(windows), 3)
        self.assertEqual(windows[0]["label"], "gemini-3.5-flash")
        self.assertEqual(windows[1]["label"], "gemini-3.1-pro-preview")
        self.assertEqual(windows[2]["label"], "gemini-2.5-pro")


class TLSFallbackTests(TestCase):
    def tearDown(self):
        shared._TLS_FALLBACK_CONTEXT = False

    def test_urlopen_uses_fallback_context_when_default_ca_missing(self):
        req = object()
        ctx = object()

        with mock.patch.object(shared, "_fallback_ssl_context", return_value=ctx), \
             mock.patch.object(shared, "_default_ca_available", return_value=False), \
             mock.patch.object(shared.urllib.request, "urlopen", return_value="ok") as urlopen:
            out = shared._urlopen(req, timeout=7)

        self.assertEqual(out, "ok")
        urlopen.assert_called_once_with(req, timeout=7, context=ctx)

    def test_urlopen_retries_cert_verify_error_with_fallback_context(self):
        req = object()
        ctx = object()
        err = urllib.error.URLError(ssl.SSLError("certificate verify failed"))

        with mock.patch.object(shared, "_fallback_ssl_context", return_value=ctx), \
             mock.patch.object(shared, "_default_ca_available", return_value=True), \
             mock.patch.object(shared.urllib.request, "urlopen",
                               side_effect=[err, "ok"]) as urlopen:
            out = shared._urlopen(req, timeout=9)

        self.assertEqual(out, "ok")
        self.assertEqual(urlopen.call_args_list, [
            mock.call(req, timeout=9),
            mock.call(req, timeout=9, context=ctx),
        ])


class ProviderCacheTests(TestCase):
    def setUp(self):
        aifuel_cli._cache.clear()
        self.snapshot_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.snapshot_dir.cleanup)
        patcher = mock.patch.object(shared, "SNAPSHOT_DIR", self.snapshot_dir.name)
        patcher.start()
        self.addCleanup(patcher.stop)

    def tearDown(self):
        aifuel_cli._cache.clear()

    def test_resetless_results_are_retried_quickly(self):
        calls = 0

        def fetch():
            nonlocal calls
            calls += 1
            return shared.result("claude", "Claude Code", "error", detail="boom")

        with mock.patch.object(aifuel_cli.shared, "read_snapshot", return_value=None), \
             mock.patch.object(aifuel_cli.shared, "now_ts", side_effect=[0, 10, 16, 16]):
            aifuel_cli.get_provider("claude", fetch, 180)
            aifuel_cli.get_provider("claude", fetch, 180)
            aifuel_cli.get_provider("claude", fetch, 180)

        self.assertEqual(calls, 2)

    def test_results_with_reset_keep_full_ttl(self):
        calls = 0

        def fetch():
            nonlocal calls
            calls += 1
            return shared.result(
                "claude", "Claude Code", "ok",
                windows=[shared.window("Weekly", "weekly", resets_at=1_000)],
            )

        with mock.patch.object(aifuel_cli.shared, "now_ts", side_effect=[0, 100]):
            aifuel_cli.get_provider("claude", fetch, 180)
            aifuel_cli.get_provider("claude", fetch, 180)

        self.assertEqual(calls, 1)
        self.assertEqual(aifuel_cli._cache["claude"][1]["windows"][0]["resets_at"], 1_000)

    def test_resetless_refresh_does_not_reuse_fresh_memory_result(self):
        good = shared.result(
            "claude", "Claude Code", "ok",
            windows=[shared.window("Weekly", "weekly", resets_at=1_000)],
        )
        error = shared.result("claude", "Claude Code", "error", detail="boom")
        aifuel_cli._cache["claude"] = (0, good)

        with mock.patch.object(aifuel_cli.shared, "now_ts", return_value=100):
            res = aifuel_cli.get_provider(
                "claude",
                lambda: error,
                180,
            )

        self.assertIs(res, error)
        self.assertIsNone(res["source"])

    def test_resetless_refresh_does_not_reuse_stale_memory_result(self):
        stale = shared.result(
            "claude", "Claude Code", "ok",
            windows=[shared.window("Weekly", "weekly", resets_at=1_000)],
        )
        error = shared.result("claude", "Claude Code", "error", detail="boom")
        aifuel_cli._cache["claude"] = (0, stale)

        with mock.patch.object(aifuel_cli.shared, "read_snapshot", return_value=None), \
             mock.patch.object(aifuel_cli.shared, "now_ts", return_value=1_400):
            res = aifuel_cli.get_provider("claude", lambda: error, 180)

        self.assertIs(res, error)
        self.assertIs(aifuel_cli._cache["claude"][1], error)

    def test_resetless_refresh_does_not_reuse_disk_snapshot(self):
        snap = shared.result(
            "claude", "Claude Code", "ok",
            windows=[shared.window("Weekly", "weekly", resets_at=1_000)],
        )
        error = shared.result("claude", "Claude Code", "error", detail="boom")

        with mock.patch.object(aifuel_cli.shared, "read_snapshot", return_value=snap):
            res = aifuel_cli.get_provider(
                "claude",
                lambda: error,
                180,
            )

        self.assertIs(res, error)
        self.assertIsNone(res["source"])


class CLITests(TestCase):
    def test_dashboard_mode_opens_browser_by_default(self):
        server = mock.Mock()
        server.serve_forever.side_effect = KeyboardInterrupt
        timer = mock.Mock()

        with mock.patch.object(sys, "argv", ["aifuel"]), \
             mock.patch.object(aifuel_cli, "ThreadingHTTPServer", return_value=server), \
             mock.patch.object(aifuel_cli.threading, "Timer", return_value=timer) as timer_cls, \
             mock.patch.object(aifuel_cli.webbrowser, "open") as browser_open:
            aifuel_cli.main()
            timer_cls.assert_called_once()
            self.assertEqual(timer_cls.call_args.args[0], 0.6)
            timer_cls.call_args.args[1]()
            browser_open.assert_called_once_with("http://127.0.0.1:8787")

        timer.start.assert_called_once_with()

    def test_no_browser_disables_browser_launch(self):
        server = mock.Mock()
        server.serve_forever.side_effect = KeyboardInterrupt

        with mock.patch.object(sys, "argv", ["aifuel", "--no-browser"]), \
             mock.patch.object(aifuel_cli, "ThreadingHTTPServer", return_value=server), \
             mock.patch.object(aifuel_cli.threading, "Timer") as timer_cls:
            aifuel_cli.main()

        timer_cls.assert_not_called()


class ProviderClassTests(TestCase):
    def test_base_provider_abstract(self):
        with self.assertRaises(TypeError):
            BaseProvider()  # Can't instantiate ABC with abstract methods

    def test_provider_classes_key_and_name(self):
        providers = [
            (ClaudeProvider(), "claude", "Claude Code", 180),
            (CodexProvider(), "codex", "Codex CLI", 30),
            (CopilotProvider(), "copilot", "GitHub Copilot", 120),
            (GeminiProvider(), "gemini", "Gemini CLI", 120),
            (AntigravityProvider(), "antigravity", "Antigravity CLI", 60),
        ]
        for p, expected_key, expected_name, expected_ttl in providers:
            self.assertIsInstance(p, BaseProvider)
            self.assertEqual(p.key, expected_key)
            self.assertEqual(p.name, expected_name)
            self.assertEqual(p.cache_ttl_seconds, expected_ttl)
            self.assertTrue(hasattr(p, "retrieve_quota"))

    def test_active_providers_registry(self):
        self.assertEqual(len(ACTIVE_PROVIDERS), 5)
        keys = [p.key for p in ACTIVE_PROVIDERS]
        self.assertEqual(keys, ["claude", "codex", "copilot", "gemini", "antigravity"])
        for p in ACTIVE_PROVIDERS:
            self.assertIsInstance(p, BaseProvider)


if __name__ == "__main__":
    main()
