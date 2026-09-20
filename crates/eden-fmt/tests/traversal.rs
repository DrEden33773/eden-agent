//! What `eden-fmt check <dir>` walks.
//!
//! Cargo compiles a file under a dot directory when the manifest names it, so
//! "starts with a dot" cannot mean "not source". A build cache must stay out of
//! the walk no matter what.

use eden_fmt::engine::collect;
use std::path::{Path, PathBuf};

/// One throwaway tree, removed when the test ends.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("eden-fmt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    /// Collected files as `/`-separated paths relative to the fixture root.
    fn collected(&self) -> Vec<String> {
        let mut found: Vec<String> = collect(std::slice::from_ref(&self.0), &[])
            .unwrap()
            .iter()
            .map(|path| {
                path.strip_prefix(&self.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        found.sort();
        found
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path_exists(fixture: &Fixture, relative: &str) -> bool {
    Path::new(&fixture.0.join(relative)).is_file()
}

#[test]
fn ordinary_sources_are_still_collected() {
    let fixture = Fixture::new("ordinary");
    fixture.write("src/lib.rs", "pub fn a() {}\n");
    fixture.write("tests/one.rs", "#[test]\nfn one() {}\n");
    assert_eq!(fixture.collected(), ["src/lib.rs", "tests/one.rs"]);
}

#[test]
fn a_target_declared_under_a_dot_directory_is_collected() {
    let fixture = Fixture::new("declared");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[package]\nname = \"hidden\"\n",
            "[[bin]]\nname = \"hidden\"\npath = \".hidden/main.rs\"\n",
        ),
    );
    fixture.write(".hidden/main.rs", "fn main() {}\n");
    assert_eq!(fixture.collected(), [".hidden/main.rs"]);
}

#[test]
fn a_nested_source_declared_under_a_dot_directory_is_collected() {
    let fixture = Fixture::new("nested");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[workspace]\nmembers = [\".member\"]\n",
            "[package]\nname = \"root\"\n",
            "[[bin]]\nname = \"root\"\npath = \".hidden/deep/main.rs\"\n",
        ),
    );
    fixture.write(".member/Cargo.toml", "[package]\nname = \"member\"\n");
    fixture.write(".member/src/lib.rs", "pub fn b() {}\n");
    fixture.write(".hidden/deep/main.rs", "fn main() {}\n");
    assert_eq!(
        fixture.collected(),
        [".hidden/deep/main.rs", ".member/src/lib.rs"]
    );
}

#[test]
fn a_workspace_glob_covers_a_dot_member() {
    // The list is written across lines and uses the same glob the repository
    // does, so a member directory named with a leading dot is still compiled.
    let fixture = Fixture::new("glob");
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\n    \"crates/*\",\n]\n",
    );
    fixture.write("crates/plain/Cargo.toml", "[package]\nname = \"plain\"\n");
    fixture.write("crates/plain/src/lib.rs", "pub fn plain() {}\n");
    fixture.write(
        "crates/.hidden/Cargo.toml",
        "[package]\nname = \"hidden\"\n",
    );
    fixture.write("crates/.hidden/src/lib.rs", "pub fn hidden() {}\n");
    assert_eq!(
        fixture.collected(),
        ["crates/.hidden/src/lib.rs", "crates/plain/src/lib.rs"]
    );
}

#[test]
fn a_dot_component_below_the_manifest_is_collected() {
    let fixture = Fixture::new("below");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[package]\nname = \"below\"\n",
            "[[bin]]\nname = \"below\"\npath = \"src/.hidden/main.rs\"\n",
        ),
    );
    fixture.write("src/.hidden/main.rs", "fn main() {}\n");
    assert_eq!(fixture.collected(), ["src/.hidden/main.rs"]);
}

