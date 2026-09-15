import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const source = fileURLToPath(new URL("../", import.meta.url));
function git(root, ...args) {
  return execFileSync("git", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}
function command(root, program, args, options = {}) {
  return spawnSync(program, args, { cwd: root, encoding: "utf8", ...options });
}
function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "eden-hooks-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  git(root, "init", "-b", "main");
  git(root, "config", "user.name", "Hook Test");
  git(root, "config", "user.email", "hooks@example.invalid");
  for (const name of [
    ".githooks",
    "scripts",
    ".markdownlint-cli2.jsonc",
    "pyproject.toml",
    "uv.lock",
    "biome.json",
    "package.json",
    "rustfmt.toml",
    "rust-toolchain.toml",
    ".gitattributes",
  ]) {
    if (existsSync(join(source, name)))
      cpSync(join(source, name), join(root, name), { recursive: true });
  }
  symlinkSync(
    join(source, "node_modules"),
    join(root, "node_modules"),
    process.platform === "win32" ? "junction" : "dir",
  );
  symlinkSync(
    join(source, ".venv"),
    join(root, ".venv"),
    process.platform === "win32" ? "junction" : "dir",
  );
  writeFileSync(join(root, ".gitignore"), "node_modules\n.venv\ntarget/\n");
  mkdirSync(join(root, "src"));
  writeFileSync(
    join(root, "Cargo.toml"),
    '[package]\nname = "hooks-fixture"\nversion = "0.1.0"\nedition = "2024"\n',
  );
  writeFileSync(
    join(root, "Cargo.lock"),
    'version = 4\n\n[[package]]\nname = "hooks-fixture"\nversion = "0.1.0"\n',
  );
  writeFileSync(join(root, "src/lib.rs"), "pub fn value() -> u32 {\n    1\n}\n");
  writeFileSync(join(root, "README.md"), "# Fixture\n\nValid documentation.\n");
  git(root, "add", ".");
  git(root, "-c", "core.hooksPath=", "commit", "-m", "baseline");
  const setup = command(root, process.execPath, ["scripts/install-hooks.mjs"]);
  assert.equal(setup.status, 0, setup.stderr);
  return root;
}
function unchanged(root, path, staged, worktree) {
  assert.equal(git(root, "show", `:${path}`), staged);
  assert.equal(readFileSync(join(root, path), "utf8"), worktree);
}

test("pre-commit rejects bad staged Markdown despite an unstaged fix, without rewriting either", (t) => {
  const root = fixture(t);
  const bad = "# Fixture\n\n##Duplicate\n";
  const good = "# Fixture\n\n## Corrected\n";
  writeFileSync(join(root, "README.md"), bad);
  git(root, "add", "README.md");
  writeFileSync(join(root, "README.md"), good);
  const before = git(root, "rev-parse", "HEAD");
  const result = command(root, "git", ["commit", "-m", "invalid staged Markdown"]);
  assert.notEqual(result.status, 0, "hook accepted malformed staged Markdown");
  assert.match(result.stdout + result.stderr, /MD018/);
  assert.equal(git(root, "rev-parse", "HEAD"), before);
  unchanged(root, "README.md", bad, good);
});

