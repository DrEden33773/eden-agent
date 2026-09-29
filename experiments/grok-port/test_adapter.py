"""Behavior checks for the experimental host-to-pager boundary."""

import unittest

from adapter import Projection


class ProjectionTests(unittest.TestCase):
    def test_cancelled_attempt_text_survives_replay(self):
        updates = []
        projection = Projection(lambda update, replay: updates.append(update))
        projection.record(
            {
                "sequence": 1,
                "run_id": 7,
                "kind": "model_attempt",
                "payload": {"attempt_id": "a", "status": "cancelled", "text": "partial answer"},
            },
            replay=True,
        )
        self.assertEqual("".join(u["content"]["text"] for u in updates), "partial answer")

    def test_durable_commit_does_not_repeat_streamed_text(self):
        updates = []
        projection = Projection(lambda update, replay: updates.append(update))
        projection.apply(
            {
                "history": [
                    {
                        "sequence": 1,
                        "run_id": 7,
                        "kind": "message",
                        "payload": {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "text", "text": "Hello world"}],
                        },
                    }
                ],
                "events": [
                    {
                        "sequence": 1,
                        "run_id": 7,
                        "kind": "model_text_delta",
                        "payload": {"delta": "Hello "},
                    },
                    {"sequence": 2, "run_id": 7, "kind": "committed", "payload": {"sequence": 1}},
                ],
            }
        )
        self.assertEqual("".join(u["content"]["text"] for u in updates), "Hello world")

    def test_tool_ids_are_scoped_to_run_and_failed_output_is_retained(self):
        updates = []
        projection = Projection(lambda update, replay: updates.append(update))
        for sequence, run in enumerate([7, 8], 1):
            projection.record(
                {
                    "sequence": sequence,
                    "run_id": run,
                    "kind": "tool_call",
                    "payload": {
                        "type": "tool_call",
                        "call_id": "same",
                        "name": "bash",
                        "arguments": '{"command":"exit 7"}',
                    },
                }
            )
        projection.record(
            {
                "sequence": 3,
                "run_id": 8,
                "kind": "tool_result",
                "payload": {
                    "type": "tool_result",
                    "call_id": "same",
                    "result": {"text": "failure output", "exit_code": 7},
                },
            }
        )
        self.assertEqual([u["toolCallId"] for u in updates], ["7:same", "8:same", "8:same"])
        self.assertEqual(updates[-1]["status"], "failed")
        self.assertEqual(bytes(updates[-1]["rawOutput"]["output"]).decode(), "failure output")

    def test_replay_comes_from_history_without_resending_prompt(self):
        updates = []
        projection = Projection(lambda update, replay: updates.append((update, replay)))
        snapshot = {
            "history": [
                {
                    "sequence": 1,
                    "run_id": 7,
                    "kind": "message",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "text", "text": "saved prompt"}],
                    },
                }
            ],
            "events": [],
        }
        projection.apply(snapshot, replay=True)
        projection.apply(snapshot)
        self.assertEqual(len(updates), 1)
        self.assertTrue(updates[0][1])
        self.assertEqual(updates[0][0]["sessionUpdate"], "user_message_chunk")


if __name__ == "__main__":
    unittest.main()
