import base64
import json
import sqlite3
import sys
import tempfile
from pathlib import Path
from unittest import TestCase, main, mock


ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from aifuel import shared
from aifuel.providers import antigravity


class AntigravityMacFallbackTests(TestCase):
    def test_fetch_antigravity_does_not_infer_period_from_reset_countdown(self):
        reset_at = 2_000_000
        load_code_assist = {
            "currentTier": {"id": "antigravity", "name": "antigravity"},
            "cloudaicompanionProject": {"id": "project"},
        }
        quota = {
            "buckets": [{
                "modelId": "weekly-model",
                "remainingFraction": 0.5,
                "resetTime": reset_at,
            }],
        }

        with mock.patch.object(antigravity, "_antigravity_keychain_creds",
                               return_value=(None, None, True)), \
             mock.patch.object(antigravity, "_antigravity_app_token",
                               return_value="desktop-token"), \
             mock.patch.object(antigravity, "_antigravity_token",
                               return_value=(None, None, True)), \
             mock.patch.object(antigravity, "_antigravity_project", return_value=None), \
             mock.patch.object(antigravity.os.path, "isdir", return_value=False), \
             mock.patch.object(shared, "now_ts", return_value=reset_at - 18_000), \
             mock.patch.object(shared, "http_get", side_effect=[
                 (load_code_assist, None),
                 (quota, None),
             ]):
            res = antigravity.fetch_antigravity()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(
            [(w["label"], w["period"]) for w in res["windows"]],
            [("weekly-model", "unknown")],
        )

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


if __name__ == "__main__":
    main()
