"""CI acceptance must distinguish a submission terminal from background session events."""

import copy
import importlib
import json
import subprocess
import unittest

coding = importlib.import_module("verify-coding")


class SubmissionEventsTests(unittest.TestCase):
    def stream(self):
        return [
            {"sequence": 7, "run_id": 2, "kind": "accepted", "payload": {}},
            {
                "sequence": 8,
                "run_id": 2,
                "kind": "settled",
                "payload": {"outcome": {"status": "completed"}, "cleanup_errors": []},
            },
            {
                "sequence": 9,
                "run_id": 0,
                "kind": "update_check",
                "payload": {"status": "failed"},
            },
        ]

    def result(self, data, returncode=0):
        return subprocess.CompletedProcess(
            ["eden", "--json"], returncode, "\n".join(json.dumps(item) for item in data), ""
        )

    def test_background_update_after_settlement_preserves_the_complete_stream(self):
        data = self.stream()
        self.assertEqual(coding.events(self.result(data)), data)
        self.assertEqual(coding.events(self.result(data[:-1])), data[:-1])
        data[1]["payload"]["outcome"]["status"] = "cancelled"
        self.assertEqual(coding.events(self.result(data, 1), completed=False), data)

    def test_background_tail_cannot_hide_invalid_submission_or_global_sequence(self):
        original = self.stream()
        cases = []
        for field, value in [("kind", "progress"), ("run_id", 3)]:
            data = copy.deepcopy(original)
            data[1][field] = value
            cases.append(data)
        data = copy.deepcopy(original)
        data[2]["run_id"] = 2
        cases.append(data)
        data = copy.deepcopy(original)
        data[2]["sequence"] = 10
        cases.append(data)
        data = copy.deepcopy(original)
        data[1]["payload"]["cleanup_errors"] = ["cleanup failed"]
        cases.append(data)
        data = copy.deepcopy(original)
        data[1]["payload"]["outcome"]["status"] = "failed"
        cases.append(data)
        for data in cases:
            with self.subTest(data=data), self.assertRaises(AssertionError):
                coding.events(self.result(data))
        with self.assertRaises(AssertionError):
            coding.events(self.result(original, 1))


if __name__ == "__main__":
    unittest.main()
