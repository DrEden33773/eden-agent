#!/usr/bin/env python3
"""Skills/templates: actual discovery, completion, expansion, reload and trust boundaries."""

import argparse
import json
from pathlib import Path

from verify_workflows import ROOT, Fixture


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def names(reply):
    return {command["name"] for command in reply["_meta"]["availableCommands"]}


def prompt(fixture, text):
    assert fixture.rpc is not None
    return fixture.rpc.call(
        "session/prompt",
        {
            "sessionId": fixture.identity,
            "prompt": [{"type": "text", "text": text}],
        },
    )


def wire(fixture):
    return json.dumps(fixture.provider.requests[-1]["messages"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/s4-g2-host")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks = {}
    for trusted in (False, True):
        with Fixture(
            args.installation, args.output, trusted=trusted, configuration_author=False
        ) as fixture:
            assert fixture.rpc is not None
            fixture.provider.mode = "short"
            root = fixture.root
            write(
                root / "global/skills/s4global/SKILL.md",
                "---\nname: s4global\ndescription: S4 global review skill\n---\nGLOBAL_SKILL_BODY\n",
            )
            write(
                root / ".eden/skills/s4local/SKILL.md",
                "---\nname: s4local\ndescription: S4 cwd review skill\n---\nLOCAL_SKILL_BODY\n",
            )
            write(
                root / ".eden/skills/s4global/SKILL.md",
                "---\nname: s4global\ndescription: conflicting skill\n---\nWRONG_CONFLICT_BODY\n",
            )
            write(
                root / "global/prompts/s4template.md",
                "---\ndescription: S4 template\n---\nTEMPLATE_BODY $1 :: $ARGUMENTS\n",
            )
            write(
                root / "global/prompts/model.md",
                "---\ndescription: collides with model picker\n---\nCOLLISION_TEMPLATE $1\n",
            )
            write(root / "AGENTS.md", "CWD_INSTRUCTION_CANARY\n")
            write(root / ".eden/SYSTEM.md", "PROJECT_SYSTEM_CANARY\n")
            fixture.mutate("/resources/reload", {})
            initial = fixture.rpc.call("initialize", {})
            available = names(initial)
            assert "skill:s4global" in available, (
                "native adapter did not advertise installed skills"
            )
            assert "s4template" in available and "template:model" in available
            assert ("skill:s4local" in available) == trusted
            # Frontend does not expand text itself: assert the actual provider's final request.
            prompt(fixture, "/skill:s4global explicit-argument")
            request = wire(fixture)
            assert "GLOBAL_SKILL_BODY" in request and "explicit-argument" in request
            assert "WRONG_CONFLICT_BODY" not in request
            assert "CWD_INSTRUCTION_CANARY" in request
            assert ("PROJECT_SYSTEM_CANARY" in request) == trusted
            prompt(fixture, '/s4template "quoted value" tail')
            assert "TEMPLATE_BODY quoted value :: quoted value tail" in wire(fixture)
            prompt(fixture, "/template:model explicit-collision")
            assert "COLLISION_TEMPLATE explicit-collision" in wire(fixture)
            if trusted:
                prompt(fixture, "/skill:s4local cwd-argument")
                assert "LOCAL_SKILL_BODY" in wire(fixture)
            page = fixture.form("resources")
            assert str(root) in page["description"], "resource inspection must show owning cwd"
            if trusted:
                assert "Duplicate resource s4global" in page["description"]
            else:
                assert "trust" in page["description"].lower()
            before = len(fixture.provider.requests)
            for text, expected in [
                ("/skill:s4missing", "unknown skill"),
                ('/s4template "unterminated', "quote"),
                ("/compactness", "not connected"),
            ]:
                try:
                    prompt(fixture, text)
                except RuntimeError as error:
                    assert expected.lower() in str(error).lower(), str(error)
                else:
                    raise AssertionError(f"invalid input did not fail: {text}")
            assert len(fixture.provider.requests) == before, (
                "invalid resources reached the provider"
            )
            fixture.terminal_start()
            assert fixture.terminal is not None
            fixture.terminal.send(b"/skill:s4g")
            fixture.terminal.wait("S4 global review skill")
            fixture.capture(f"completion-{trusted}")
            fixture.terminal.send(b"\x1b")
            fixture.terminal.send(b"\x15")
            fixture.terminal.command("/resources")
            fixture.terminal.wait("Skills and templates")
            fixture.capture(f"resources-{trusted}")
            if trusted:
                broken = root / ".eden/skills/s4broken/SKILL.md"
                write(broken, "---\nname: s4broken\n---\ninvalid missing description\n")
                fixture.click("Reload from disk")
                fixture.terminal.wait("nonempty description")
                fixture.capture("reload-error")
                assert fixture.call("/resources", {})["revision"] == 2, (
                    "failed reload replaced the old inventory"
                )
                assert "skill:s4global" in names(fixture.rpc.call("initialize", {}))
                broken.unlink()
            fixture.terminal.send(b"\x1b")
            # Reload updates both the backend snapshot and the live original completion widget.
            write(
                root / "global/skills/s4added/SKILL.md",
                "---\nname: s4added\ndescription: S4 reloaded skill\n---\nRELOADED_BODY\n",
            )
            (root / "global/skills/s4global/SKILL.md").unlink()
            fixture.terminal.command("/resources")
            fixture.click("Reload from disk")
            fixture.terminal.wait("Loaded revision: 3")
            fixture.terminal.send(b"\x1b")
            fixture.terminal.send(b"/skill:s4add")
            fixture.terminal.wait("S4 reloaded skill")
            fixture.terminal.send(b"\t")
            fixture.terminal.send(b" final-argument\r")
            fixture.terminal.wait("STREAM_FINISHED", seconds=20)
            assert "RELOADED_BODY" in wire(fixture) and "final-argument" in wire(fixture)
            after = names(fixture.rpc.call("initialize", {}))
            assert "skill:s4added" in after
            assert ("skill:s4global" in after) == trusted, (
                "deleted winner should disappear or reveal the next source"
            )
            checks[f"trusted-{trusted}"] = {
                "discovery_cwd": True,
                "completion": True,
                "final_provider_expansion": True,
                "reload": True,
                "conflicts": True,
                "errors_no_provider": True,
                "failed_reload_keeps_inventory": trusted,
            }
    (args.output / "summary.json").write_text(json.dumps(checks, indent=2) + "\n")
    print(json.dumps(checks))


if __name__ == "__main__":
    main()
