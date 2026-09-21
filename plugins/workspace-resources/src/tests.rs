use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
fn load(config: &SourceConfig, revision: u64) -> Result<Loaded, Fault> {
    super::load_with_home(
        config,
        revision,
        Some(Path::new(&config.global_dir).join("isolated-home")),
    )
}
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "eden-resources-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("project/.eden/skills/a")).unwrap();
        std::fs::create_dir_all(path.join("global/prompts")).unwrap();
        Self(path)
    }
    fn config(&self) -> SourceConfig {
        SourceConfig {
            cwd: self.0.join("project").to_string_lossy().into(),
            global_dir: self.0.join("global").to_string_lossy().into(),
            trusted: true,
            settings: json!({}),
            ..SourceConfig::default()
        }
    }
    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.0.join(path), text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn override_and_system_sources_have_distinct_precedence() {
    let f = Fixture::new();
    f.write("global/AGENTS.md", "global instruction");
    f.write("project/AGENTS.md", "shadowed instruction");
    f.write("project/AGENTS.override.md", "winning instruction");
    f.write("global/SYSTEM.md", "global system");
    f.write("project/.eden/SYSTEM.md", "project system");
    f.write("project/.eden/APPEND_SYSTEM.md", "append text");
    let loaded = load(&f.config(), 1).unwrap();
    assert!(loaded.snapshot.instructions.contains("global instruction"));
    assert!(loaded.snapshot.instructions.contains("winning instruction"));
    assert!(
        !loaded
            .snapshot
            .instructions
            .contains("shadowed instruction")
    );
    assert_eq!(loaded.snapshot.system.as_deref(), Some("project system"));
    assert_eq!(loaded.snapshot.append_system, "append text");
}
#[test]
fn no_context_disables_all_automatic_instruction_sources() {
    let f = Fixture::new();
    for path in [
        "project/AGENTS.md",
        "global/SYSTEM.md",
        "project/.eden/SYSTEM.md",
        "global/APPEND_SYSTEM.md",
    ] {
        f.write(path, "must not be loaded");
    }
    let mut config = f.config();
    config.settings = json!({ "discover_context": false });
    let loaded = load(&config, 1).unwrap();
    assert!(loaded.snapshot.instructions.is_empty());
    assert!(loaded.snapshot.system.is_none());
    assert!(loaded.snapshot.append_system.is_empty());
    assert!(loaded.snapshot.sources.is_empty());
}
#[test]
fn untrusted_skill_and_system_are_ignored_but_context_text_is_read() {
    let f = Fixture::new();
    f.write("project/AGENTS.md", "readable instructions");
    f.write("project/.eden/SYSTEM.md", "untrusted system");
    f.write(
        "project/.eden/skills/a/SKILL.md",
        "---\nname: a\ndescription: a skill\n---\nprivate body",
    );
    let mut config = f.config();
    config.trusted = false;
    let loaded = load(&config, 1).unwrap();
    assert!(
        loaded
            .snapshot
            .instructions
            .contains("readable instructions")
    );
    assert!(loaded.snapshot.skills.is_empty());
    assert!(loaded.snapshot.system.is_none());
}
#[test]
fn skill_body_is_only_returned_when_invoked_and_snapshot_stays_fixed() {
    let f = Fixture::new();
    f.write(
        "project/.eden/skills/a/SKILL.md",
        concat!(
            "---\n",
            "name: a\n",
            "description: do the task\n",
            "disable-model-invocation: true\n",
            "---\n",
            "ORIGINAL BODY",
        ),
    );
    let loaded = load(&f.config(), 1).unwrap();
    let serialized = serde_json::to_string(&loaded.snapshot).unwrap();
    assert!(!serialized.contains("ORIGINAL BODY"));
    assert_eq!(loaded.snapshot.skills.len(), 1);
    assert!(!loaded.snapshot.skills[0].model_invocable);
    f.write(
        "project/.eden/skills/a/SKILL.md",
        "---\nname: a\ndescription: do task\n---\nNEW BODY",
    );
    assert!(
        expand(&loaded, "/skill:a now")
            .unwrap()
            .contains("ORIGINAL BODY")
    );
    assert!(
        expand(&load(&f.config(), 2).unwrap(), "/skill:a now")
            .unwrap()
            .contains("NEW BODY")
    );
}
#[test]
fn template_arguments_defaults_and_slices_do_not_execute_shell() {
    let f = Fixture::new();
    f.write(
        "global/prompts/test.md",
        "---\ndescription: test\n---\n$1|$2|$@|${3:-fallback}|${@:2:1}|${ARGUMENTS:-empty}",
    );
    let loaded = load(&f.config(), 1).unwrap();
    assert_eq!(
        expand(&loaded, "/test first 'two words'").unwrap(),
        "first|two words|first two words|fallback|two words|first two words"
    );
    assert_eq!(expand(&loaded, "/test").unwrap(), "|||fallback||empty");
    assert!(expand(&loaded, "/test 'unterminated").is_err());
}

