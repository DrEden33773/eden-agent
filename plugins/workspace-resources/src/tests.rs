use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
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
        "---\nname: a\ndescription: do the task\ndisable-model-invocation: \
                true\n---\nORIGINAL BODY",
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
