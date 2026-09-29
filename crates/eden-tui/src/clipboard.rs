//! System clipboard operations for blocking workers; terminal output stays with its owner.
use std::{
    io::Write,
    process::{Command, Stdio},
};

use base64::{Engine, engine::general_purpose::STANDARD};

/// Encoded images retain their bytes. Windows bitmap-only clipboard data is exported
/// losslessly as PNG because it has no original encoded file; no resizing occurs here.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClipboardContent {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
}

/// OSC52 means a request only: terminals may reject it or require user confirmation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyOutcome {
    System,
    Osc52(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Windows,
    Mac,
    Wayland,
    X11,
}

fn remote() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}
fn backends() -> Vec<Backend> {
    if cfg!(windows) {
        return vec![Backend::Windows];
    }
    if cfg!(target_os = "macos") {
        return vec![Backend::Mac];
    }
    let mut result = Vec::new();
    if std::env::var_os("WSL_INTEROP").is_some() || std::env::var_os("WSL_DISTRO_NAME").is_some() {
        result.push(Backend::Windows);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        result.push(Backend::Wayland);
    }
    if std::env::var_os("DISPLAY").is_some() {
        result.push(Backend::X11);
    }
    result
}

/// Read images before text when the clipboard advertises both representations.
/// SSH clipboard queries are unavailable: OSC52 reads require terminal input ownership.
pub(crate) fn read() -> Result<ClipboardContent, String> {
    if remote() {
        return Err(
            "Clipboard read is unavailable over SSH; paste text or attach an image file".into(),
        );
    }
    let mut errors = Vec::new();
    for backend in backends() {
        match read_backend(backend, &mut system_command) {
            Ok(content) => return Ok(content),
            Err(error) => errors.push(error),
        }
    }
    Err(if errors.is_empty() {
        "No system clipboard backend is available".into()
    } else {
        errors.join("; ")
    })
}

/// Call on a blocking worker. Send an OSC52 result only from the terminal owner.
pub(crate) fn write(text: &str) -> Result<CopyOutcome, String> {
    Ok(write_with(text, remote(), &backends(), &mut system_command))
}

fn write_with(text: &str, remote: bool, backends: &[Backend], run: &mut Runner<'_>) -> CopyOutcome {
    if !remote {
        for &backend in backends {
            if write_backend(backend, text, run).is_ok() {
                return CopyOutcome::System;
            }
        }
    }
    CopyOutcome::Osc52(format!("\x1b]52;c;{}\x07", STANDARD.encode(text)))
}

type Runner<'a> = dyn FnMut(&str, &[&str], Option<&[u8]>) -> Result<Vec<u8>, String> + 'a;

