# Eden Mocha foreground palette

Reference: [pi-catppuccin catppuccin-mocha](https://github.com/sherif-fanous/pi-catppuccin/blob/cd09277df06621155d9c4c20e45309bce5341779/themes/catppuccin-mocha.json), MIT © 2026 Sherif Fanous. The original JSON and license are kept beside this file.

The default uses Mocha text, mauve accents, green success, red errors, peach numbers, yellow types, blue functions and lavender links. Eden uses lavender headings rather than red to reserve red for failures; body hints use subtext_0 for readability, while overlay colours are reserved for borders and decoration. Every application background stays terminal-default, including the editor, panels and tool output. The light-foreground setting remains a separate contrast adaptation and does not change the terminal background.

Shared constants in `ui-sdk::mocha` keep the host and native editor aligned. `NO_COLOR` preserves emphasis and layout without forcing RGB colours.
