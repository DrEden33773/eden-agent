//! Explicit dotenv parsing. No discovery, shell execution or environment mutation.
use std::{ffi::OsString, fs::File, path::PathBuf};

pub struct Startup {
    pub args: Vec<String>,
    pub environment: Vec<(String, String)>,
}
/// Parse the entire selected file before installing any variables.
/// Existing process environment values win, including explicitly empty values.
/// Keep file order when applying assignments: Windows compares keys without case.
pub fn prepare(
    args: Vec<String>,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Result<Startup, String> {
    let mut selected = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--env-file" {
            if selected.is_some() {
                return Err("--env-file may be specified only once".into());
            }
            let path = args.next().ok_or("--env-file needs a path")?;
            if path.is_empty() {
                return Err("--env-file needs a nonempty path".into());
            }
            selected = Some(PathBuf::from(path));
        } else {
            let takes_value = [
                "--composition",
                "--cwd",
                "--session",
                "--resume",
                "--history",
                "--attach",
                "--image",
                "--file",
            ]
            .contains(&arg.as_str());
            remaining.push(arg);
            if takes_value && let Some(value) = args.next() {
                remaining.push(value);
            }
        }
    }
    let mut environment = Vec::new();
    if let Some(path) = selected {
        let file =
            File::open(&path).map_err(|_| format!("cannot read env file {}", path.display()))?;
        for (index, item) in dotenvy::from_read_iter(file).enumerate() {
            // dotenvy parse errors can contain whole secret-bearing lines.
            let (key, value) = item.map_err(|_| {
                format!("invalid env file {} at entry {}", path.display(), index + 1)
            })?;
            if key.is_empty() || key.contains(['\0', '=']) || value.contains('\0') {
                return Err(format!(
                    "invalid env file {} at entry {}",
                    path.display(),
                    index + 1
                ));
            }
            if inherited(&key).is_none() {
                environment.push((key, value));
            }
        }
    }
    Ok(Startup {
        args: remaining,
        environment,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_environment_wins_and_file_values_are_data() {
        let path = std::env::temp_dir().join(format!("eden-env-values-{}.env", std::process::id()));
        std::fs::write(
            &path,
            "PRESERVED=file-value\nEMPTY=file-value\nLITERAL='$(not a shell \
             command)'\nQUOTED=\"a value with spaces\"\n",
        )
        .unwrap();
        let result = prepare(
            vec![
                "--env-file".into(),
                path.to_string_lossy().into(),
                "--version".into(),
            ],
            |key| match key {
                "PRESERVED" => Some("process-value".into()),
                "EMPTY" => Some("".into()),
                _ => None,
            },
        )
        .unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(result.args, vec!["--version"]);
        let environment: std::collections::BTreeMap<_, _> =
            result.environment.into_iter().collect();
        assert!(!environment.contains_key("PRESERVED"));
        assert!(!environment.contains_key("EMPTY"));
        assert_eq!(environment["LITERAL"], "$(not a shell command)");
        assert_eq!(environment["QUOTED"], "a value with spaces");
    }
}
