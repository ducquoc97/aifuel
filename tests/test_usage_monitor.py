import importlib.util
import io
import tempfile
import urllib.error
from pathlib import Path
from unittest import TestCase, main, mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("usage_monitor", ROOT / "src" / "usage_monitor.py")
usage_monitor = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(usage_monitor)


class QuotaWindowTests(TestCase):
    def test_period_override_preserves_daily_label_near_reset(self):
        quota = {
            "buckets": [{
                "modelId": "gemini-2.5-pro",
                "remainingFraction": 0.5,
                "resetTime": 1_000 + 5 * 3600,
            }]
        }

        with mock.patch.object(usage_monitor, "now_ts", return_value=1_000):
            inferred = usage_monitor._quota_windows(quota)[0]
            daily = usage_monitor._quota_windows(quota, period_override="daily")[0]

        self.assertEqual(inferred["period"], "5h")
        self.assertEqual(daily["period"], "daily")


class ClaudeRefreshTests(TestCase):
    def test_refresh_claude_token_uses_stored_client_id(self):
        creds = {
            "claudeAiOauth": {
                "refreshToken": "old-refresh",
                "clientId": "custom-client",
            }
        }

        with mock.patch.object(usage_monitor, "_claude_oauth_refresh", return_value={
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
        }) as refresh, mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "write_json_atomic"):
            usage_monitor._refresh_claude_token(creds, "/tmp/claude-creds.json")

        refresh.assert_called_once_with("old-refresh", "custom-client")

    def test_refresh_claude_token_falls_back_to_default_client_id(self):
        creds = {"claudeAiOauth": {"refreshToken": "old-refresh"}}

        with mock.patch.object(usage_monitor, "_claude_oauth_refresh", return_value={
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
        }) as refresh, mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "write_json_atomic"):
            usage_monitor._refresh_claude_token(creds, "/tmp/claude-creds.json")

        refresh.assert_called_once_with(
            "old-refresh",
            usage_monitor.CLAUDE_CLI_PUBLIC_CLIENT_ID,
        )

    def test_refresh_claude_token_rotates_refresh_token_and_expiry(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 1,
                "scopes": ["old"],
            }
        }
        path = "/tmp/claude-creds.json"

        with mock.patch.object(usage_monitor, "_claude_oauth_refresh", return_value={
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
            "scope": "user:profile user:inference",
        }), mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "write_json_atomic") as write_json:
            token = usage_monitor._refresh_claude_token(creds, path)

        self.assertEqual(token, "new-access")
        self.assertEqual(creds["claudeAiOauth"]["accessToken"], "new-access")
        self.assertEqual(creds["claudeAiOauth"]["refreshToken"], "new-refresh")
        self.assertEqual(creds["claudeAiOauth"]["expiresAt"], 3_700_000)
        self.assertEqual(creds["claudeAiOauth"]["scopes"], ["user:profile", "user:inference"])
        write_json.assert_called_once_with(path, creds)

    def test_fetch_claude_refreshes_expired_token_before_usage_call(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 0,
                "subscriptionType": "team",
            }
        }
        usage_payload = {"weekly": {"used_percent": 10, "resets_at": 1_000}}

        with mock.patch.object(usage_monitor.os.path, "exists", return_value=True), \
             mock.patch.object(usage_monitor, "read_json", return_value=creds), \
             mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "_refresh_claude_token", return_value="new-access") as refresh, \
             mock.patch.object(usage_monitor, "http_get", return_value=(usage_payload, None)) as http_get:
            res = usage_monitor.fetch_claude()

        self.assertEqual(res["status"], "ok")
        refresh.assert_called_once()
        self.assertEqual(http_get.call_args.kwargs["headers"]["Authorization"], "Bearer new-access")

    def test_fetch_claude_retries_after_401_with_refreshed_token(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 9_999_999_999_999,
                "subscriptionType": "team",
            }
        }
        usage_payload = {"weekly": {"used_percent": 10, "resets_at": 1_000}}
        err = urllib.error.HTTPError(
            "https://api.anthropic.com/api/oauth/usage", 401, "Unauthorized", hdrs=None,
            fp=io.BytesIO(b'{"error":"unauthorized"}'),
        )

        with mock.patch.object(usage_monitor.os.path, "exists", return_value=True), \
             mock.patch.object(usage_monitor, "read_json", return_value=creds), \
             mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "_refresh_claude_token", return_value="new-access") as refresh, \
             mock.patch.object(usage_monitor, "http_get", side_effect=[err, (usage_payload, None)]) as http_get:
            res = usage_monitor.fetch_claude()

        self.assertEqual(res["status"], "ok")
        refresh.assert_called_once()
        self.assertEqual(http_get.call_count, 2)
        self.assertEqual(http_get.call_args.kwargs["headers"]["Authorization"], "Bearer new-access")

    def test_fetch_claude_parses_known_usage_claims_as_used_utilization(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "access",
                "expiresAt": 9_999_999_999_999,
                "subscriptionType": "team",
            }
        }
        usage_payload = {
            "five_hour": {"utilization": 0.04, "resets_at": 1_000},
            "seven_day": {"utilization": 0, "resets_at": 2_000},
            "unexpected_hourly_metadata": {"utilization": 0.9, "resets_at": 3_000},
        }

        with mock.patch.object(usage_monitor.os.path, "exists", return_value=True), \
             mock.patch.object(usage_monitor, "read_json", return_value=creds), \
             mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "http_get", return_value=(usage_payload, None)):
            res = usage_monitor.fetch_claude()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["plan"], "team")
        self.assertEqual(
            [(w["label"], w["period"], w["used_percent"], w["remaining_percent"])
             for w in res["windows"]],
            [
                ("Current session", "5h", 4.0, 96.0),
                ("Current week (all models)", "weekly", 0.0, 100.0),
            ],
        )

    def test_fetch_claude_prefers_explicit_used_percentage_fields(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "access",
                "expiresAt": 9_999_999_999_999,
                "subscriptionType": "team",
            }
        }
        usage_payload = {
            "five_hour": {
                "utilization": 0.04,
                "used_percentage": 60,
                "remaining_percentage": 40,
                "resets_at": 1_000,
            },
            "seven_day": {
                "utilization": 0,
                "used_percentage": 8,
                "remaining_percentage": 92,
                "resets_at": 2_000,
            },
            "seven_day_sonnet": {
                "used_percentage": 3,
                "remaining_percentage": 97,
                "resets_at": 2_000,
            },
        }

        with mock.patch.object(usage_monitor.os.path, "exists", return_value=True), \
             mock.patch.object(usage_monitor, "read_json", return_value=creds), \
             mock.patch.object(usage_monitor, "now_ts", return_value=100), \
             mock.patch.object(usage_monitor, "http_get", return_value=(usage_payload, None)):
            res = usage_monitor.fetch_claude()

        self.assertEqual(
            [(w["label"], w["used_percent"], w["remaining_percent"]) for w in res["windows"]],
            [
                ("Current session", 60.0, 40.0),
                ("Current week (all models)", 8.0, 92.0),
                ("Current week (Sonnet only)", 3.0, 97.0),
            ],
        )


