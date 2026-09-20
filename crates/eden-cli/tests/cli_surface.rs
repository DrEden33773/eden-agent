//! Process-level contracts of the command surface.
//!
//! These assert observable behaviour — exit codes, stream choice and the words
//! a reader needs — never the layout of a help page. clap documents help and
//! error rendering as unstable across releases, so a whole-page snapshot here
//! would fail on a dependency update without anything about eden changing. The
//! few substring assertions below pin wording this CLI promises (a usage error
//! names a suggestion, a conflict says so); if clap rewords one, the contract
//! moved and the assertion should be updated with it.
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

fn eden() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eden"))
}

fn run(args: &[&str]) -> Output {
    eden().args(args).output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// An empty directory a process may run in.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "eden-cli-surface-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn version_is_one_stdout_line_starting_with_the_program_name() {
    for flag in ["--version", "-V"] {
        let output = run(&[flag]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(stdout(&output).starts_with("eden "), "{}", stdout(&output));
        assert!(output.stderr.is_empty(), "{}", stderr(&output));
    }
}

#[test]
fn help_is_layered_from_families_down_to_single_actions() {
    let root = run(&["--help"]);
    assert_eq!(root.status.code(), Some(0), "{}", stderr(&root));
    let root = stdout(&root);
    for family in [
        "trust",
        "resources",
        "commands",
        "command",
        "package",
        "history",
        "session",
    ] {
        assert!(root.contains(family), "root help lost {family}: {root}");
    }
    let family = run(&["session", "--help"]);
    assert_eq!(family.status.code(), Some(0), "{}", stderr(&family));
    let family = stdout(&family);
    for action in ["info", "fork", "migrate", "queue-mode"] {
        assert!(
            family.contains(action),
            "family help lost {action}: {family}"
        );
    }
    // The root points at command help and the family carries what spans its
    // actions; neither is the other's copy.
    assert!(root.contains("for more information on a command"), "{root}");
    assert!(
        family.contains("Copy actions preview by default"),
        "{family}"
    );
    let action = run(&["session", "fork", "--help"]);
    assert_eq!(action.status.code(), Some(0), "{}", stderr(&action));
    let action = stdout(&action);
    for word in ["<SOURCE>", "<DESTINATION>", "--apply", "--at"] {
        assert!(action.contains(word), "action help lost {word}: {action}");
    }
}

#[test]
fn a_misspelled_action_fails_before_any_side_effect() {
    let scratch = Scratch::new();
    let output = eden()
        .current_dir(scratch.path())
        .args(["session", "frok", "a", "b"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("tip"), "{diagnostic}");
    assert!(diagnostic.contains("fork"), "{diagnostic}");
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn a_misspelled_family_suggests_the_family_it_meant() {
    let output = run(&["resorces", "list"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("tip"), "{diagnostic}");
    assert!(diagnostic.contains("resources"), "{diagnostic}");
}

#[test]
fn a_global_option_may_precede_the_family() {
    // cargo accepts `cargo --color always build`; the new global options invite
    // the same placement, so it must not be a conflict.
    let colored = run(&["--color", "always", "session", "info", "--help"]);
    assert_eq!(colored.status.code(), Some(0), "{}", stderr(&colored));
    assert!(stdout(&colored).contains('\u{1b}'), "{}", stdout(&colored));
    let quiet = run(&["-q", "session", "info", "--help"]);
    assert_eq!(quiet.status.code(), Some(0), "{}", stderr(&quiet));
    assert!(
        stdout(&quiet).contains("Session history file"),
        "{}",
        stdout(&quiet)
    );
}

#[test]
fn a_prompt_written_with_a_command_is_a_conflict() {
    // Without this, the stray word would be dropped and the command would run.
    let output = run(&["anything", "session", "info", "task.jsonl"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("cannot be used with"), "{diagnostic}");
    assert!(diagnostic.contains("[PROMPT]"), "{diagnostic}");
}

#[test]
fn a_misspelled_option_suggests_the_option_it_meant() {
    let output = run(&["--cwrd", "/tmp"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("a similar argument exists"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_ordinary_prompt_is_never_read_as_a_misspelled_family() {
    // A prompt that is near a family name is only rewritten when the word
    // after it is one of that family's own actions.
    for args in [
        vec!["resorce this file", "extra"],
        vec!["resource this file"],
        vec!["test session"],
    ] {
        let output = run(&args);
        assert_ne!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(
            !stderr(&output).contains("'resources'"),
            "{args:?} was rewritten: {}",
            stderr(&output)
        );
        assert!(
            !stderr(&output).contains("'trust'"),
            "{args:?} was rewritten: {}",
            stderr(&output)
        );
    }
}

#[test]
fn a_display_request_wins_over_a_misspelled_word() {
    for args in [vec!["--help", "resorce"], vec!["--version", "resorce"]] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty(), "{}", stderr(&output));
    }
}

#[test]
fn json_may_precede_the_family() {
    // The flag is global, so writing it first must reach the run rather than
    // parse into a root value the family never reads.
    let output = run(&["--json", "session", "info", "/nonexistent-history"]);
    assert_ne!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("PersistenceFailure"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn session_switches_that_contradict_each_other_are_a_usage_error() {
    let output = run(&["--no-session", "--session", "task.jsonl", "prompt"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("cannot be used with"),
        "{}",
        stderr(&output)
    );
    let output = run(&["--session", "task.jsonl", "--continue", "prompt"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("cannot be used with"),
        "{}",
        stderr(&output)
    );
    let output = run(&["--continue"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("--session"), "{}", stderr(&output));
}

#[test]
fn color_always_survives_a_pipe_and_color_never_removes_it() {
    let always = run(&["--color", "always", "--help"]);
    assert_eq!(always.status.code(), Some(0), "{}", stderr(&always));
    assert!(
        stdout(&always).contains('\u{1b}'),
        "--color always did not color"
    );
    let never = run(&["--color", "never", "--help"]);
    assert!(!stdout(&never).contains('\u{1b}'), "--color never colored");
    let rejected = run(&["--color", "sometimes", "--help"]);
    assert_eq!(rejected.status.code(), Some(2), "{}", stderr(&rejected));
    assert!(stderr(&rejected).contains("auto"), "{}", stderr(&rejected));
}

/// A composition that names no packages: opening it reaches the status line and
/// then fails in preflight, so one process prints both kinds of message without
/// needing a native plugin.
fn bare_run(scratch: &Scratch, extra: &[&str]) -> Output {
    let composition = scratch.path().join("composition.json");
    fs::write(&composition, br#"{"packages":[],"roles":{}}"#).unwrap();
    let session = scratch.path().join("task.jsonl");
    eden()
        .current_dir(scratch.path())
        .args([
            "--composition",
            composition.to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ])
        .args(extra)
        .arg("prompt")
        .output()
        .unwrap()
}

#[test]
fn quiet_drops_status_but_never_a_failure() {
    let scratch = Scratch::new();
    let plain = stderr(&bare_run(&scratch, &[]));
    assert!(plain.contains("Session:"), "{plain}");
    assert!(plain.contains("error:"), "{plain}");
    let quiet = stderr(&bare_run(&scratch, &["--quiet"]));
    assert!(!quiet.contains("Session:"), "{quiet}");
    assert!(quiet.contains("error:"), "{quiet}");
    // -q is the same request, and --verbose is accepted without inventing output.
    let short = stderr(&bare_run(&scratch, &["-q"]));
    assert!(!short.contains("Session:"), "{short}");
    let verbose = bare_run(&scratch, &["-vv"]);
    assert_eq!(verbose.status.code(), bare_run(&scratch, &[]).status.code());
    assert!(
        stderr(&verbose).contains("Session:"),
        "{}",
        stderr(&verbose)
    );
}
