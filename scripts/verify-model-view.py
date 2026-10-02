#!/usr/bin/env python3
"""Compare negotiated model presentation with full reads through an installed isolated host."""

import argparse
import json
from pathlib import Path

from session_fixture import Fixture


def projected(entries):
    return [
        {
            "name": entry["name"],
            "status": entry["status"],
            "target": {
                "provider": entry["target"]["provider"],
                "model": entry["target"]["model"],
                "thinking": entry["target"]["thinking"],
                "limits": {"context_window": entry["target"]["limits"]["context_window"]},
                "capabilities": {"images": entry["target"]["capabilities"]["images"]},
            },
        }
        for entry in entries
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output) as fixture:
        full = fixture.call("/models/list", {})
        view = fixture.call("/tui/models", {})
        assert view["models"] == projected(full["models"]), "model presentation differs"
        assert all("base_url" in entry["target"] for entry in full["models"])
        assert all("base_url" not in entry["target"] for entry in view["models"])
        fixture.mutate(
            "/models/catalog",
            {
                "request": {
                    "action": "set_default",
                    "selection": {"provider": "fixture", "model": "workflow", "thinking": "off"},
                }
            },
        )
        current = fixture.call("/models/current", {})
        view = fixture.call("/tui/models", {})
        assert view["effective_target"] == current["effective_target"], "current target differs"
        assert view["diagnostic"] is None
        fixture.mutate(
            "/models/select",
            {"selection": {"provider": "fixture", "model": "workflow", "thinking": "high"}},
        )
        current = fixture.call("/models/current", {})
        view = fixture.call("/tui/models", {})
        assert view["effective_target"] == current["effective_target"], (
            "selection did not invalidate"
        )
        assert not fixture.provider.requests and not fixture.business.calls
        summary = {
            "models": len(full["models"]),
            "display_equal": True,
            "full_catalog_retained": True,
            "current_equal": True,
            "selection_changed": True,
            "full_catalog_bytes": len(json.dumps(full).encode()),
            "view_bytes": len(json.dumps(view).encode()),
            "provider_requests": 0,
            "business_calls": 0,
        }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
