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
    fixture.write("src/lib.rs", "pub fn d() {}\n");
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
    assert_eq!(fixture.collected(), ["src/lib.rs"]);
    for directory in ["target", "node_modules", "artifacts", "__pycache__"] {
        assert!(path_exists(&fixture, &format!("{directory}/hidden.rs")));
    }
}

#[test]
fn an_explicit_skip_still_wins_over_a_dot_directory() {
    let fixture = Fixture::new("skip");
    fixture.write("Cargo.toml", "[package]\nname = \"root\"\n");
    fixture.write(".vendor/dependency.rs", "pub fn e() {}\n");
    let skipped = fixture.0.join(".vendor");
    let found = collect(std::slice::from_ref(&fixture.0), &[skipped]).unwrap();
    assert!(found.is_empty(), "{found:?}");
}