#[test]
fn bom_markdown_preserves_skill_metadata_and_template_body() {
    let f = Fixture::new();
    f.write(
        "project/.eden/skills/a/SKILL.md",
        "\u{feff}---\r\nname: renamed\r\ndescription: usable skill\r\n---\r\nFrozen body",
    );
    f.write(
        "global/prompts/test.md",
        "\u{feff}---\ndescription: test\n---\n$1",
    );
    let loaded = load(&f.config(), 1).unwrap();
    assert_eq!(loaded.snapshot.skills[0].name, "renamed");
    assert_eq!(loaded.snapshot.skills[0].description, "usable skill");
    assert!(
        expand(&loaded, "/skill:renamed")
            .unwrap()
            .ends_with("Frozen body\n\nUser: ")
    );
    assert_eq!(expand(&loaded, "/test content").unwrap(), "content");
}

#[test]
fn template_windows_paths_survive_argument_expansion() {
    let f = Fixture::new();
    f.write("global/prompts/test.md", "$1|$2|$3|$4|$5|$@|${@:2:1}");
    let loaded = load(&f.config(), 1).unwrap();
    assert_eq!(
        expand(
            &loaded,
            r#"/test C:\repo\src "C:\two words\src" '\\server\share' \\server\share C:\"#
        )
        .unwrap(),
        concat!(
            "C:\\repo\\src|",
            "C:\\two words\\src|",
            "\\\\server\\share|",
            "\\\\server\\share|",
            "C:\\|",
            "C:\\repo\\src C:\\two words\\src \\\\server\\share \\\\server\\share C:\\|",
            "C:\\two words\\src",
        )
    );
}

#[test]
fn template_explicit_escapes_empty_arguments_and_dollars_are_preserved() {
    let f = Fixture::new();
    f.write("global/prompts/test.md", "$1|$2|$3|$4|${5:-default}");
    let loaded = load(&f.config(), 1).unwrap();
    assert_eq!(
        expand(&loaded, r#"/test two\ words "say \"hi\"" '' '$1'"#).unwrap(),
        "two words|say \"hi\"||$1|default"
    );
}

#[test]
fn double_quoted_windows_directory_accepts_escaped_trailing_separator() {
    let f = Fixture::new();
    f.write("global/prompts/test.md", "$1|$2");
    let loaded = load(&f.config(), 1).unwrap();
    assert_eq!(
        expand(&loaded, r#"/test "C:\two words\\" "\\server\share\\""#).unwrap(),
        r"C:\two words\|\\server\share\"
    );
}

#[test]
fn startup_isolates_optional_bad_entries_but_reload_and_required_entries_fail() {
    let f = Fixture::new();
    f.write(
        "project/.eden/skills/a/SKILL.md",
        "---\nname: a\ndescription: good\n---\ngood body",
    );
    f.write("global/prompts/broken.md", "---\nname: [\n---\nbad");
    let loaded = load(&f.config(), 1).unwrap();
    assert_eq!(loaded.snapshot.skills.len(), 1);
    assert!(
        loaded
            .snapshot
            .diagnostics
            .iter()
            .any(|d| d.message.contains("broken.md"))
    );
    assert!(load(&f.config(), 2).is_err());
    let mut required = f.config();
    required.template_paths = vec![f.0.join("global/prompts/broken.md").display().to_string()];
    assert!(load(&required, 1).is_err());
    required.template_paths = vec![f.0.join("missing.md").display().to_string()];
    assert!(load(&required, 1).is_err());
}

#[test]
fn discovery_respects_ignore_hidden_dependencies_and_selective_exclusions() {
    let f = Fixture::new();
    for name in [
        "visible",
        "excluded",
        "ignored",
        ".hidden",
        "node_modules/dependency",
    ] {
        let directory = f.0.join("global/skills").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!(
                "---\nname: {}\ndescription: skill\n---\nbody",
                name.replace('/', "-")
            ),
        )
        .unwrap();
    }
    std::fs::create_dir_all(f.0.join("global/skills/nested")).unwrap();
    f.write(
        "global/skills/nested/notes.md",
        "---\nname: notes\ndescription: not a skill\n---\nbody",
    );
    f.write("global/skills/.ignore", "ignored/\n");
    f.write(
        "global/skills/excluded/SKILL.md",
        "---\nname: different-name\ndescription: excluded directory\n---\nbody",
    );
    let mut config = f.config();
    config.settings = json!({ "skill_excludes": ["excluded"] });
    let names: Vec<_> = load(&config, 1)
        .unwrap()
        .snapshot
        .skills
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["visible"]);
    let directory = f.0.join("global/skills/new");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        "---\nname: new\ndescription: future entry\n---\nbody",
    )
    .unwrap();
    assert_eq!(load(&config, 2).unwrap().snapshot.skills.len(), 2);
}