fn system_command(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    if matches!(program, "pbpaste" | "pbcopy") {
        command.env("LC_CTYPE", "en_US.UTF-8");
    }
    let mut child = command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // A clipboard owner may fork and keep descriptors alive. Writes need no output.
        .stdout(if input.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    // Clipboard writes do not produce output before reading stdin. Close stdin explicitly
    // so PowerShell ReadToEnd and utilities waiting for EOF can finish.
    let written = if let Some(input) = input {
        child
            .stdin
            .take()
            .ok_or_else(|| format!("{program}: missing input pipe"))
            .and_then(|mut pipe| {
                pipe.write_all(input)
                    .map_err(|error| format!("{program}: {error}"))
            })
    } else {
        Ok(())
    };
    let output = child
        .wait_with_output()
        .map_err(|error| format!("{program}: {error}"))?;
    written?;
    if !output.status.success() {
        return Err(format!(
            "{program}: clipboard command failed ({})",
            output.status
        ));
    }
    Ok(output.stdout)
}

fn image_type(types: &str) -> Option<&str> {
    // Preserve native encoded data, preferring formats accepted by most image consumers.
    [
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/tiff",
        "image/bmp",
        "image/gif",
    ]
    .into_iter()
    .find(|candidate| types.lines().any(|line| line.trim() == *candidate))
}
fn text(bytes: Vec<u8>) -> Result<ClipboardContent, String> {
    String::from_utf8(bytes)
        .map(ClipboardContent::Text)
        .map_err(|_| "Clipboard text is not valid UTF-8".into())
}
fn read_backend(backend: Backend, run: &mut Runner<'_>) -> Result<ClipboardContent, String> {
    match backend {
        Backend::Windows => decode_native(&run(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                WINDOWS_READ,
            ],
            None,
        )?),
        Backend::Mac => {
            let image = run("osascript", &["-l", "JavaScript", "-e", MAC_IMAGE], None)?;
            if image.starts_with(b"NONE") {
                text(run("pbpaste", &[], None)?)
            } else {
                decode_native(&image)
            }
        }
        Backend::Wayland | Backend::X11 => {
            let types = match backend {
                Backend::Wayland => run("wl-paste", &["--list-types"], None),
                _ => run(
                    "xclip",
                    &["-selection", "clipboard", "-out", "-target", "TARGETS"],
                    None,
                ),
            }?;
            let types = String::from_utf8(types)
                .map_err(|_| "Clipboard format list is not UTF-8".to_owned())?;
            if let Some(media_type) = image_type(&types) {
                let bytes = match backend {
                    Backend::Wayland => {
                        run("wl-paste", &["--no-newline", "--type", media_type], None)
                    }
                    _ => run(
                        "xclip",
                        &["-selection", "clipboard", "-out", "-target", media_type],
                        None,
                    ),
                }?;
                if bytes.is_empty() {
                    return Err("Clipboard image is empty".into());
                }
                return Ok(ClipboardContent::Image {
                    media_type: media_type.to_owned(),
                    bytes,
                });
            }
            match backend {
                Backend::Wayland => {
                    text(run("wl-paste", &["--no-newline", "--type", "text"], None)?)
                }
                _ => text(run(
                    "xclip",
                    &["-selection", "clipboard", "-out", "-target", "UTF8_STRING"],
                    None,
                )?),
            }
        }
    }
}
fn write_backend(backend: Backend, value: &str, run: &mut Runner<'_>) -> Result<(), String> {
    match backend {
        Backend::Windows => {
            if run(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-STA",
                    "-Command",
                    WINDOWS_WRITE,
                ],
                Some(value.as_bytes()),
            )
            .is_ok()
            {
                return Ok(());
            }
            // clip.exe expects Windows text; a UTF-16LE BOM avoids the current OEM codepage.
            let mut bytes = vec![0xff, 0xfe];
            bytes.extend(value.encode_utf16().flat_map(u16::to_le_bytes));
            run("clip.exe", &[], Some(&bytes))?;
        }
        Backend::Mac => {
            run("pbcopy", &[], Some(value.as_bytes()))?;
        }
        Backend::Wayland => {
            run(
                "wl-copy",
                &["--type", "text/plain;charset=utf-8"],
                Some(value.as_bytes()),
            )?;
        }
        Backend::X11 => {
            run(
                "xclip",
                &["-selection", "clipboard", "-in", "-target", "UTF8_STRING"],
                Some(value.as_bytes()),
            )?;
        }
    }
    Ok(())
}
fn decode_native(bytes: &[u8]) -> Result<ClipboardContent, String> {
    // PowerShell and JXA encode binary transfer as base64 JSON, never through a text codepage.
    let value: serde_json::Value =
        serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .map_err(|_| "Clipboard backend returned invalid data".to_owned())?;
    if let Some(text) = value["text"].as_str() {
        return Ok(ClipboardContent::Text(text.to_owned()));
    }
    let media_type = value["media_type"]
        .as_str()
        .ok_or("Clipboard image type is missing")?;
    if image_type(media_type) != Some(media_type) {
        return Err("Unsupported clipboard image type".into());
    }
    let bytes = STANDARD
        .decode(
            value["data"]
                .as_str()
                .ok_or("Clipboard image data is missing")?,
        )
        .map_err(|_| "Clipboard image encoding is invalid".to_owned())?;
    if bytes.is_empty() {
        return Err("Clipboard image is empty".into());
    }
    Ok(ClipboardContent::Image {
        media_type: media_type.to_owned(),
        bytes,
    })
}

