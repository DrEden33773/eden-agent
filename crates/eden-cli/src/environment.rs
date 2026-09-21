//! Explicit dotenv parsing. No discovery, shell execution or environment mutation.
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;

/// Parse the entire selected file before any variable is installed.
/// Existing process environment values win, including explicitly empty values.
/// Keep file order when applying assignments: Windows compares keys without case.
pub fn read(
    path: &Path,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Result<Vec<(String, String)>, String> {
    if path.as_os_str().is_empty() {
        return Err("--env-file needs a nonempty path".into());
    }
    let file = File::open(path).map_err(|_| format!("cannot read env file {}", path.display()))?;
    let mut environment = Vec::new();
    for (index, item) in dotenvy::from_read_iter(file).enumerate() {
        // dotenvy parse errors can contain whole secret-bearing lines.
        let (key, value) = item
            .map_err(|_| format!("invalid env file {} at entry {}", path.display(), index + 1))?;
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
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_environment_wins_and_file_values_are_data() {
        let path = std::env::temp_dir().join(format!("eden-env-values-{}.env", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                "PRESERVED=file-value\n",
                "EMPTY=file-value\n",
                "LITERAL='$(not a shell command)'\n",
                "QUOTED=\"a value with spaces\"\n",
            ),
        )
        .unwrap();
        let environment = read(&path, |key| match key {
            "PRESERVED" => Some("process-value".into()),
            "EMPTY" => Some("".into()),
            _ => None,
        })
        .unwrap();
        std::fs::remove_file(path).unwrap();
        let environment: std::collections::BTreeMap<_, _> = environment.into_iter().collect();
        assert!(!environment.contains_key("PRESERVED"));
        assert!(!environment.contains_key("EMPTY"));
        assert_eq!(environment["LITERAL"], "$(not a shell command)");
        assert_eq!(environment["QUOTED"], "a value with spaces");
    }
}
