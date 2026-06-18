import importlib.util
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


if __name__ == "__main__":
    main()
