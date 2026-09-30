#!/usr/bin/env python3
"""Observe effective context estimates through the native adapter and real pager."""

import argparse
import time
from pathlib import Path

from verify_workflows import ROOT, Fixture


def context_events(fixture):
    assert fixture.rpc is not None
    return [e["params"] for e in fixture.rpc.events if e.get("method") == "_eden/context/state"]


def next_context(fixture, predicate):
    assert fixture.rpc is not None
    deadline = time.monotonic() + 15
    while True:
        event = fixture.rpc.messages.get(timeout=max(0.1, deadline - time.monotonic()))
        fixture.rpc.events.append(event)
        if event.get("method") == "_eden/context/state" and predicate(event["params"]):
            return event["params"]
        assert time.monotonic() < deadline, "expected context transition missing"


def fmt_count(value):
    return (
        f"{value // 1000}K"
        if value >= 10000
        else f"{value / 1000:.1f}K"
        if value >= 1000
        else str(value)
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/s4-g2-session-host")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(
        args.installation, args.output, configuration_author=False, context_footer=True
    ) as fixture:
        assert fixture.rpc is not None
        initial = context_events(fixture)[-1]
        assert initial["estimated"] and initial["used"] > 0 and initial["window"] == 65536, initial
        fixture.terminal_start()
        assert fixture.terminal is not None
        fixture.terminal.wait(" / 65K")
        fixture.capture("empty")
        screen = (args.output / "empty.txt").read_text()
        assert "~" in screen and "░" in screen
        fixture.provider.mode = "short"
        result = fixture.rpc.call(
            "session/prompt",
            {
                "sessionId": fixture.identity,
                "prompt": [
                    {"type": "text", "text": "Context footer acceptance " + "payload " * 1600}
                ],
                "_meta": {"promptId": "context-footer"},
            },
        )
        assert result["stopReason"] == "end_turn"
        deadline = time.monotonic() + 15
        updated = context_events(fixture)[-1]
        while not updated["estimated"] or updated["used"] <= initial["used"]:
            event = fixture.rpc.messages.get(timeout=max(0.1, deadline - time.monotonic()))
            fixture.rpc.events.append(event)
            if event.get("method") == "_eden/context/state":
                updated = event["params"]
            assert time.monotonic() < deadline, "settled context update missing"
        assert updated["estimated"] and updated["used"] > initial["used"], (initial, updated)
        assert fixture.provider.requests
        numerator = (
            f"{updated['used'] // 1000}K"
            if updated["used"] >= 10000
            else f"{updated['used'] / 1000:.1f}K"
        )
        fixture.terminal.wait(f"~{numerator} / 65K")
        fixture.capture("after-request")
        assert f"~{numerator} / 65K" in (args.output / "after-request.txt").read_text()
        fixture.terminal.resize(38, 30)
        fixture.terminal.wait(f"~{numerator} / 65K")
        fixture.capture("narrow")
        fixture.checks["effective_context_count_reaches_footer_after_request"] = {
            "before": initial["used"],
            "after": updated["used"],
            "window": updated["window"],
        }
        fixture.checks["narrow_footer_keeps_counts"] = True
        fixture.terminal.resize(110, 40)
        fixture.terminal.command("/settings")
        fixture.terminal.wait("Context footer")
        fixture.capture("settings")
        fixture.checks["context_footer_in_display_settings"] = True
        fixture.click("Context footer")
        fixture.terminal.send(b"\x1b")
        fixture.capture("hidden")
        assert " / 65K" not in (args.output / "hidden.txt").read_text()
        fixture.terminal.command("/settings")
        fixture.terminal.wait("Context footer")
        fixture.click("Context footer")
        fixture.terminal.send(b"\x1b")
        fixture.terminal.wait(f"~{numerator} / 65K")
        fixture.checks["footer_toggle_hides_and_restores"] = True
        fixture.stop_terminal()
        fixture.terminal_start()
        fixture.terminal.wait(f"~{numerator} / 65K")
        fixture.capture("reopened")
        fixture.checks["reopen_restores_effective_context"] = True
        challenge = fixture.mutate(
            "/auth/start", {"request": {"action": "start", "provider": "anthropic"}}
        )
        reply = fixture.call(
            "/auth/input",
            {
                "operation_id": challenge["operation_id"],
                "api_key": True,
                "input": "fixture-footer-key",
            },
        )
        fixture.call("/terminal", reply)
        fixture.rpc.call(
            "session/set_model", {"sessionId": fixture.identity, "modelId": "anthropic/workflow"}
        )
        fixture.terminal.wait(" / ?")
        fixture.capture("unknown-window")
        fixture.rpc.call(
            "session/set_model", {"sessionId": fixture.identity, "modelId": "fixture/workflow"}
        )
        fixture.terminal.wait(f"~{numerator} / 65K")
        fixture.checks["model_switch_invalidates_unknown_capacity"] = True
        fixture.mutate("/context/compact", {"instructions": "Summarize the earlier turn."})
        compacted = next_context(fixture, lambda c: c["estimated"] and c["used"] < updated["used"])
        fixture.terminal.wait(f"~{fmt_count(compacted['used'])} / 65K")
        fixture.capture("compacted")
        assert any(r["kind"] == "compaction" for r in fixture.call("/tui/snapshot")["history"])
        context = fixture.call("/context/inspect", {})
        document = context["effective"]
        document["entries"] = [
            {
                "id": "inserted:footer",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "text", "text": "Edited effective request"}],
                },
                "references": [],
            }
        ]
        fixture.call(
            "/context/apply",
            {
                "request_id": "footer-edit",
                "edit": {
                    "revision": context["revision"],
                    "document": document,
                    "scope": "branch",
                    "source": "footer acceptance",
                },
            },
        )
        edited = next_context(fixture, lambda c: c["estimated"] and c["used"] < compacted["used"])
        fixture.terminal.wait(f"~{fmt_count(edited['used'])} / 65K")
        fixture.capture("context-edited")
        fixture.checks["compaction_and_context_edits_refresh_displayed_numerator"] = True
        fixture.terminal.command("/sessions")
        fixture.click(str(fixture.history))
        fixture.click("Read-only")
        fixture.terminal.wait("Read-only history.")
        fixture.capture("readonly")
        assert "? / " in (args.output / "readonly.txt").read_text()
        fixture.checks["readonly_context_is_unknown"] = True
        fixture.stop_terminal()


if __name__ == "__main__":
    main()
