import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { command, fixture, git, unchanged } from "./development-hooks-fixture.mjs";

test("pre-push rejects Clippy errors in a non-HEAD ref without changing the checkout", (t) => {
  const root = fixture(t);
  const remote = mkdtempSync(join(tmpdir(), "eden-hooks-remote-"));
  t.after(() => rmSync(remote, { recursive: true, force: true }));
  git(remote, "init", "--bare");
  git(root, "-c", "core.hooksPath=", "push", remote, "main");
  git(root, "switch", "-c", "bad");
  const bad = "pub fn value() -> u32 {\n    let unused = 7;\n    1\n}\n";
  writeFileSync(join(root, "src/lib.rs"), bad);
  git(root, "add", "src/lib.rs");
  git(root, "commit", "-m", "Clippy should reject this");
  git(root, "switch", "main");
  const head = git(root, "rev-parse", "HEAD");
  const index = git(root, "show", ":src/lib.rs");
  const worktree = readFileSync(join(root, "src/lib.rs"), "utf8");
  const result = command(root, "git", ["push", remote, "bad:refs/heads/topic"]);
  assert.notEqual(result.status, 0, "push accepted the unchecked non-HEAD branch");
  assert.match(result.stdout + result.stderr, /unused variable/);
  assert.equal(git(root, "rev-parse", "HEAD"), head);
  unchanged(root, "src/lib.rs", index, worktree);
  assert.equal(git(remote, "for-each-ref", "--format=%(refname)", "refs/heads/topic"), "");
});

test("standalone author crates participate in formatting and Clippy hooks", (t) => {
  const root = fixture(t);
  const author = join(root, "tests", "contract-authors", "new-author");
  mkdirSync(join(author, "src"), { recursive: true });
  writeFileSync(
    join(author, "Cargo.toml"),
    '[package]\nname = "new-author"\nversion = "0.1.0"\nedition = "2024"\n[workspace]\n',
  );
  writeFileSync(
    join(author, "Cargo.lock"),
    'version = 4\n\n[[package]]\nname = "new-author"\nversion = "0.1.0"\n',
  );
  writeFileSync(join(author, "src/lib.rs"), "pub fn value()->u32{2}\n");
  git(root, "add", "tests");
  let result = command(root, "git", ["commit", "-m", "bad author format"]);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout + result.stderr, /new-author/);
  assert.match(result.stdout + result.stderr, /Diff in/);
  writeFileSync(
    join(author, "src/lib.rs"),
    "pub fn value() -> u32 {\n    let unused = 2;\n    2\n}\n",
  );
  git(root, "add", "tests");
  git(root, "commit", "-m", "formatted author with lint error");
  const remote = mkdtempSync(join(tmpdir(), "eden-hooks-remote-"));
  t.after(() => rmSync(remote, { recursive: true, force: true }));
  git(remote, "init", "--bare");
  result = command(root, "git", ["push", remote, "main:refs/heads/topic"]);
  assert.notEqual(result.status, 0);
  assert.match(
    result.stdout + result.stderr,
    /clippy: tests\/contract-authors\/new-author\/Cargo.toml/,
  );
  assert.match(result.stdout + result.stderr, /unused variable/);
});

