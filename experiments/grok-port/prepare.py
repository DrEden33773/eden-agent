#!/usr/bin/env python3
"""Apply the Eden transport seam to an extracted, fixed Grok source archive."""

import argparse
import shutil
from pathlib import Path

HERE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    args = parser.parse_args()
    root = args.source
    acp = root / "crates/codegen/xai-grok-pager/src/acp"
    path = acp / "mod.rs"
    source = path.read_text()
    marker = "pub async fn connect(cancel: &CancellationToken, flags: ConnectFlags) -> Result<AcpConnection> {"
    replacement = (
        marker
        + '\n    if let Some(adapter) = std::env::var_os("EDEN_GROK_BRIDGE") {\n        return eden_transport::connect(cancel, flags, adapter.into()).await;\n    }'
    )
    if "mod eden_transport;" not in source:
        assert source.count(marker) == 1
        source = source.replace(marker, replacement)
        source += "\nmod eden_transport;\n"
        path.write_text(source)
    shutil.copyfile(HERE / "eden_transport.rs", acp / "eden_transport.rs")

    startup = root / "crates/codegen/xai-grok-pager/src/app/session_startup.rs"
    source = startup.read_text()
    marker = ") -> anyhow::Result<ResolvedExisting> {"
    if "// Eden owns session persistence." not in source:
        assert source.count(marker) >= 1
        source = source.replace(
            marker,
            marker
            + '\n    // Eden owns session persistence.\n    if std::env::var_os("EDEN_GROK_BRIDGE").is_some() {\n        return Ok(ResolvedExisting { id: session_id.into(), original_cwd: None, title: None, deferred_local_miss: false, suppress_code_restore: true });\n    }',
            1,
        )
        startup.write_text(source)
    cli = root / "crates/codegen/xai-grok-pager/src/app/cli.rs"
    source = cli.read_text()
    marker = "pub fn pin_local_resume_target(&mut self) -> anyhow::Result<()> {"
    if "// Eden endpoint identity is checked by the adapter." not in source:
        source = source.replace(
            marker,
            marker
            + '\n        // Eden endpoint identity is checked by the adapter.\n        if std::env::var_os("EDEN_GROK_BRIDGE").is_some() { return Ok(()); }',
            1,
        )
        cli.write_text(source)


if __name__ == "__main__":
    main()