class ProviderCacheTests(TestCase):
    def setUp(self):
        usage_monitor._cache.clear()
        self.snapshot_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.snapshot_dir.cleanup)
        patcher = mock.patch.object(usage_monitor, "SNAPSHOT_DIR", self.snapshot_dir.name)
        patcher.start()
        self.addCleanup(patcher.stop)

    def tearDown(self):
        usage_monitor._cache.clear()

    def test_resetless_results_are_retried_quickly(self):
        calls = 0

        def fetch():
            nonlocal calls
            calls += 1
            return usage_monitor.result("claude", "Claude Code", "error", detail="boom")

        with mock.patch.object(usage_monitor, "read_snapshot", return_value=None), \
             mock.patch.object(usage_monitor, "now_ts", side_effect=[0, 10, 16, 16]):
            usage_monitor.get_provider("claude", fetch, 180)
            usage_monitor.get_provider("claude", fetch, 180)
            usage_monitor.get_provider("claude", fetch, 180)

        self.assertEqual(calls, 2)

    def test_results_with_reset_keep_full_ttl(self):
        calls = 0

        def fetch():
            nonlocal calls
            calls += 1
            return usage_monitor.result(
                "claude", "Claude Code", "ok",
                windows=[usage_monitor.window("Weekly", "weekly", resets_at=1_000)],
            )

        with mock.patch.object(usage_monitor, "now_ts", side_effect=[0, 100]):
            usage_monitor.get_provider("claude", fetch, 180)
            usage_monitor.get_provider("claude", fetch, 180)

        self.assertEqual(calls, 1)
        self.assertEqual(usage_monitor._cache["claude"][1]["windows"][0]["resets_at"], 1_000)

    def test_resetless_refresh_keeps_last_good_result(self):
        good = usage_monitor.result(
            "claude", "Claude Code", "ok",
            windows=[usage_monitor.window("Weekly", "weekly", resets_at=1_000)],
        )
        usage_monitor._cache["claude"] = (0, good)

        with mock.patch.object(usage_monitor, "now_ts", return_value=100):
            res = usage_monitor.get_provider(
                "claude",
                lambda: usage_monitor.result("claude", "Claude Code", "error", detail="boom"),
                180,
            )

        self.assertEqual(res["source"], "local-cache")
        self.assertEqual(res["windows"], good["windows"])

    def test_resetless_refresh_does_not_reuse_stale_memory_result(self):
        stale = usage_monitor.result(
            "claude", "Claude Code", "ok",
            windows=[usage_monitor.window("Weekly", "weekly", resets_at=1_000)],
        )
        error = usage_monitor.result("claude", "Claude Code", "error", detail="boom")
        usage_monitor._cache["claude"] = (0, stale)

        with mock.patch.object(usage_monitor, "read_snapshot", return_value=None), \
             mock.patch.object(usage_monitor, "now_ts", return_value=1_400):
            res = usage_monitor.get_provider("claude", lambda: error, 180)

        self.assertIs(res, error)
        self.assertIs(usage_monitor._cache["claude"][1], error)

    def test_resetless_refresh_uses_disk_snapshot_without_memory_cache(self):
        snap = usage_monitor.result(
            "claude", "Claude Code", "ok",
            windows=[usage_monitor.window("Weekly", "weekly", resets_at=1_000)],
        )

        with mock.patch.object(usage_monitor, "read_snapshot", return_value=snap):
            res = usage_monitor.get_provider(
                "claude",
                lambda: usage_monitor.result("claude", "Claude Code", "error", detail="boom"),
                180,
            )

        self.assertEqual(res["source"], "local-cache")
        self.assertEqual(res["windows"], snap["windows"])


if __name__ == "__main__":
    main()
