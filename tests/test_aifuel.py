import importlib.util
import base64
import io
import json
import sqlite3
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
    antigravity,
    claude,
    codex,
    copilot,
    gemini,
)


SPEC = importlib.util.spec_from_file_location("aifuel_cli", SRC / "aifuel.py")
aifuel_cli = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(aifuel_cli)


class QuotaWindowTests(TestCase):
    def test_period_override_preserves_daily_label_near_reset(self):
        quota = {
            "buckets": [{
                "modelId": "gemini-2.5-pro",
                "remainingFraction": 0.5,
                "resetTime": 1_000 + 5 * 3600,
            }]
        }

        with mock.patch.object(shared, "now_ts", return_value=1_000):
            inferred = gemini._quota_windows(quota)[0]
            daily = gemini._quota_windows(quota, period_override="daily")[0]

        self.assertEqual(inferred["period"], "5h")
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


class ClaudeRefreshTests(TestCase):
    def test_refresh_claude_token_uses_stored_client_id(self):
        creds = {
            "claudeAiOauth": {
                "refreshToken": "old-refresh",
                "clientId": "custom-client",
            }
        }

        with mock.patch.object(claude, "_claude_oauth_refresh", return_value={
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
        }) as refresh, mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "write_json_atomic"):
            claude._refresh_claude_token(creds, "/tmp/claude-creds.json")

        refresh.assert_called_once_with("old-refresh", "custom-client")

    def test_refresh_claude_token_falls_back_to_default_client_id(self):
        creds = {"claudeAiOauth": {"refreshToken": "old-refresh"}}

        with mock.patch.object(claude, "_claude_oauth_refresh", return_value={
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
        }) as refresh, mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "write_json_atomic"):
            claude._refresh_claude_token(creds, "/tmp/claude-creds.json")

        refresh.assert_called_once_with(
            "old-refresh",
            shared.CLAUDE_CLI_PUBLIC_CLIENT_ID,
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

        with mock.patch.object(claude, "_claude_oauth_refresh", return_value={
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
            "scope": "user:profile user:inference",
        }), mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "write_json_atomic") as write_json:
            token = claude._refresh_claude_token(creds, path)

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

        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(claude, "_refresh_claude_token", return_value="new-access") as refresh, \
             mock.patch.object(shared, "http_get", return_value=(usage_payload, None)) as http_get:
            res = claude.fetch_claude()

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

        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(claude, "_refresh_claude_token", return_value="new-access") as refresh, \
             mock.patch.object(shared, "http_get", side_effect=[err, (usage_payload, None)]) as http_get:
            res = claude.fetch_claude()

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

        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "http_get", return_value=(usage_payload, None)):
            res = claude.fetch_claude()

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

        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "http_get", return_value=(usage_payload, None)):
            res = claude.fetch_claude()

        self.assertEqual(
            [(w["label"], w["used_percent"], w["remaining_percent"]) for w in res["windows"]],
            [
                ("Current session", 60.0, 40.0),
                ("Current week (all models)", 8.0, 92.0),
                ("Current week (Sonnet only)", 3.0, 97.0),
            ],
        )

    def test_fetch_claude_errors_when_response_has_no_usage_windows(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "access",
                "expiresAt": 9_999_999_999_999,
                "subscriptionType": "team",
            }
        }

        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "http_get", return_value=({"ignored": {}}, None)):
            res = claude.fetch_claude()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertIn("no usage windows", res["detail"])

    def test_fetch_claude_errors_on_http_failure_scenarios(self):
        creds = {
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 9_999_999_999_999,
                "subscriptionType": "team",
            }
        }
        err_401 = urllib.error.HTTPError(
            "https://api.anthropic.com/api/oauth/usage", 401, "Unauthorized", hdrs=None, fp=io.BytesIO(b"{}"),
        )
        err_500 = urllib.error.HTTPError(
            "https://api.anthropic.com/api/oauth/usage", 500, "Internal Server Error", hdrs=None, fp=io.BytesIO(b"{}"),
        )

        # 1. 401 initially and refresh fails (returns None) -> expect friendly error
        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(claude, "_refresh_claude_token", return_value=None), \
             mock.patch.object(shared, "http_get", side_effect=err_401):
            res = claude.fetch_claude()
        self.assertEqual(res["status"], "error")
        self.assertEqual(res["detail"], "OAuth token expired and auto-refresh failed")

        # 2. 401 initially, refresh succeeds, but new token still returns 401 -> expect friendly error
        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(claude, "_refresh_claude_token", return_value="new-access"), \
             mock.patch.object(shared, "http_get", side_effect=[err_401, err_401]):
            res = claude.fetch_claude()
        self.assertEqual(res["status"], "error")
        self.assertEqual(res["detail"], "OAuth token expired and auto-refresh failed")

        # 3. 500 initially -> expect standard HTTP 500 error
        with mock.patch.object(claude.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "http_get", side_effect=err_500):
            res = claude.fetch_claude()
        self.assertEqual(res["status"], "error")
        self.assertIn("HTTP 500", res["detail"])


