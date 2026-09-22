//! JSON presentation for people, with an explicit uncolored record path for machines.
use crate::{cli::Cli, shell::colored};
use clap::{ColorChoice, builder::styling::AnsiColor};
use serde::Serialize;
use std::io::{IsTerminal, Write};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Print an ordinary result; machine mode takes precedence over forced color.
pub fn result(cli: &Cli, value: &impl Serialize) -> Result<()> {
    let choice = match cli.color.as_str() {
        "always" => ColorChoice::Always,
        "never" => ColorChoice::Never,
        _ => ColorChoice::Auto,
    };
    let color = !cli.json && colored(choice, std::io::stdout().is_terminal());
    write_result(&mut std::io::stdout().lock(), value, cli.json, color)
}

/// Keep record boundaries parseable even when the caller requested terminal colors.
pub fn record(value: &impl Serialize) -> Result<()> {
    write_result(&mut std::io::stdout().lock(), value, true, false)
}

/// Serialize before writing so serialization failures cannot leave partial records.
fn write_result(
    writer: &mut impl Write,
    value: &impl Serialize,
    compact: bool,
    color: bool,
) -> Result<()> {
    let text = if compact {
        serde_json::to_string(value)?
    } else {
        serde_json::to_string_pretty(value)?
    };
    if color && !compact {
        highlight(writer, &text)?;
    } else {
        writer.write_all(text.as_bytes())?;
    }
    writeln!(writer)?;
    writer.flush()?;
    Ok(())
}

/// Only serialized JSON enters here: ASCII delimiters bound whole UTF-8 tokens.
fn highlight(writer: &mut impl Write, text: &str) -> std::io::Result<()> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let color = match bytes[at] {
            b'"' => {
                at += 1;
                while at < bytes.len() {
                    match bytes[at] {
                        b'\\' => at += 2,
                        b'"' => {
                            at += 1;
                            break;
                        }
                        _ => at += 1,
                    }
                }
                if text[at..].trim_start().starts_with(':') {
                    Some(AnsiColor::Cyan)
                } else {
                    Some(AnsiColor::Green)
                }
            }
            b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => {
                at += 1;
                while at < bytes.len()
                    && !matches!(bytes[at], b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t')
                {
                    at += 1;
                }
                Some(if matches!(bytes[start], b't' | b'f' | b'n') {
                    AnsiColor::Magenta
                } else {
                    AnsiColor::Yellow
                })
            }
            _ => {
                at += 1;
                None
            }
        };
        if let Some(color) = color {
            let style = color.on_default();
            write!(
                writer,
                "{}{}{}",
                style.render(),
                &text[start..at],
                style.render_reset()
            )?;
        } else {
            writer.write_all(&bytes[start..at])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formatting_and_coloring_preserve_json_tokens() {
        let value =
            json!({ "键\\\"": ["中文🙂\n\"\\\u{1b}", -1.25e30, true, false, null, {}, []] });
        let mut plain = Vec::new();
        write_result(&mut plain, &value, false, false).unwrap();
        assert_eq!(
            String::from_utf8(plain.clone()).unwrap(),
            format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
        );
        let mut colored = Vec::new();
        write_result(&mut colored, &value, false, true).unwrap();
        let mut text = String::from_utf8(colored).unwrap();
        for code in ["\x1b[36m", "\x1b[32m", "\x1b[33m", "\x1b[35m"] {
            assert!(text.contains(code));
            text = text.replace(code, "");
        }
        text = text.replace("\x1b[0m", "");
        assert_eq!(text.as_bytes(), plain);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            value
        );
    }

    #[test]
    fn compact_records_override_color_and_keep_order() {
        let mut bytes = Vec::new();
        for value in [
            json!({ "plan": { "ok": true } }),
            json!({ "created": "path" }),
        ] {
            write_result(&mut bytes, &value, true, true).unwrap();
        }
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"plan\":{\"ok\":true}}\n{\"created\":\"path\"}\n"
        );
    }
}
