"""Scheduling must keep coverage while respecting budgets and explicit overrides."""

import importlib
import unittest
from unittest.mock import patch

verifier = importlib.import_module("verify-all")


class SchedulingTests(unittest.TestCase):
    def test_small_runners_keep_the_established_queue_and_bound_concurrency(self):
        for budget, workers in [(1, 2), (2, 3), (3, 4), (4, 4)]:
            with self.subTest(budget=budget):
                actual, order = verifier.choose_schedule(budget, None, "auto")
                self.assertEqual(actual, workers)
                self.assertEqual(order, list(verifier.SUITES))

    def test_larger_budgets_use_the_measured_limit_without_losing_suites(self):
        workers, order = verifier.choose_schedule(16, None, "auto")
        self.assertEqual(workers, 5)
        self.assertEqual(order[0], "workspace")
        self.assertEqual(set(order), set(verifier.SUITES))
        self.assertEqual(len(order), len(verifier.SUITES))

    def test_every_platform_schedule_runs_all_registered_suites(self):
        for platform in ["linux", "darwin", "win32"]:
            with self.subTest(platform=platform), patch.object(verifier.sys, "platform", platform):
                order = verifier.suite_order()
                self.assertEqual(set(order), set(verifier.SUITES))
                self.assertEqual(len(order), len(verifier.SUITES))

    def test_explicit_overrides_win_and_task_count_caps_the_pool(self):
        workers, order = verifier.choose_schedule(1, 5, "longest")
        self.assertEqual(workers, 5)
        self.assertEqual(order[0], "workspace")
        workers, order = verifier.choose_schedule(16, 100, "declared")
        self.assertEqual(workers, len(verifier.SUITES))
        self.assertEqual(order, list(verifier.SUITES))


if __name__ == "__main__":
    unittest.main()
