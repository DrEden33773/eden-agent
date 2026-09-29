#!/usr/bin/env python3
"""Render decoded terminal cells with the reference harness's 8x18 cell geometry."""

import argparse
import html
import json
import unicodedata
from pathlib import Path


def cell_width(text):
    return sum(
        0 if unicodedata.combining(char) else 2 if unicodedata.east_asian_width(char) in "WF" else 1
        for char in text
    )


def render(source):
    value = json.loads(source.read_text())
    label = html.escape(value.get("label", "Eden host / Grok pager"))
    rows = value["screen"]["size"]["rows"]
    cols = value["screen"]["size"]["cols"]
    width, height = cols * 8 + 32, rows * 18 + 58
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#1e1e1e"/>',
        "<style>text { font-family: Menlo, Monaco, Consolas, monospace; font-size: 14px; white-space: pre; }</style>",
        f'<text font-family="DejaVu Sans Mono" font-size="14" font-style="normal" x="16" y="28" fill="#9cdcfe">{label} — {html.escape(source.stem)} — {cols}×{rows}</text>',
    ]
    for line in value["styled"]:
        column = 0
        y = 42 + (line["line"] - 1) * 18
        for run in line["runs"]:
            text = run["text"]
            cells = cell_width(text)
            x = 16 + column * 8
            fg, bg = run.get("fg", "#d4d4d4"), run.get("bg", "#1e1e1e")
            if run.get("inverse"):
                fg, bg = bg, fg
            parts.append(f'<rect x="{x}" y="{y}" width="{cells * 8}" height="18" fill="{bg}"/>')
            weight = ' font-weight="bold"' if run.get("bold") else ""
            parts.append(
                f'<text font-family="DejaVu Sans Mono" font-size="14" font-style="normal" x="{x}" y="{y + 14}" fill="{fg}"{weight}>{html.escape(text)}</text>'
            )
            column += cells
    parts.append("</svg>")
    source.with_suffix(".svg").write_text("\n".join(parts) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("screens", type=Path, nargs="+")
    args = parser.parse_args()
    for source in args.screens:
        render(source)


if __name__ == "__main__":
    main()