test("pre-push accepts committed code despite dirty staged and worktree files, and skips deletions", (t) => {
  const root = fixture(t);
  const remote = mkdtempSync(join(tmpdir(), "eden-hooks-remote-"));
  t.after(() => rmSync(remote, { recursive: true, force: true }));
  git(remote, "init", "--bare");
  const staged = "pub fn value() -> u32 {\n    let unused = 2;\n    2\n}\n";
  const working = "not even Rust\n";
  writeFileSync(join(root, "src/lib.rs"), staged);
  git(root, "add", "src/lib.rs");
  writeFileSync(join(root, "src/lib.rs"), working);
  let result = command(root, "git", ["push", remote, "main:refs/heads/topic"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.match(result.stdout + result.stderr, /Clippy for pushed/);
  unchanged(root, "src/lib.rs", staged, working);
  result = command(root, "git", ["push", remote, ":refs/heads/topic"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.doesNotMatch(result.stdout + result.stderr, /Clippy for pushed/);
  unchanged(root, "src/lib.rs", staged, working);
});

test("multi-ref push checks both an existing remote update and a second branch", (t) => {
  const root = fixture(t);
  const remote = mkdtempSync(join(tmpdir(), "eden-hooks-remote-"));
  t.after(() => rmSync(remote, { recursive: true, force: true }));
  git(remote, "init", "--bare");
  git(root, "-c", "core.hooksPath=", "push", remote, "main");
  const baseline = git(root, "rev-parse", "HEAD").trim();
  writeFileSync(join(root, "src/lib.rs"), "pub fn value() -> u32 {\n    5\n}\n");
  git(root, "add", "src/lib.rs");
  git(root, "commit", "-m", "good update");
  const good = git(root, "rev-parse", "HEAD").trim();
  git(root, "switch", "-c", "other", baseline);
  writeFileSync(
    join(root, "src/lib.rs"),
    "pub fn value() -> u32 {\n    let unused = 9;\n    9\n}\n",
  );
  git(root, "add", "src/lib.rs");
  git(root, "commit", "-m", "bad second ref");
  git(root, "switch", "main");
  let result = command(root, "git", ["push", remote, "main:main", "other:topic"]);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout + result.stderr, /unused variable/);
  assert.equal(git(remote, "rev-parse", "refs/heads/main").trim(), baseline);
  assert.equal(git(remote, "for-each-ref", "--format=%(refname)", "refs/heads/topic"), "");
  git(root, "switch", "other");
  writeFileSync(join(root, "src/lib.rs"), "pub fn value() -> u32 {\n    9\n}\n");
  git(root, "add", "src/lib.rs");
  git(root, "commit", "-m", "fix second ref");
  const other = git(root, "rev-parse", "HEAD").trim();
  result = command(root, "git", ["push", remote, "main:main", "other:topic"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(git(remote, "rev-parse", "refs/heads/main").trim(), good);
  assert.equal(git(remote, "rev-parse", "refs/heads/topic").trim(), other);
});

test("Python checks read staged configuration and reject type errors in a non-HEAD push", (t) => {
  const root = fixture(t);
  const bad = 'value: int = "wrong"\n';
  writeFileSync(join(root, "sample.py"), bad);
  git(root, "add", "sample.py");
  const config = readFileSync(join(root, "pyproject.toml"), "utf8");
  writeFileSync(
    join(root, "pyproject.toml"),
    config.replace('typeCheckingMode = "standard"', 'typeCheckingMode = "off"'),
  );
  const rejected = command(root, "git", ["commit", "-m", "staged standard must apply"]);
  assert.notEqual(rejected.status, 0);
  assert.match(rejected.stdout + rejected.stderr, /reportAssignmentType/);
  writeFileSync(join(root, "pyproject.toml"), config);
  git(root, "switch", "-c", "bad-python");
  git(root, "-c", "core.hooksPath=", "commit", "-m", "type error fixture");
  git(root, "switch", "main");
  const remote = mkdtempSync(join(tmpdir(), "eden-python-remote-"));
  t.after(() => rmSync(remote, { recursive: true, force: true }));
  git(remote, "init", "--bare");
  const head = git(root, "rev-parse", "HEAD");
  const pushed = command(root, "git", ["push", remote, "bad-python:topic"]);
  assert.notEqual(pushed.status, 0);
  assert.match(pushed.stdout + pushed.stderr, /reportAssignmentType/);
  assert.equal(git(root, "rev-parse", "HEAD"), head);
  assert.equal(git(remote, "for-each-ref", "--format=%(refname)", "refs/heads/topic"), "");
});

test("same-commit multi-ref push checks the union of each ref's required families", (t) => {
  const root = fixture(t);
  writeFileSync(join(root, "sample.py"), 'value: int = "wrong"\n');
  git(root, "add", "sample.py");
  git(root, "-c", "core.hooksPath=", "commit", "-m", "existing type debt");
  const previous = git(root, "rev-parse", "HEAD").trim();
  writeFileSync(join(root, "README.md"), "# Fixture\n\nUpdated documentation.\n");
  git(root, "add", "README.md");
  git(root, "commit", "-m", "documentation update");
  const head = git(root, "rev-parse", "HEAD").trim();
  const staged = git(root, "ls-files", "--stage", "-z");
  const result = command(root, process.execPath, ["scripts/hooks.mjs", "pre-push"], {
    input: `refs/heads/main ${head} refs/heads/main ${previous}\nrefs/heads/new ${head} refs/heads/new ${"0".repeat(40)}\n`,
  });
  assert.notEqual(result.status, 0, "new ref skipped checks already absent from first ref delta");
  assert.match(result.stdout + result.stderr, /reportAssignmentType/);
  assert.equal(git(root, "rev-parse", "HEAD").trim(), head);
  assert.equal(git(root, "ls-files", "--stage", "-z"), staged);
  assert.equal(readFileSync(join(root, "sample.py"), "utf8"), 'value: int = "wrong"\n');
});
