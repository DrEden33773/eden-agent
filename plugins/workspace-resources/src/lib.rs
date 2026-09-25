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
fn create(mut config: Value) -> Result<Package, Fault> {
    if let Some(host) =
        eden_plugin_sdk::protocol::environment::HostEnvironment::from_config(&config)?
    {
        config["cwd"] = serde_json::json!(host.cwd);
        config["global_dir"] = serde_json::json!(host.global_dir);
        config["trusted"] = serde_json::json!(host.project_trusted);
        config["settings"] = host.settings;
        config["resource_packages"] = serde_json::json!(host.resource_packages);
    }
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
    load_with_home(config, revision, eden_workspace::paths::user_home())
}
fn load_with_home(
    config: &SourceConfig,
    revision: u64,
    home: Option<PathBuf>,
) -> Result<Loaded, Fault> {
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
    if enabled(&config.settings, "discover_context")? {
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
    }
    let mut skill_roots: Vec<_> = config
        .skill_paths
        .iter()
        .map(|path| eden_workspace::paths::resolve_path(cwd, Path::new(path)))
        .collect::<Result<_, _>>()?;
    let mut template_roots: Vec<_> = config
        .template_paths
        .iter()
        .map(|path| eden_workspace::paths::resolve_path(cwd, Path::new(path)))
        .collect::<Result<_, _>>()?;
    for (key, roots) in [
        ("skills", &mut skill_roots),
        ("templates", &mut template_roots),
    ] {
        if let Some(paths) = config.settings.get(key) {
            let paths = paths
                .as_array()
                .ok_or_else(|| invalid(format!("{key} must be a path array")))?;
            for path in paths {
                roots.push(eden_workspace::paths::resolve_path(
                    cwd,
                    Path::new(
                        path.as_str()
                            .ok_or_else(|| invalid(format!("{key} paths must be strings")))?,
                    ),
                )?);
            }
        }
    }
    eden_workspace::packages::validate_resource_packages(&config.resource_packages)?;
    for package in &config.resource_packages {
        skill_roots.extend(
            package
                .manifest
                .skills
                .iter()
                .map(|path| Path::new(&package.root).join(path)),
        );
        template_roots.extend(
            package
                .manifest
                .templates
                .iter()
                .map(|path| Path::new(&package.root).join(path)),
        );
    }
    let required_skills = skill_roots.len();
    let required_templates = template_roots.len();
    if enabled(&config.settings, "discover_skills")? {
        skill_roots.push(global.join("skills"));
        // A custom application state directory does not move the shared user home.
        if let Some(home) = home {
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
    for (is_skill, roots, required_count, key) in [
        (true, skill_roots, required_skills, "skill_excludes"),
        (
            false,
            template_roots,
            required_templates,
            "template_excludes",
        ),
    ] {
        let excludes = selectors(&config.settings, key)?;
        let mut visited = BTreeSet::new();
        for (index, root) in roots.iter().enumerate() {
            let policy = Discovery {
                is_skill,
                required: index < required_count,
                tolerate: revision == 1,
                excludes: &excludes,
            };
            discover(root, &policy, &mut visited, &mut loaded)?;
        }
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
struct Discovery<'a> {
    is_skill: bool,
    required: bool,
    tolerate: bool,
    excludes: &'a globset::GlobSet,
}
fn selectors(settings: &Value, key: &str) -> Result<globset::GlobSet, Fault> {
    let mut builder = globset::GlobSetBuilder::new();
    if let Some(value) = settings.get(key) {
        for pattern in value
            .as_array()
            .ok_or_else(|| invalid(format!("{key} must be an array")))?
        {
            builder.add(
                globset::Glob::new(
                    pattern
                        .as_str()
                        .ok_or_else(|| invalid(format!("{key} entries must be strings")))?,
                )
                .map_err(|e| invalid(format!("{key}: {e}")))?,
            );
        }
    }
    builder.build().map_err(|e| invalid(e.to_string()))
}
fn discovery_error(
    path: &Path,
    policy: &Discovery<'_>,
    loaded: &mut Loaded,
    error: Fault,
) -> Result<(), Fault> {
    let error = invalid(format!("{}: {}", path.display(), error.message));
    if policy.tolerate && !policy.required {
        loaded
            .snapshot
            .diagnostics
            .push(Diagnostic::warning(error.message));
        Ok(())
    } else {
        Err(error)
    }
}
fn discover(
    root: &Path,
    policy: &Discovery<'_>,
    visited: &mut BTreeSet<PathBuf>,
    loaded: &mut Loaded,
) -> Result<(), Fault> {
    if !root.exists() {
        if policy.required {
            return Err(invalid(format!(
                "required resource does not exist: {}",
                root.display()
            )));
        }
        return Ok(());
    }
    let mut walk = ignore::WalkBuilder::new(root);
    walk.parents(false)
        .hidden(!policy.required)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".fdignore")
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    if !policy.is_skill {
        walk.max_depth(Some(1));
    }
    let is_skill = policy.is_skill;
    let owned_root = root.to_owned();
    let excludes = policy.excludes.clone();
    walk.filter_entry(move |entry| {
        if excludes.is_match(entry.path())
            || excludes.is_match(
                entry
                    .path()
                    .strip_prefix(&owned_root)
                    .unwrap_or(entry.path()),
            )
            || (entry.file_type().is_some_and(|kind| kind.is_dir())
                && excludes.is_match(Path::new(entry.file_name())))
        {
            return false;
        }
        if entry.depth() == 0 {
            return true;
        }
        if entry.file_name() == "node_modules" {
            return false;
        }
        if is_skill {
            for parent in entry.path().ancestors().skip(1) {
                if parent.join("SKILL.md").is_file() && entry.path() != parent.join("SKILL.md") {
                    return false;
                }
                if parent == owned_root {
                    break;
                }
            }
        }
        true
    });
    for entry in walk.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery_error(root, policy, loaded, invalid(error.to_string()))?;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        if policy.is_skill
            && entry.depth() > 1
            && path.file_name().is_none_or(|name| name != "SKILL.md")
        {
            continue;
        }
        if policy.excludes.is_match(path)
            || policy
                .excludes
                .is_match(path.strip_prefix(root).unwrap_or(path))
        {
            continue;
        }
        let canonical = match std::fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) => {
                discovery_error(path, policy, loaded, file_error(error))?;
                continue;
            }
        };
        if !visited.insert(canonical) {
            continue;
        }
        if let Err(error) = load_resource(path, policy.is_skill, policy.excludes, loaded) {
            discovery_error(path, policy, loaded, error)?;
        }
    }
    Ok(())
}
fn load_resource(
    path: &Path,
    is_skill: bool,
    excludes: &globset::GlobSet,
    loaded: &mut Loaded,
) -> Result<(), Fault> {
    let raw = std::fs::read_to_string(path).map_err(file_error)?;
    let (metadata, body) = frontmatter(&raw)?;
    let description = metadata
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    if is_skill && description.trim().is_empty() {
        if path.file_name().is_some_and(|n| n == "SKILL.md") {
            return Err(invalid("Skill requires a nonempty description"));
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
    if excludes.is_match(&name) {
        return Ok(());
    }
    let bodies = if is_skill {
        &mut loaded.skills
    } else {
        &mut loaded.templates
    };
    if bodies.contains_key(&name) {
        loaded
            .snapshot
            .diagnostics
            .push(Diagnostic::warning(format!(
                "Duplicate resource {name}; first source wins, ignored {}",
                path.display()
            )));
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
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
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
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        // A doubled trailing separator allows a quoted Windows directory to end
        // before the closing quote, while leading UNC separators remain literal.
        if ch == '\\' && quote == Some('"') && chars.clone().take(2).eq(['\\', '"']) {
            chars.next();
            word.push('\\');
            started = true;
            continue;
        }
        // Only grouping syntax is escapable; path separators (including UNC prefixes)
        // remain literal instead of silently changing the user's file names.
        if ch == '\\'
            && quote != Some('\'')
            && chars.peek().is_some_and(|next| {
                if let Some(q) = quote {
                    *next == q
                } else {
                    next.is_whitespace() || matches!(next, '\'' | '"')
                }
            })
        {
            if let Some(next) = chars.next() {
                word.push(next);
            }
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
    if quote.is_some() {
        return Err(invalid("unterminated quoted argument"));
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
