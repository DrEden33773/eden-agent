#!/usr/bin/env python3
"""Launch the isolated Grok pager experiment against an explicit Eden endpoint."""

import argparse
import json
import os
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SUFFIX = ".exe" if os.name == "nt" else ""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="?", choices=["live-tui"])
    parser.add_argument("--endpoint", type=Path, required=True)
    parser.add_argument(
        "--history", type=Path, help="Open this saved history through the attached management host"
    )
    parser.add_argument(
        "--pager", type=Path, default=ROOT / f"artifacts/g1-native-port/eden-grok{SUFFIX}"
    )
    args = parser.parse_args()
    endpoint = args.endpoint.resolve()
    home = endpoint.parent / ".grok-port"
    home.mkdir(mode=0o700, exist_ok=True)
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("GROK_", "XAI_", "LC_GROK_"))
        or key == "GROK_PROMPT_ACK_TIMEOUT_SECS"
    }
    environment.update(
        {
            "GROK_HOME": str(home),
            "GROK_TELEMETRY_ENABLED": "false",
            "GROK_TELEMETRY_MIXPANEL_ENABLED": "false",
            "GROK_TELEMETRY_TRACE_UPLOAD": "false",
            "GROK_FEEDBACK_ENABLED": "false",
            "GROK_TRACE_UPLOAD": "false",
            "GROK_INSTRUMENTATION": "disabled",
            "OTEL_SDK_DISABLED": "true",
            "DISABLE_TELEMETRY": "1",
            "DISABLE_FEEDBACK_COMMAND": "1",
            "GROK_DISABLE_AUTOUPDATER": "1",
            "GROK_PROMPT_SUGGESTIONS": "false",
            "GROK_AGENT_ID": "eden-grok-port",
            "GROK_TURN_SUMMARY": "0",
            "GROK_XAI_API_BASE_URL": "http://127.0.0.1:9",
            "EDEN_GROK_BRIDGE": str(args.pager.resolve().parent / f"eden-grok-adapter{SUFFIX}"),
            "GROK_AGENT_DASHBOARD": "false",
            "EDEN_GROK_ENDPOINT": str(endpoint),
        }
    )
    identity = f"eden-{json.loads(endpoint.read_text())['session_id']}"
    if args.history:
        history = args.history.resolve()
        environment["EDEN_GROK_INITIAL_HISTORY"] = str(history)
        environment["EDEN_GROK_SESSION_DIR"] = str(history.parent)
        identity = f"history:{history}"
    raise SystemExit(
        subprocess.call(
            [str(args.pager.resolve()), "--no-leader", "--resume", identity],
            env=environment,
        )
    )


if __name__ == "__main__":
    main()