test("pre-commit checks staged Rust and preserves partial staging in both directions", (t) => {
  const root = fixture(t);
  const bad = "pub fn value()->u32{2}\n";
  const good = "pub fn value() -> u32 {\n    2\n}\n";
  writeFileSync(join(root, "src/lib.rs"), bad);
  git(root, "add", "src/lib.rs");
  writeFileSync(join(root, "src/lib.rs"), good);
  let result = command(root, "git", ["commit", "-m", "bad staged Rust"]);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout + result.stderr, /Diff in/);
  unchanged(root, "src/lib.rs", bad, good);
  git(root, "add", "src/lib.rs");
  writeFileSync(join(root, "src/lib.rs"), bad);
  result = command(root, "git", ["commit", "-m", "good staged Rust"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(git(root, "show", "HEAD:src/lib.rs"), good);
  unchanged(root, "src/lib.rs", good, bad);
});

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

test("pre-commit uses staged lint configuration and checks remaining Markdown on deletion", (t) => {
  const root = fixture(t);
  const normalConfig = readFileSync(join(root, ".markdownlint-cli2.jsonc"), "utf8");
  writeFileSync(
    join(root, ".markdownlint-cli2.jsonc"),
    '{"config":{"default":true,"MD013":false,"MD018":false},"ignores":["node_modules/**"]}\n',
  );
  writeFileSync(join(root, "README.md"), "# Fixture\n\n##Unspaced\n");
  git(root, "add", "README.md", ".markdownlint-cli2.jsonc");
  writeFileSync(join(root, ".markdownlint-cli2.jsonc"), normalConfig);
  const result = command(root, "git", ["commit", "-m", "staged rules allow this text"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(readFileSync(join(root, ".markdownlint-cli2.jsonc"), "utf8"), normalConfig);
  git(root, "add", ".markdownlint-cli2.jsonc");
  const strict = command(root, "git", ["commit", "-m", "stricter staged rules"]);
  assert.notEqual(strict.status, 0);
  assert.match(strict.stdout + strict.stderr, /MD018/);
  // Introduce a bad tracked document without running hooks, then stage only a deletion.
  git(root, "-c", "core.hooksPath=", "commit", "-m", "fixture with lint debt");
  writeFileSync(join(root, "extra.md"), "# Extra\n");
  git(root, "add", "extra.md");
  git(root, "-c", "core.hooksPath=", "commit", "-m", "deletable document");
  git(root, "rm", "extra.md");
  const deletion = command(root, "git", ["commit", "-m", "delete only"]);
  assert.notEqual(deletion.status, 0);
  assert.match(deletion.stdout + deletion.stderr, /MD018/);
});

test("pre-commit excludes intent-to-add files that are absent from the actual commit", (t) => {
  const root = fixture(t);
  const relative = "tests/contract-authors/pending/Cargo.toml";
  mkdirSync(dirname(join(root, relative)), { recursive: true });
  writeFileSync(join(root, relative), "an unfinished manifest\n");
  git(root, "add", "-N", relative);
  writeFileSync(join(root, "src/lib.rs"), "pub fn value() -> u32 {\n    3\n}\n");
  git(root, "add", "src/lib.rs");
  const staged = git(root, "ls-files", "--stage", "-z");
  const result = command(root, process.execPath, ["scripts/hooks.mjs", "pre-commit"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(git(root, "ls-files", "--stage", "-z"), staged);
  assert.equal(readFileSync(join(root, relative), "utf8"), "an unfinished manifest\n");
  git(root, "commit", "-m", "only the staged Rust change");
  assert.doesNotMatch(git(root, "ls-tree", "-r", "--name-only", "HEAD"), /pending\/Cargo.toml/);
});

test("git commit --only checks Git's temporary index and leaves unrelated staging intact", (t) => {
  const root = fixture(t);
  const bad = "# Fixture\n\n##Unstaged-in-this-commit\n";
  writeFileSync(join(root, "README.md"), bad);
  git(root, "add", "README.md");
  const rust = "pub fn value() -> u32 {\n    4\n}\n";
  writeFileSync(join(root, "src/lib.rs"), rust);
  const result = command(root, "git", ["commit", "--only", "src/lib.rs", "-m", "only Rust"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(git(root, "show", "HEAD:src/lib.rs"), rust);
  assert.equal(git(root, "show", "HEAD:README.md"), "# Fixture\n\nValid documentation.\n");
  unchanged(root, "README.md", bad, bad);
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

test("pre-commit omits intent-to-add placeholders for paths already present in HEAD", (t) => {
  const root = fixture(t);
  const relative = "tests/contract-authors/existing/Cargo.toml";
  const author = dirname(join(root, relative));
  mkdirSync(join(author, "src"), { recursive: true });
  const manifest =
    '[package]\nname = "existing"\nversion = "0.1.0"\nedition = "2024"\n[workspace]\n';
  writeFileSync(join(root, relative), manifest);
  writeFileSync(join(author, "src/lib.rs"), "pub fn value() -> u32 {\n    1\n}\n");
  git(root, "add", "tests");
  git(root, "commit", "-m", "existing independent author");
  git(root, "rm", "--cached", relative);
  git(root, "add", "-N", relative);
  const staged = git(root, "ls-files", "--stage", "-z");
  const result = command(root, process.execPath, ["scripts/hooks.mjs", "pre-commit"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(git(root, "ls-files", "--stage", "-z"), staged);
  assert.equal(readFileSync(join(root, relative), "utf8"), manifest);
});

for (const [name, bad, good, diagnostic] of [
  ["Python formatting", "value=1\n", "value = 1\n", /Would reformat/],
  ["Ruff lint", "import os\n", "value = 1\n", /F401/],
  ["Python types", 'value: int = "wrong"\n', "value: int = 1\n", /reportAssignmentType/],
  ["Biome", "debugger;\n", 'console.log("ok");\n', /noDebugger/],
]) {
  test(`pre-commit checks staged ${name} and preserves an unstaged repair`, (t) => {
    const root = fixture(t);
    const path = name === "Biome" ? "sample.mjs" : "sample.py";
    writeFileSync(join(root, path), bad);
    git(root, "add", path);
    writeFileSync(join(root, path), good);
    const result = command(root, "git", ["commit", "-m", `bad staged ${name}`]);
    assert.notEqual(result.status, 0);
    assert.match(result.stdout + result.stderr, diagnostic);
    unchanged(root, path, bad, good);
    git(root, "add", path);
    writeFileSync(join(root, path), bad);
    const repaired = command(root, "git", ["commit", "-m", `good staged ${name}`]);
    assert.equal(repaired.status, 0, repaired.stdout + repaired.stderr);
    unchanged(root, path, good, bad);
  });
}

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

test("private-style submodule boundaries are not traversed by snapshot checks", (t) => {
  const root = fixture(t);
  const oid = git(root, "rev-parse", "HEAD").trim();
  git(root, "update-index", "--add", "--cacheinfo", `160000,${oid},nested-product`);
  writeFileSync(join(root, "sample.py"), "value = 1\n");
  git(root, "add", "sample.py");
  const result = command(root, "git", ["commit", "-m", "own source beside gitlink"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
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
