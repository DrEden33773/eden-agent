#!/usr/bin/env python3
"""Exercise mixed saved histories, narrow CJK presentation and first shell admission."""

import argparse
import json
import re
import shlex
import subprocess
from pathlib import Path

from session_fixture import Fixture, committed, exited, owner, records, trace_rows, wait_until


def cli(fixture, *arguments):
    result = subprocess.run(
        [*fixture.arguments(), *map(str, arguments)],
        cwd=fixture.root,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    trace = args.output / "wire.jsonl"
    checks = {}
    with Fixture(args.installation, args.output) as fixture:
        directory = fixture.root / ".eden/sessions"
        directory.mkdir(parents=True, exist_ok=True)
        empty = directory / "legacy-empty.jsonl"
        empty.write_bytes(fixture.history.read_bytes())
        before_empty = empty.read_bytes()
        named = []
        for index in range(2):
            path = directory / f"named-empty-{index}.jsonl"
            path.write_bytes(before_empty)
            cli(fixture, "session", "metadata", path, "--name", "同名任务", "--tag", f"标签{index}")
            named.append(path)
        copied = directory / "explicit-copy.jsonl"
        cli(fixture, "session", "clone", empty, copied, "--apply")
        damaged = directory / "damaged.jsonl"
        damaged.write_bytes(b"{invalid\n")
        before = {path: path.read_bytes() for path in directory.glob("*.jsonl")}

        terminal = fixture.start(trace=trace, extra=["--color", "never"])
        terminal.command("/resume")
        terminal.wait("History needs attention")
        terminal.wait("Saved copy")
        terminal.wait("同名任务")
        assert terminal.display.count("同名任务") == 2
        assert "Empty session" not in terminal.display
        fixture.capture("mixed-recent")
        checks["named_empty_and_explicit_copy_and_corrupt_histories_remain_visible"] = True
        terminal.send(b"f")
        terminal.wait("All saved")
        terminal.wait("Empty session")
        checks["legacy_empty_history_is_available_in_all_saved_without_mutation"] = True
        terminal.send("/标签1".encode())
        terminal.wait("同名任务")
        wait_until(lambda: terminal.display.count("同名任务") == 1, terminal)
        terminal.send(b"\x1b[B")
        terminal.send(b"e")
        terminal.wait("Session details")
        terminal.wait(named[1].name)
        checks["tag_search_distinguishes_identical_titles_and_details_show_locator"] = True
        fixture.capture("selected-identical-title-details")
        terminal.send(b"\x1b")
        terminal.wait("Resume session")
        terminal.resize(58, 24)
        terminal.wait("同名任务")
        fixture.capture("narrow-cjk-picker")
        terminal.send(b"r")
        terminal.wait("Rename session")
        fixture.capture("narrow-cjk-action")
        terminal.send(b"\x1b")
        terminal.wait("Resume session")
        terminal.send(b"\x1b")
        terminal.wait("/ to search")
        terminal.send(b"\x1b")
        wait_until(lambda: "Resume session" not in terminal.display, terminal)
        color_codes = []
        for parameters in re.findall(rb"\x1b\[([0-9;]*)m", bytes(terminal.output)):
            codes = [int(value) for value in parameters.split(b";") if value]
            color_codes.extend(
                code for code in codes if 30 <= code <= 38 or 40 <= code <= 48 or 90 <= code <= 107
            )
        assert not color_codes, color_codes[:20]
        checks["narrow_cjk_actions_and_explicit_no_color"] = True
        assert {path: path.read_bytes() for path in directory.glob("*.jsonl")} == before
        exited(terminal)

        terminal = fixture.start(trace=trace)
        # The first operating-system side effect verifies its own durable admission.
        # It must not rely on the later user_shell result record written by the host.
        probe = (
            "import json,pathlib; root=pathlib.Path('.eden/sessions'); "
            "histories=[p for p in root.glob('*.jsonl') if p.name not in "
            f"{[path.name for path in before]!r}]; "
            "assert len(histories)==1; "
            "rows=[r for line in histories[0].read_text().splitlines() "
            "for r in json.loads(line).get('transaction',[json.loads(line)])]; "
            "assert rows[0]['kind']=='session'; "
            "assert any(r['kind']=='work_admitted' and "
            "r['payload']['intent']['kind']=='user_shell' for r in rows); "
            "pathlib.Path('shell-admission-proof').write_text('durable-before-execution'); "
            "print('SHELL_ADMISSION_PROVEN')"
        )
        terminal.command("!python3 -c " + shlex.quote(probe))
        wait_until((fixture.root / "shell-admission-proof").is_file, terminal)
        terminal.wait("SHELL_ADMISSION_PROVEN")
        saved = next(path for path in directory.glob("*.jsonl") if path not in before)
        _, endpoint = owner(fixture, saved)
        committed(endpoint, "SHELL_ADMISSION_PROVEN")
        assert any(record["kind"] == "user_shell" for record in records(saved))
        checks["first_shell_effect_observes_durable_identity_and_intent"] = True
        terminal.command("/resume")
        terminal.wait("current")
        terminal.wait("!python3")
        fixture.capture("shell-only-recognizable")
        terminal.send(b"\x1b")
        wait_until(lambda: "Resume session" not in terminal.display, terminal)
        exited(terminal)
        terminal = fixture.start(trace=trace)
        count = len(list(directory.glob("*.jsonl")))
        terminal.command("/rename 显式保存的草稿")
        wait_until(lambda: len(list(directory.glob("*.jsonl"))) == count + 1, terminal)
        explicit = next(
            path for path in directory.glob("*.jsonl") if path not in before and path != saved
        )
        wait_until(
            lambda: any(
                record["kind"] == "session_metadata"
                and record["payload"].get("name") == "显式保存的草稿"
                for record in records(explicit)
            ),
            terminal,
        )
        terminal.command("/resume")
        terminal.wait("显式保存的草稿")
        fixture.capture("explicitly-saved-draft")
        checks["explicit_draft_name_persists_and_remains_discoverable_without_messages"] = True
        terminal.send(b"\x1b")
        wait_until(lambda: "Resume session" not in terminal.display, terminal)
        exited(terminal)
        assert not fixture.provider.requests and not fixture.business.calls
        checks["provider_requests"] = 0
        checks["grok_business_http_calls"] = 0
        checks["typed_requests_observed"] = len(
            [row for row in trace_rows(trace) if row.get("direction") == "in"]
        )
    (args.output / "summary.json").write_text(json.dumps(checks, indent=2) + "\n")


if __name__ == "__main__":
    main()