#[test]
fn a_path_is_read_in_both_toml_spellings_and_both_separators() {
    let fixture = Fixture::new("spellings");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[package]\nname = \"spellings\"\n",
            "[[bin]]\nname = \"one\"\npath = './.one/main.rs'\n",
            "[[bin]]\nname = \"two\"\npath = \".two\\\\main.rs\"\n",
        ),
    );
    fixture.write(".one/main.rs", "fn one() {}\n");
    fixture.write(".two/main.rs", "fn two() {}\n");
    assert_eq!(fixture.collected(), [".one/main.rs", ".two/main.rs"]);
}

#[test]
fn a_comment_naming_a_dot_directory_does_not_exempt_it() {
    let fixture = Fixture::new("comment");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[package]\nname = \"comment\"\n",
            "[[bin]]\nname = \"comment\"\npath = \".hidden/main.rs\" # moved out of \".cache/main.rs\"\n",
        ),
    );
    fixture.write(".hidden/main.rs", "fn main() {}\n");
    fixture.write(".cache/blob.rs", "pub fn cached() {}\n");
    assert_eq!(fixture.collected(), [".hidden/main.rs"]);
}

#[test]
fn an_exclude_list_is_not_a_declaration() {
    // An exclude list says what is *not* a member, so it cannot make a
    // directory part of the walk.
    let fixture = Fixture::new("exclude");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[workspace]\nmembers = []\nexclude = [\".git\", \".cache\"]\n",
            "[package]\nname = \"exclude\"\n",
        ),
    );
    fixture.write("src/lib.rs", "pub fn kept() {}\n");
    fixture.write(".git/objects/blob.rs", "pub fn git() {}\n");
    fixture.write(".cache/blob.rs", "pub fn cached() {}\n");
    assert_eq!(fixture.collected(), ["src/lib.rs"]);
}

#[test]
fn a_dot_directory_no_manifest_names_is_not_walked() {
    // A nested project owns its own run, the way the checks drive one manifest
    // at a time; a stray Rust file is not source just because it ends in .rs.
    let fixture = Fixture::new("unnamed");
    fixture.write("Cargo.toml", "[workspace]\nmembers = []\n");
    fixture.write(".scratch/project/Cargo.toml", "[package]\nname = \"p\"\n");
    fixture.write(".scratch/project/src/lib.rs", "pub fn c() {}\n");
    assert_eq!(fixture.collected(), Vec::<String>::new());
    assert_eq!(
        collect(&[fixture.0.join(".scratch/project")], &[])
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn caches_and_build_output_stay_out_of_the_walk() {
    let fixture = Fixture::new("caches");
    fixture.write(
        "Cargo.toml",
        "[package]\nname = \"caches\"\n[[bin]]\nname = \"c\"\npath = \"src/main.rs\"\n",
    );
    fixture.write("src/main.rs", "fn main() {}\n");
    for directory in [
        "target",
        "node_modules",
        "artifacts",
        "__pycache__",
        ".cache",
        ".venv",
        ".git",
    ] {
        fixture.write(&format!("{directory}/hidden.rs"), "fn hidden() {}\n");
    }
    // Nothing named above may appear, even though it holds a Rust file.
    assert_eq!(fixture.collected(), ["src/main.rs"]);
    for directory in ["target", "node_modules", "artifacts", "__pycache__"] {
        assert!(path_exists(&fixture, &format!("{directory}/hidden.rs")));
    }
}

#[test]
fn an_explicit_skip_still_wins_over_a_declared_dot_directory() {
    let fixture = Fixture::new("skip");
    fixture.write(
        "Cargo.toml",
        concat!(
            "[package]\nname = \"skip\"\n",
            "[[bin]]\nname = \"vendor\"\npath = \".vendor/dependency.rs\"\n",
        ),
    );
    fixture.write(".vendor/dependency.rs", "fn main() {}\n");
    // Without the skip the declared target is walked, so the skip is what the
    // assertion below is really measuring.
    assert_eq!(fixture.collected(), [".vendor/dependency.rs"]);
    let skipped = fixture.0.join(".vendor");
    let found = collect(std::slice::from_ref(&fixture.0), &[skipped]).unwrap();
    assert!(found.is_empty(), "{found:?}");
}
