import importlib.util
import sys
import threading
import urllib.request
from pathlib import Path
from unittest import TestCase, main, mock


ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from aifuel import shared
from aifuel.providers import ClaudeProvider


SPEC = importlib.util.spec_from_file_location("aifuel_cli_edge_cases", SRC / "aifuel.py")
aifuel_cli = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(aifuel_cli)


class ProviderDiscoveryFailureTests(TestCase):
    def test_collect_reports_inaccessible_provider_credential_source(self):
        with mock.patch.object(
            aifuel_cli,
            "SUPPORTED_PROVIDER_CLASSES",
            [ClaudeProvider],
        ), mock.patch.object(shared.os, "stat", side_effect=PermissionError(
            "credential store unavailable",
        )):
            data = aifuel_cli.collect(force=True)

        self.assertEqual(data["providers"], [])
        self.assertEqual(data["discovery_errors"], [{
            "provider": "Claude",
            "detail": "PermissionError: credential store unavailable",
        }])


class DashboardAssetTests(TestCase):
    def test_dashboard_serves_its_stylesheet(self):
        server = aifuel_cli.ThreadingHTTPServer(("127.0.0.1", 0), aifuel_cli.Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)

        with urllib.request.urlopen(
            f"http://127.0.0.1:{server.server_port}/dashboard.css",
        ) as response:
            body = response.read().decode("utf-8")
            content_type = response.headers.get_content_type()

        self.assertEqual(content_type, "text/css")
        self.assertIn("--radius-card", body)


if __name__ == "__main__":
    main()
