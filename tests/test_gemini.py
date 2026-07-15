import json
import sys
import tempfile
from pathlib import Path
from unittest import TestCase, main, mock


ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from aifuel import shared
from aifuel.providers import gemini


STANDARD_LOAD_CODE_ASSIST = {
    "currentTier": {
        "id": "standard-tier",
        "name": "Standard",
        "userDefinedCloudaicompanionProject": True,
    },
}
QUOTA = {
    "buckets": [{
        "modelId": "gemini-2.5-pro",
        "remainingFraction": 0.5,
        "resetTime": 2_000,
    }],
}


class GeminiFetchTests(TestCase):
    def test_fetch_gemini_reports_unreadable_project_config(self):
        creds = {"access_token": "token"}
        cred_path = gemini.os.path.join(shared.HOME, ".gemini", "oauth_creds.json")
        env_path = gemini.os.path.join(gemini.os.getcwd(), ".gemini", ".env")
        with mock.patch.object(gemini.os.path, "exists",
                               side_effect=lambda path: path in (cred_path, env_path)), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "http_get",
                               return_value=(STANDARD_LOAD_CODE_ASSIST, None)), \
             mock.patch("builtins.open", side_effect=PermissionError("permission denied")), \
             mock.patch.dict(gemini.os.environ, {}, clear=True):
            res = gemini.fetch_gemini()

        self.assertEqual(res["status"], "error")
        self.assertIn("Could not read Gemini project configuration", res["detail"])
        self.assertIn("PermissionError", res["detail"])
        self.assertIn("Check the file permissions", res["detail"])

    def test_fetch_gemini_matches_cli_env_file_precedence(self):
        cases = [
            (
                "ancestor plain env",
                {"workspace/.env": "GOOGLE_CLOUD_PROJECT_ID=ancestor-project\n"},
                "workspace/src/feature",
                "ancestor-project",
            ),
            (
                "gemini env before plain env",
                {
                    "workspace/.gemini/.env": "GOOGLE_CLOUD_PROJECT_ID=gemini-project\n",
                    "workspace/.env": "GOOGLE_CLOUD_PROJECT=plain-project\n",
                },
                "workspace/src/feature",
                "gemini-project",
            ),
            (
                "home gemini env before home env",
                {
                    "home/.gemini/.env": "GOOGLE_CLOUD_PROJECT_ID=home-gemini-project\n",
                    "home/.env": "GOOGLE_CLOUD_PROJECT=home-plain-project\n",
                },
                "outside/workspace",
                "home-gemini-project",
            ),
            (
                "home plain env fallback",
                {"home/.env": "GOOGLE_CLOUD_PROJECT_ID=home-plain-project\n"},
                "outside/workspace",
                "home-plain-project",
            ),
        ]

        for name, files, cwd, expected_project in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as td:
                root = Path(td)
                home = root / "home"
                credentials = home / ".gemini" / "oauth_creds.json"
                credentials.parent.mkdir(parents=True)
                credentials.write_text(
                    json.dumps({"access_token": "token"}),
                    encoding="utf-8",
                )

                existing_paths = {str(credentials)}
                for relative_path, contents in files.items():
                    env_path = root / relative_path
                    env_path.parent.mkdir(parents=True, exist_ok=True)
                    env_path.write_text(contents, encoding="utf-8")
                    existing_paths.add(str(env_path))

                cwd_path = root / cwd
                cwd_path.mkdir(parents=True)
                requested_projects = []

                def http_get(url, **kwargs):
                    if url.endswith(":loadCodeAssist"):
                        return STANDARD_LOAD_CODE_ASSIST, None
                    requested_projects.append(kwargs["data"]["project"])
                    return QUOTA, None

                with mock.patch.object(shared, "HOME", str(home)), \
                     mock.patch.object(gemini.os, "getcwd", return_value=str(cwd_path)), \
                     mock.patch.object(gemini.os.path, "exists",
                                       side_effect=lambda path: path in existing_paths), \
                     mock.patch.object(shared, "http_get", side_effect=http_get), \
                     mock.patch.dict(gemini.os.environ, {}, clear=True):
                    res = gemini.fetch_gemini()

                self.assertEqual(res["status"], "ok")
                self.assertEqual(requested_projects, [expected_project])

    def test_fetch_gemini_explains_how_to_configure_required_project(self):
        creds = {"access_token": "token"}
        cred_path = gemini.os.path.join(shared.HOME, ".gemini", "oauth_creds.json")

        with mock.patch.object(gemini.os.path, "exists",
                               side_effect=lambda path: path == cred_path), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "http_get",
                               return_value=(STANDARD_LOAD_CODE_ASSIST, None)), \
             mock.patch.dict(gemini.os.environ, {}, clear=True):
            res = gemini.fetch_gemini()

        self.assertEqual(res["status"], "error")
        self.assertEqual(
            res["detail"],
            "Standard/Enterprise tier needs a Google Cloud project. "
            "Add GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID to .gemini/.env "
            "and refresh, or export one before starting aifuel and restart.",
        )

    def test_fetch_gemini_honors_cli_project_id_alias(self):
        creds = {"access_token": "token"}
        cred_path = gemini.os.path.join(shared.HOME, ".gemini", "oauth_creds.json")

        with mock.patch.object(gemini.os.path, "exists",
                               side_effect=lambda path: path == cred_path), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "http_get",
                               side_effect=[(STANDARD_LOAD_CODE_ASSIST, None),
                                            (QUOTA, None)]) as http_get, \
             mock.patch.dict(gemini.os.environ,
                             {"GOOGLE_CLOUD_PROJECT_ID": "valid-org-project"},
                             clear=True):
            res = gemini.fetch_gemini()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["source"], "live")
        self.assertEqual(
            http_get.call_args_list[1].kwargs["data"]["project"],
            "valid-org-project",
        )

    def test_fetch_gemini_prefers_primary_project_variable(self):
        creds = {"access_token": "token"}
        cred_path = gemini.os.path.join(shared.HOME, ".gemini", "oauth_creds.json")
        requested_projects = []

        def http_get(url, **kwargs):
            if url.endswith(":loadCodeAssist"):
                return STANDARD_LOAD_CODE_ASSIST, None
            requested_projects.append(kwargs["data"]["project"])
            return QUOTA, None

        with mock.patch.object(gemini.os.path, "exists",
                               side_effect=lambda path: path == cred_path), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(shared, "http_get", side_effect=http_get), \
             mock.patch.dict(gemini.os.environ, {
                 "GOOGLE_CLOUD_PROJECT": "primary-project",
                 "GOOGLE_CLOUD_PROJECT_ID": "alias-project",
             }, clear=True):
            res = gemini.fetch_gemini()

        self.assertEqual(res["status"], "ok")
        self.assertEqual(requested_projects, ["primary-project"])

    def test_fetch_gemini_errors_when_live_quota_fails(self):
        creds = {"access_token": "token", "refresh_token": "refresh"}

        with mock.patch.object(gemini.os.path, "exists", return_value=True), \
             mock.patch.object(shared, "read_json", return_value=creds), \
             mock.patch.object(gemini, "_codeassist_quota",
                               return_value=("fallback", "Gemini Code Assist", [],
                                             "retrieveUserQuota HTTP 403")), \
             mock.patch.object(gemini, "_refresh_gemini_token", return_value=None):
            res = gemini.fetch_gemini()

        self.assertEqual(res["status"], "error")
        self.assertIsNone(res["source"])
        self.assertEqual(res["plan"], "Gemini Code Assist")
        self.assertIn("retrieveUserQuota HTTP 403", res["detail"])


if __name__ == "__main__":
    main()
