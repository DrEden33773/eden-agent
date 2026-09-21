import assert from "node:assert/strict";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { command, fixture, git, unchanged } from "./development-hooks-fixture.mjs";

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
  assert.match(result.stdout + result.stderr, /would be reformatted/);
  unchanged(root, "src/lib.rs", bad, good);
  git(root, "add", "src/lib.rs");
  writeFileSync(join(root, "src/lib.rs"), bad);
  result = command(root, "git", ["commit", "-m", "good staged Rust"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.equal(git(root, "show", "HEAD:src/lib.rs"), good);
  unchanged(root, "src/lib.rs", good, bad);
});

test("pre-commit runs every family when the hook sources themselves change", (t) => {
  const root = fixture(t);
  // A committed change to the check sources runs all five families, which is the
  // one case where the doc check also belongs to a commit rather than a push.
  writeFileSync(
    join(root, "scripts/hooks.mjs"),
    `${readFileSync(join(root, "scripts/hooks.mjs"), "utf8")}\n// fixture infrastructure change\n`,
  );
  writeFileSync(
    join(root, "src/lib.rs"),
    "/// See [`moved_away`] for the contract.\npub fn value() -> u32 {\n    4\n}\n",
  );
  git(root, "add", "scripts/hooks.mjs", "src/lib.rs");
  const result = command(root, "git", ["commit", "-m", "infrastructure change with a doc link"]);
  const output = result.stdout + result.stderr;
  assert.notEqual(result.status, 0, "commit accepted a broken doc link beside a hook change");
  assert.match(output, /doc --workspace --no-deps --locked failed/);
  assert.match(output, /unresolved link/);
});

test("the doc check keeps a RUSTDOCFLAGS it was given and adds its own lints", (t) => {
  const root = fixture(t);
  // `inner` is private, so rustdoc documents nothing in it and the default check
  // cannot see the broken link. Only a RUSTDOCFLAGS the contributor supplied can
  // reach it, which is how this pins that the environment is kept rather than
  // replaced: the first run passes and the second one has to fail.
  writeFileSync(
    join(root, "src/inner.rs"),
    "/// See [`missing_link`].\npub(crate) fn helper() -> u32 {\n    1\n}\n",
  );
  writeFileSync(join(root, "src/lib.rs"), "mod inner;\n\npub fn value() -> u32 {\n    1\n}\n");
  let result = command(root, process.execPath, ["scripts/checks.mjs", "doc"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  result = command(root, process.execPath, ["scripts/checks.mjs", "doc"], {
    env: { ...process.env, RUSTDOCFLAGS: "-D warnings --document-private-items" },
  });
  const output = result.stdout + result.stderr;
  assert.notEqual(result.status, 0, "the doc check dropped the RUSTDOCFLAGS it was given");
  assert.match(output, /unresolved link/);
});

test("pre-commit rejects an unformatted json! body in the root workspace", (t) => {
  const root = fixture(t);
  const body = 'pub fn value() -> u32 {\n    let v = json!({"a":1});\n    1\n}\n';
  writeFileSync(join(root, "src/lib.rs"), body);
  git(root, "add", "src/lib.rs");
  const result = command(root, "git", ["commit", "-m", "unformatted macro body"]);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout + result.stderr, /would be reformatted/);
  unchanged(root, "src/lib.rs", body, body);
});

test("pre-commit rejects an unformatted select! body in the root workspace", (t) => {
  const root = fixture(t);
  const body =
    "pub async fn value() -> u32 {\n    tokio::select! {\n    biased;\n    _=a()=>1,\n    _=b()=>2,\n    }\n}\n";
  writeFileSync(join(root, "src/lib.rs"), body);
  git(root, "add", "src/lib.rs");
  const result = command(root, "git", ["commit", "-m", "unformatted select body"]);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout + result.stderr, /would be reformatted/);
  unchanged(root, "src/lib.rs", body, body);
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

test("private-style submodule boundaries are not traversed by snapshot checks", (t) => {
  const root = fixture(t);
  const oid = git(root, "rev-parse", "HEAD").trim();
  git(root, "update-index", "--add", "--cacheinfo", `160000,${oid},nested-product`);
  writeFileSync(join(root, "sample.py"), "value = 1\n");
  git(root, "add", "sample.py");
  const result = command(root, "git", ["commit", "-m", "own source beside gitlink"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
});

test("autocrlf checkout preserves Biome source and configuration line endings", (t) => {
  const root = fixture(t);
  git(root, "-c", "core.autocrlf=true", "checkout-index", "--force", "--all");
  for (const name of ["biome.json", "scripts/checks.mjs", "scripts/hooks.mjs"]) {
    assert.doesNotMatch(readFileSync(join(root, name), "utf8"), /\r/);
  }
  const result = command(root, process.execPath, ["scripts/checks.mjs", "javascript"]);
  assert.equal(result.status, 0, result.stdout + result.stderr);
});

test("nightly checks catch long-string layout in an independent author and preserve staging", (t) => {
  const root = fixture(t);
  const author = join(root, "tests/contract-authors/long-message");
  mkdirSync(join(author, "src"), { recursive: true });
  writeFileSync(
    join(author, "Cargo.toml"),
    '[package]\nname="long-message"\nversion="0.1.0"\nedition="2024"\n[workspace]\n',
  );
  const path = "tests/contract-authors/long-message/src/lib.rs";
  const bad = `pub fn message()-> &'static str { "${"ordinary long message ".repeat(12)}" }\n`;
  writeFileSync(join(root, path), bad);
  git(root, "add", "tests");
  const failed = command(root, process.execPath, ["scripts/hooks.mjs", "pre-commit"]);
  assert.notEqual(failed.status, 0);
  unchanged(root, path, bad, bad);
  const fixed = command(root, process.execPath, ["scripts/checks.mjs", "format"]);
  assert.equal(fixed.status, 0, fixed.stdout + fixed.stderr);
  assert.match(readFileSync(join(root, path), "utf8"), /\\\n/);
  git(root, "add", "tests");
  const passed = command(root, process.execPath, ["scripts/hooks.mjs", "pre-commit"]);
  assert.equal(passed.status, 0, passed.stdout + passed.stderr);
});

test("editor command from a nested source directory matches CLI stdin", () => {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const settings = JSON.parse(readFileSync(join(root, ".vscode/settings.json"), "utf8"));
  const [program, ...args] = settings["rust-analyzer.rustfmt.overrideCommand"].map((value) =>
    value.replace("${workspaceFolder}", root),
  );
  const source = `fn f(){let message="${"a long ordinary message ".repeat(12)}";}\n`;
  const nested = join(root, "tests/contract-authors/service-a/src");
  const editor = command(nested, program, args, { input: source });
  assert.equal(editor.status, 0, editor.stderr);
  const cli = command(root, process.env.EDEN_FMT, ["stdin"], { input: source });
  assert.equal(cli.status, 0, cli.stderr);
  assert.equal(editor.stdout, cli.stdout);
  assert.match(editor.stdout, /\\\n/);
});