const WINDOWS_READ: &str = r#"$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.Windows.Forms; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $d=[Windows.Forms.Clipboard]::GetDataObject(); if ($null -eq $d) { @{text=''} | ConvertTo-Json -Compress; exit }; $formats=@(@('PNG','image/png'),@('image/png','image/png'),@('JFIF','image/jpeg'),@('image/jpeg','image/jpeg'),@('image/webp','image/webp'),@('image/tiff','image/tiff')); foreach ($f in $formats) { if ($d.GetDataPresent($f[0],$false)) { $v=$d.GetData($f[0],$false); if ($v -is [IO.MemoryStream]) { $b=$v.ToArray() } elseif ($v -is [byte[]]) { $b=$v } else { continue }; @{media_type=$f[1];data=[Convert]::ToBase64String($b)} | ConvertTo-Json -Compress; exit } }; if ([Windows.Forms.Clipboard]::ContainsImage()) { $i=[Windows.Forms.Clipboard]::GetImage(); $s=[IO.MemoryStream]::new(); try { $i.Save($s,[Drawing.Imaging.ImageFormat]::Png); @{media_type='image/png';data=[Convert]::ToBase64String($s.ToArray())} | ConvertTo-Json -Compress } finally { $s.Dispose(); $i.Dispose() }; exit }; @{text=[Windows.Forms.Clipboard]::GetText()} | ConvertTo-Json -Compress"#;
const WINDOWS_WRITE: &str = r#"$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.Windows.Forms; [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); $v=[Console]::In.ReadToEnd(); if ($v.Length -eq 0) { [Windows.Forms.Clipboard]::Clear() } else { [Windows.Forms.Clipboard]::SetText($v) }"#;
const MAC_IMAGE: &str = r#"ObjC.import('AppKit'); var p=$.NSPasteboard.generalPasteboard; var formats=[['public.png','image/png'],['public.jpeg','image/jpeg'],['public.tiff','image/tiff']]; var answer='NONE'; for (var i=0;i<formats.length;i++) { var d=p.dataForType(formats[i][0]); if (d && !d.isNil()) { answer=JSON.stringify({media_type:formats[i][1],data:ObjC.unwrap(d.base64EncodedStringWithOptions(0))}); break; } } answer;"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_copy_returns_encoded_request_without_invoking_system_backend() {
        let outcome = write_with("text\x1b\n", true, &[Backend::X11], &mut |_, _, _| {
            panic!("must not access remote system clipboard")
        });
        assert_eq!(outcome, CopyOutcome::Osc52("\x1b]52;c;dGV4dBsK\x07".into()));
    }

    #[test]
    fn failed_system_write_is_not_reported_as_confirmed_copy() {
        let outcome = write_with("copy", false, &[Backend::Wayland], &mut |_, _, _| {
            Err("unavailable".into())
        });
        assert!(matches!(outcome, CopyOutcome::Osc52(_)));
        let outcome = write_with("copy", false, &[Backend::Wayland], &mut |_, _, _| {
            Ok(Vec::new())
        });
        assert_eq!(outcome, CopyOutcome::System);
    }

    #[test]
    fn wayland_prefers_encoded_image_and_preserves_every_byte() {
        let original = vec![0, 255, 13, 10, 128];
        let mut calls = Vec::new();
        let result = read_backend(Backend::Wayland, &mut |program, args, input| {
            assert_eq!(program, "wl-paste");
            assert!(input.is_none());
            calls.push(args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
            Ok(if args == ["--list-types"] {
                b"text/plain\nimage/png\n".to_vec()
            } else {
                original.clone()
            })
        })
        .unwrap();
        assert_eq!(
            result,
            ClipboardContent::Image {
                media_type: "image/png".into(),
                bytes: original
            }
        );
        assert_eq!(calls[1], ["--no-newline", "--type", "image/png"]);
    }
    #[test]
    fn x11_text_retains_unicode_and_exact_trailing_newlines() {
        let result = read_backend(Backend::X11, &mut |_, args, _| {
            Ok(if args.last() == Some(&"TARGETS") {
                b"UTF8_STRING\n".to_vec()
            } else {
                "中文\n\n".as_bytes().to_vec()
            })
        })
        .unwrap();
        assert_eq!(result, ClipboardContent::Text("中文\n\n".into()));
    }
    #[test]
    fn windows_clip_fallback_uses_utf16_instead_of_oem_encoding() {
        let mut captured = Vec::new();
        write_backend(Backend::Windows, "中👩", &mut |program, _, input| {
            if program == "powershell.exe" {
                return Err("unavailable".into());
            }
            captured = input.unwrap().to_vec();
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(captured, [0xff, 0xfe, 0x2d, 0x4e, 0x3d, 0xd8, 0x69, 0xdc]);
    }
    #[test]
    fn native_transfer_decodes_binary_and_rejects_corruption() {
        let content = decode_native(br#"{"media_type":"image/png","data":"AP+A"}"#).unwrap();
        assert_eq!(
            content,
            ClipboardContent::Image {
                media_type: "image/png".into(),
                bytes: vec![0, 255, 128]
            }
        );
        assert!(decode_native(br#"{"media_type":"image/png","data":"!"}"#).is_err());
    }
    #[test]
    fn mac_text_uses_pbpaste_without_trimming() {
        let content = read_backend(Backend::Mac, &mut |program, _, _| {
            Ok(if program == "osascript" {
                b"NONE\n".to_vec()
            } else {
                b"text\n".to_vec()
            })
        })
        .unwrap();
        assert_eq!(content, ClipboardContent::Text("text\n".into()));
    }
    #[cfg(unix)]
    #[test]
    fn controlled_child_receives_stdin_and_nonzero_exit_is_error() {
        assert!(system_command("sh", &["-c", r#"test "$(cat)" = input"#], Some(b"input")).is_ok());
        assert_eq!(
            system_command("sh", &["-c", "printf output"], None).unwrap(),
            b"output"
        );
        assert!(system_command("sh", &["-c", "exit 3"], None).is_err());
    }
}