class CodexFetchTests(TestCase):
    def test_fetch_codex_returns_live_usage(self):
        auth = {"access_token": "token", "account_id": "acct"}
        payload = {
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "limit_window_seconds": 18_000,
                    "used_percent": 25,
                    "reset_after_seconds": 60,
                },
                "secondary_window": {
                    "limit_window_seconds": 604_800,
                    "used_percent": 10,
                    "reset_after_seconds": 3600,
                },
            },
            "additional_rate_limits": [{
                "limit_name": "Codex-Spark",
                "rate_limit": {
                    "primary_window": {
                        "limit_window_seconds": 86_400,
                        "used_percent": 5,
                        "reset_after_seconds": 120,
                    },
                },
            }],
        }

        with mock.patch.object(codex.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=auth), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "http_get", return_value=(payload, None)):
            res = codex.fetch_codex()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["source"], "live")
        self.assertEqual(res["plan"], "pro")
        self.assertEqual(
            [(w["label"], w["period"], w["used_percent"], w["resets_at"]) for w in res["windows"]],
            [
                ("5-hour", "5h", 25, 160),
                ("Weekly", "weekly", 10, 3700),
                ("Codex-Spark", "daily", 5, 220),
            ],
        )

    def test_fetch_codex_requires_live_auth(self):
        with mock.patch.object(codex.os.path, "exists", return_value=False):
            res = codex.fetch_codex()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertIn("missing access_token", res["detail"])

    def test_fetch_codex_returns_error_on_http_failure(self):
        auth = {"access_token": "token", "account_id": "acct"}
        err = urllib.error.HTTPError(
            shared.CODEX_USAGE_URL, 401, "Unauthorized", hdrs=None, fp=io.BytesIO(b"{}"),
        )

        with mock.patch.object(codex.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=auth), \
             mock.patch.object(shared, "http_get", side_effect=err):
            res = codex.fetch_codex()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertIn("Token expired — run the Codex CLI once to refresh", res["detail"])

        err_500 = urllib.error.HTTPError(
            shared.CODEX_USAGE_URL, 500, "Internal Server Error", hdrs=None, fp=io.BytesIO(b"{}"),
        )
        with mock.patch.object(codex.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=auth), \
             mock.patch.object(shared, "http_get", side_effect=err_500):
            res = codex.fetch_codex()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertIn("HTTP 500", res["detail"])


class CopilotFetchTests(TestCase):
    def test_fetch_copilot_errors_when_live_usage_unreachable(self):
        with mock.patch.object(copilot, "_copilot_token", return_value=("token", None)), \
             mock.patch.object(shared, "http_get", side_effect=Exception("boom")):
            res = copilot.fetch_copilot()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertIn("live usage endpoint unreachable", res["detail"])

    def test_fetch_copilot_errors_when_unauthorized(self):
        err = urllib.error.HTTPError(
            "https://api.github.com/copilot_internal/user", 401, "Unauthorized", hdrs=None, fp=io.BytesIO(b"{}"),
        )
        with mock.patch.object(copilot, "_copilot_token", return_value=("token", None)), \
             mock.patch.object(shared, "http_get", side_effect=err):
            res = copilot.fetch_copilot()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertIn("Token expired/unauthorized — sign in using the GitHub or Copilot CLI", res["detail"])


