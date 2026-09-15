//! Default text resources. Discovery and expansion never execute resource code.
use eden_plugin_sdk::{
    Package,
    protocol::{Descriptor, Fault, resources::*},
    serde_json::{self, Value},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Loaded {
    snapshot: Snapshot,
    skills: BTreeMap<String, String>,
    templates: BTreeMap<String, String>,
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "workspace-resources".into(),
        version: "0.1.0".into(),
        provides: vec![SOURCE.into()],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let config: SourceConfig =
        serde_json::from_value(config).map_err(|e| invalid(e.to_string()))?;
    let initial = load(&config, 1)?;
    let state = Arc::new(Mutex::new(initial));
    Ok(
        Package::new("workspace-resources").service(
            SOURCE,
            move |request: ResourceRequest, _cx| {
                let state = state.clone();
                let config = config.clone();
                async move {
                    let mut current = state
                        .lock()
                        .map_err(|_| invalid("resource state poisoned"))?;
                    if matches!(request, ResourceRequest::Reload) {
                        let next = load(&config, current.snapshot.revision + 1)?;
                        *current = next;
                    }
                    let text = match request {
                        ResourceRequest::Expand { text } => Some(expand(&current, &text)?),
                        ResourceRequest::Skill { name, arguments } => {
                            Some(skill(&current, &name, &arguments)?)
                        }
                        _ => None,
                    };
                    Ok(ResourceReply {
                        snapshot: current.snapshot.clone(),
                        text,
                    })
                }
            },
        ),
    )
}
eden_plugin_sdk::export_plugin!(descriptor, create);

fn load(config: &SourceConfig, revision: u64) -> Result<Loaded, Fault> {
    let cwd = Path::new(&config.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(invalid("resource cwd must be an absolute directory"));
    }
    let global = Path::new(&config.global_dir);
    let mut loaded = Loaded {
        snapshot: Snapshot {
            revision,
            ..Snapshot::default()
        },
        ..Loaded::default()
    };
    let mut seen = BTreeSet::new();
    if enabled(&config.settings, "discover_context")? {
        let mut dirs = vec![global.to_owned()];
        let mut ancestors: Vec<_> = cwd.ancestors().map(Path::to_owned).collect();
        ancestors.reverse();
        dirs.extend(ancestors);
        for dir in dirs {
            for name in [
                "AGENTS.override.md",
                "AGENTS.md",
                "AGENTS.MD",
                "CLAUDE.md",
                "CLAUDE.MD",
            ] {
                let path = dir.join(name);
                if let Some(text) = read_optional(&path)? {
                    let canonical = std::fs::canonicalize(&path).map_err(file_error)?;
                    if seen.insert(canonical) {
                        loaded.snapshot.instructions.push_str(&format!(
                            "\n\nInstructions from {}:\n{text}",
                            path.display()
                        ));
                        loaded.snapshot.sources.push(path.display().to_string());
                    }
                    break;
                }
            }
        }
    }
    for (name, append) in [("SYSTEM.md", false), ("APPEND_SYSTEM.md", true)] {
        let mut paths = vec![];
        if config.trusted {
            paths.push(cwd.join(".eden").join(name));
        }
        paths.push(global.join(name));
        for path in paths {
            if let Some(text) = read_optional(&path)? {
                if append {
                    loaded.snapshot.append_system = text;
                } else {
                    loaded.snapshot.system = Some(text);
                }
                loaded.snapshot.sources.push(path.display().to_string());
                break;
            }
        }
    }
    let mut skill_roots: Vec<_> = config.skill_paths.iter().map(PathBuf::from).collect();
    let mut template_roots: Vec<_> = config.template_paths.iter().map(PathBuf::from).collect();
    for (key, roots) in [
        ("skills", &mut skill_roots),
        ("templates", &mut template_roots),
    ] {
        if let Some(paths) = config.settings.get(key) {
            let paths = paths
                .as_array()
                .ok_or_else(|| invalid(format!("{key} must be a path array")))?;
            for path in paths {
                roots.push(
                    cwd.join(
                        path.as_str()
                            .ok_or_else(|| invalid(format!("{key} paths must be strings")))?,
                    ),
                );
            }
        }
    }
    if enabled(&config.settings, "discover_skills")? {
        skill_roots.push(global.join("skills"));
        // The shared harness directory is a sibling of the application state directory.
        if let Some(home) = global.parent().and_then(Path::parent) {
            skill_roots.push(home.join(".agents/skills"));
        }
        if config.trusted {
            skill_roots.push(cwd.join(".eden/skills"));
            for dir in cwd.ancestors() {
                skill_roots.push(dir.join(".agents/skills"));
                if dir.join(".git").exists() {
                    break;
                }
            }
        }
    }
    if enabled(&config.settings, "discover_templates")? {
        template_roots.push(global.join("prompts"));
        if config.trusted {
            template_roots.push(cwd.join(".eden/prompts"));
        }
    }
    let mut visited = BTreeSet::new();
    for root in skill_roots {
        discover(&root, true, true, &mut visited, &mut loaded)?;
    }
    visited.clear();
    for root in template_roots {
        discover(&root, false, false, &mut visited, &mut loaded)?;
    }
    Ok(loaded)
}
fn enabled(settings: &Value, key: &str) -> Result<bool, Fault> {
    match settings.get(key) {
        None => Ok(true),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid(format!("{key} must be boolean"))),
    }
}
fn read_optional(path: &Path) -> Result<Option<String>, Fault> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(invalid(format!("{}: {error}", path.display()))),
    }
}
fn file_error(error: std::io::Error) -> Fault {
    Fault::new("FileFailure", "workspace-resources", error.to_string())
}
fn discover(
    path: &Path,
    is_skill: bool,
    recursive: bool,
    visited: &mut BTreeSet<PathBuf>,
    loaded: &mut Loaded,
) -> Result<(), Fault> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(file_error(error)),
    };
    if !visited.insert(canonical) {
        return Ok(());
    }
    if path.is_dir() {
        if is_skill && path.join("SKILL.md").is_file() {
            return discover(&path.join("SKILL.md"), true, false, visited, loaded);
        }
        let mut entries = std::fs::read_dir(path)
            .map_err(file_error)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(file_error)?;
        entries.sort();
        for entry in entries {
            if recursive || entry.is_file() {
                discover(&entry, is_skill, recursive, visited, loaded)?;
            }
        }
        return Ok(());
    }
    if path
        .extension()
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("md"))
    {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).map_err(file_error)?;
    let (metadata, body) = frontmatter(&raw)?;
    let description = metadata
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    if is_skill && description.trim().is_empty() {
        if path.file_name().is_some_and(|n| n == "SKILL.md") {
            loaded.snapshot.diagnostics.push(format!(
                "Skill without description ignored: {}",
                path.display()
            ));
        }
        return Ok(());
    }
    // Root Markdown in the shared .agents/skills directory is not a skill entry.
    if is_skill
        && path.parent().is_some_and(|p| p.ends_with(".agents/skills"))
        && path.file_name().is_none_or(|n| n != "SKILL.md")
    {
        return Ok(());
    }
    let inferred = if is_skill && path.file_name().is_some_and(|n| n == "SKILL.md") {
        path.parent().and_then(Path::file_name)
    } else {
        path.file_stem()
    };
    let name = if is_skill {
        metadata.get("name").and_then(Value::as_str)
    } else {
        None
    }
    .or_else(|| inferred.and_then(|s| s.to_str()))
    .ok_or_else(|| invalid("resource name is not UTF-8"))?
    .to_owned();
    if name.is_empty() || name.chars().any(char::is_whitespace) || name.contains(['/', '\\']) {
        return Err(invalid(format!(
            "invalid resource name at {}",
            path.display()
        )));
    }
    let bodies = if is_skill {
        &mut loaded.skills
    } else {
        &mut loaded.templates
    };
    if bodies.contains_key(&name) {
        loaded.snapshot.diagnostics.push(format!(
            "Duplicate resource {name}; first source wins, ignored {}",
            path.display()
        ));
        return Ok(());
    }
    let description = if description.is_empty() {
        body.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
    } else {
        description
    };
    let resource = Resource {
        name: name.clone(),
        description: description.into(),
        path: path.display().to_string(),
        model_invocable: metadata
            .get("disable-model-invocation")
            .and_then(Value::as_bool)
            != Some(true),
    };
    let body = if is_skill {
        format!(
            "Skill {name}; resolve relative resources from {}.\n\n{body}",
            path.parent().unwrap_or(Path::new(".")).display()
        )
    } else {
        body.into()
    };
    bodies.insert(name, body);
    if is_skill {
        loaded.snapshot.skills.push(resource);
    } else {
        loaded.snapshot.templates.push(resource);
    }
    Ok(())
}
fn frontmatter(raw: &str) -> Result<(Value, &str), Fault> {
    let Some((first, rest)) = raw.split_once('\n') else {
        return Ok((Value::Null, raw));
    };
    if first.trim_end() != "---" {
        return Ok((Value::Null, raw));
    }
    let mut end = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim() == "---" {
            let metadata: Value = serde_yaml_ng::from_str(&rest[..end])
                .map_err(|error| invalid(format!("frontmatter: {error}")))?;
            if !metadata.is_object() && !metadata.is_null() {
                return Err(invalid("frontmatter must be a mapping"));
            }
            return Ok((metadata, &rest[end + line.len()..]));
        }
        end += line.len();
    }
    Err(invalid("unterminated frontmatter"))
}
fn expand(loaded: &Loaded, text: &str) -> Result<String, Fault> {
    let Some(command) = text.strip_prefix('/') else {
        return Ok(text.into());
    };
    let (name, arguments) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    if let Some(name) = name.strip_prefix("skill:") {
        return skill(loaded, name, arguments);
    }
    let Some(template) = loaded.templates.get(name) else {
        return Ok(text.into());
    };
    template_expand(template, &split_arguments(arguments)?)
}
fn split_arguments(text: &str) -> Result<Vec<String>, Fault> {
    let mut args = vec![];
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in text.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                word.push(ch);
            }
            started = true;
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                args.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    if quote.is_some() || escaped {
        return Err(invalid("unterminated quoted argument or escape"));
    }
    if started {
        args.push(word);
    }
    Ok(args)
}
fn template_expand(template: &str, args: &[String]) -> Result<String, Fault> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        result.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        let (expression, consumed, braced) = if let Some(after) = rest.strip_prefix('{') {
            let Some(end) = after.find('}') else {
                result.push('$');
                continue;
            };
            (&after[..end], end + 2, true)
        } else if rest.starts_with("ARGUMENTS") {
            ("ARGUMENTS", 9, false)
        } else if rest.starts_with('@') {
            ("@", 1, false)
        } else {
            let length = rest.bytes().take_while(u8::is_ascii_digit).count();
            (&rest[..length], length, false)
        };
        let (key, default) = expression
            .split_once(":-")
            .map_or((expression, None), |(k, v)| (k, Some(v)));
        let value = if key == "@" || key == "ARGUMENTS" {
            Some(args.join(" "))
        } else if let Some(slice) = key.strip_prefix("@:") {
            let (start, length) = slice
                .split_once(':')
                .map_or((slice, None), |(a, b)| (a, Some(b)));
            let start = start
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid("template slice must start at a positive argument"))?;
            let length = length
                .map(|s| {
                    s.parse::<usize>()
                        .map_err(|_| invalid("invalid template slice length"))
                })
                .transpose()?
                .unwrap_or(usize::MAX);
            Some(
                args.iter()
                    .skip(start - 1)
                    .take(length)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        } else {
            key.parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| args.get(n - 1).cloned().unwrap_or_default())
        };
        if let Some(value) = value {
            result.push_str(if value.is_empty() {
                default.unwrap_or(&value)
            } else {
                &value
            });
        } else {
            result.push('$');
            if braced {
                result.push('{');
            }
            result.push_str(expression);
            if braced {
                result.push('}');
            }
        }
        rest = &rest[consumed..];
    }
    result.push_str(rest);
    Ok(result)
}
fn skill(loaded: &Loaded, name: &str, arguments: &str) -> Result<String, Fault> {
    let body = loaded
        .skills
        .get(name)
        .ok_or_else(|| invalid(format!("unknown skill: {name}")))?;
    Ok(format!("{body}\n\nUser: {arguments}"))
}
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "workspace-resources", message)
}

#[cfg(test)]
mod tests;
