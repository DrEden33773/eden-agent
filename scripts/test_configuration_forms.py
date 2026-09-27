"""The native acceptance driver must not equate clock ticks with request identities."""

import importlib.util
import pathlib
import unittest
from typing import Any
from unittest.mock import patch


class ConfigurationFormsTests(unittest.TestCase):
    def test_headless_actions_have_distinct_ids_even_when_clock_does_not_advance(self):
        source = pathlib.Path(__file__).with_name("verify-configuration-forms.py")
        spec = importlib.util.spec_from_file_location("configuration_acceptance", source)
        assert spec and spec.loader
        driver = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(driver)
        seen: set[str] = set()

        def call(_endpoint: dict, route: str, body: Any = None) -> dict:
            if route == "/action":
                self.assertNotIn(body["request_id"], seen, "distinct actions reused one ID")
                seen.add(body["request_id"])
                return {"errors": [], "status": "applied"}
            return {}

        with (
            patch.object(driver, "call", side_effect=call),
            patch.object(
                driver,
                "form",
                return_value=(
                    {"owner": "host", "id": "settings", "revision": 1},
                    {"id": "settings", "binding": {}},
                ),
            ),
            patch.object(driver.time, "monotonic_ns", return_value=1),
        ):
            result = driver.headless({"session_id": 1}, None, "test has no browser")
        self.assertEqual(len(seen), 24)
        self.assertEqual(len(result["receipts"]), 8)


if __name__ == "__main__":
    unittest.main()