class GeminiFetchTests(TestCase):
    def test_fetch_gemini_errors_when_live_quota_fails(self):
        creds = {"access_token": "token", "refresh_token": "refresh"}

        with mock.patch.object(gemini.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(gemini, "_codeassist_quota",
                               return_value=("fallback", "Gemini Code Assist", [], "retrieveUserQuota HTTP 403")), \
             mock.patch.object(gemini, "_refresh_gemini_token", return_value=None):
            res = gemini.fetch_gemini()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertEqual(res["plan"], "Gemini Code Assist")
        self.assertIn("retrieveUserQuota HTTP 403", res["detail"])


class AntigravityMacFallbackTests(TestCase):
    def _write_state_db(self, path, auth_status):
        con = sqlite3.connect(path)
        try:
            con.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")
            con.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
                ("antigravityAuthStatus", json.dumps(auth_status)),
            )
            con.commit()
        finally:
            con.close()

    def test_antigravity_app_token_reads_state_db(self):
        with tempfile.TemporaryDirectory() as td:
            db_path = Path(td) / "state.vscdb"
            self._write_state_db(db_path, {
                "name": "Quoc",
                "email": "quoc@example.com",
                "apiKey": "desktop-token",
            })

            with mock.patch.object(antigravity, "_antigravity_app_state_dbs",
                                   return_value=[str(db_path)]):
                token = antigravity._antigravity_app_token()

        self.assertEqual(token, "desktop-token")

    def test_fetch_antigravity_uses_desktop_token_when_cli_token_missing(self):
        windows = [shared.window(
            "Gemini 3.5 Flash", "5h", remaining_percent=62, resets_at=1_000,
        )]

        with mock.patch.object(antigravity, "_antigravity_app_token",
                               return_value="desktop-token"), \
             mock.patch.object(antigravity, "_antigravity_keychain_creds",
                               return_value=(None, None, True)), \
             mock.patch.object(antigravity, "_antigravity_token",
                               return_value=(None, None, True)), \
             mock.patch.object(antigravity, "_antigravity_project", return_value=None), \
             mock.patch.object(antigravity.gemini, "_codeassist_quota",
                               return_value=("live", "antigravity", windows, None)) as quota, \
             mock.patch.object(antigravity.gemini, "_rank_models", side_effect=lambda w: w), \
             mock.patch.object(antigravity.os.path, "isdir", return_value=False):
            res = antigravity.fetch_antigravity()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["source"], "live")
        self.assertEqual(res["plan"], "Antigravity Starter Quota")
        self.assertEqual(res["windows"], windows)
        quota.assert_called_once_with("desktop-token", None, "antigravity/usage-monitor")

    def test_antigravity_keychain_creds_decodes_go_keyring_secret(self):
        payload = {
            "auth_method": "consumer",
            "token": {
                "access_token": "keychain-token",
                "refresh_token": "refresh-token",
                "token_type": "Bearer",
                "expiry": "2099-01-01T00:00:00+00:00",
            },
        }
        secret = "go-keyring-base64:" + base64.b64encode(
            json.dumps(payload).encode("utf-8")
        ).decode("ascii")

        with mock.patch.object(shared, "read_keychain_secret", return_value=secret), \
             mock.patch.object(shared, "now_ts", return_value=100):
            creds, token, expired = antigravity._antigravity_keychain_creds()

        self.assertEqual(creds["auth_method"], "consumer")
        self.assertEqual(token, "keychain-token")
        self.assertFalse(expired)

    def test_fetch_antigravity_uses_keychain_token_before_desktop_fallback(self):
        windows = [shared.window(
            "Gemini 3.5 Flash", "5h", remaining_percent=62, resets_at=1_000,
        )]

        with mock.patch.object(antigravity, "_antigravity_keychain_creds",
                               return_value=({"token": {"access_token": "keychain-token"}}, "keychain-token", False)), \
             mock.patch.object(antigravity, "_antigravity_app_token",
                               return_value="desktop-token"), \
             mock.patch.object(antigravity, "_antigravity_token",
                               return_value=(None, None, True)), \
             mock.patch.object(antigravity, "_antigravity_project", return_value=None), \
             mock.patch.object(antigravity.gemini, "_codeassist_quota",
                               return_value=("live", "antigravity", windows, None)) as quota, \
             mock.patch.object(antigravity.gemini, "_rank_models", side_effect=lambda w: w), \
             mock.patch.object(antigravity.os.path, "isdir", return_value=False):
            res = antigravity.fetch_antigravity()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["source"], "live")
        self.assertEqual(res["plan"], "Antigravity Starter Quota")
        self.assertEqual(res["windows"], windows)
        quota.assert_called_once_with("keychain-token", None, "antigravity/usage-monitor")

    def test_fetch_antigravity_errors_without_live_quota(self):
        with mock.patch.object(antigravity, "_antigravity_keychain_creds",
                               return_value=({"token": {"access_token": "keychain-token"}}, "keychain-token", False)), \
             mock.patch.object(antigravity, "_antigravity_app_token",
                               return_value=None), \
             mock.patch.object(antigravity, "_antigravity_token",
                               return_value=(None, None, True)), \
             mock.patch.object(antigravity, "_antigravity_project", return_value=None), \
             mock.patch.object(antigravity.gemini, "_codeassist_quota",
                               return_value=("fallback", "antigravity", [], "retrieveUserQuota HTTP 403")), \
             mock.patch.object(antigravity.os.path, "isdir", return_value=False):
            res = antigravity.fetch_antigravity()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertEqual(res["plan"], "Antigravity Starter Quota")
        self.assertIn("retrieveUserQuota HTTP 403", res["detail"])


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
            (ClaudeProvider(), "claude", "Claude Code"),
            (CodexProvider(), "codex", "Codex CLI"),
            (CopilotProvider(), "copilot", "GitHub Copilot"),
            (GeminiProvider(), "gemini", "Gemini CLI"),
            (AntigravityProvider(), "antigravity", "Antigravity CLI"),
        ]
        for p, expected_key, expected_name in providers:
            self.assertIsInstance(p, BaseProvider)
            self.assertEqual(p.key, expected_key)
            self.assertEqual(p.name, expected_name)


if __name__ == "__main__":
    main()
