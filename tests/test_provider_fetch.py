import io
import sys
import urllib.error
from pathlib import Path
from unittest import TestCase, main, mock


ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from aifuel import shared
from aifuel.providers import claude, codex, copilot


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
    def test_fetch_codex_labels_weekly_primary_from_duration(self):
        auth = {"access_token": "token", "account_id": "acct"}
        payload = {
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "limit_window_seconds": 604_800,
                    "used_percent": 25,
                    "reset_after_seconds": 60,
                },
            },
        }

        with mock.patch.object(codex.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=auth), \
             mock.patch.object(shared, "now_ts", return_value=100), \
             mock.patch.object(shared, "http_get", return_value=(payload, None)):
            res = codex.fetch_codex()

        labels = [w["label"] for w in res["windows"]]
        self.assertEqual(
            [(w["label"], w["period"]) for w in res["windows"]],
            [("Weekly", "weekly")],
        )
        self.assertNotIn("5-hour", labels)

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
                ("Codex-Spark Daily", "daily", 5, 220),
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


if __name__ == "__main__":
    main()
