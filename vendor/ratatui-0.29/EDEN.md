# Ratatui 0.29 compatibility patch

This is the crates.io Ratatui 0.29.0 source, retained under its original MIT license. Eden changes only the unicode-width dependency requirement from =0.2.0 to 0.2.2 so the Grok-derived rendering stack and the existing 0.30 terminal plugins can share one Cargo resolution. Rendering APIs remain 0.29; no upstream Rust source is changed. Wide text and layout are covered by the terminal acceptance checks.
