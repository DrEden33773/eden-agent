import assert from "node:assert/strict";
import { cpSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { command, fixture, git, unchanged } from "./development-hooks-fixture.mjs";

const source = fileURLToPath(new URL("../", import.meta.url));
const runHook = (root, phase, input) =>
  command(root, process.execPath, ["scripts/hooks.mjs", phase], { input });
const output = (result) => result.stdout + result.stderr;
const commit = (root) => {
  git(root, "add", ".");
  git(root, "-c", "core.hooksPath=", "commit", "-m", "fixture");
  return git(root, "rev-parse", "HEAD").trim();
};
function webFixture(t) {
  const root = fixture(t);
  for (const path of [
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "web/presentation/package.json",
    "web/presentation/tsconfig.json",
  ]) {
    mkdirSync(join(root, path, ".."), { recursive: true });
    cpSync(join(source, path), join(root, path));
  }
  mkdirSync(join(root, "web/presentation/src"), { recursive: true });
  writeFileSync(join(root, "web/presentation/src/value.ts"), "export const value: number = 1;\n");
  symlinkSync(
    join(source, "web/presentation/node_modules"),
    join(root, "web/presentation/node_modules"),
    process.platform === "win32" ? "junction" : "dir",
  );
  commit(root);
  return root;
}
const push = (root, head, base) =>
  runHook(root, "pre-push", `refs/heads/topic ${head} refs/heads/topic ${base}\n`);

for (const [extension, bad, good] of [
  ["js", "export const value=1\n", "export const value = 1;\n"],
  ["jsx", "export const value=<div/>\n", "export const value = <div />;\n"],
  ["mjs", "export const value=1\n", "export const value = 1;\n"],
  ["ts", "export const value:number=1\n", "export const value: number = 1;\n"],
  ["tsx", "export const value=<div/>\n", "export const value = <div />;\n"],
  ["css", "body{color:red}", "body {\n  color: red;\n}\n"],
  ["html", '<div  id="root"></div>', '<div id="root"></div>\n'],
]) {
  test(`${extension}: both hooks check snapshots and leave staged/working bytes unchanged`, (t) => {
    const root = fixture(t);
    const path = `sample.${extension}`;
    const base = git(root, "rev-parse", "HEAD").trim();
    writeFileSync(join(root, path), bad);
    git(root, "add", path);
    writeFileSync(join(root, path), good);
    const rejected = runHook(root, "pre-commit");
    assert.notEqual(rejected.status, 0, output(rejected));
    assert.match(output(rejected), /formatter|formatting/i);
    unchanged(root, path, bad, good);
    git(root, "-c", "core.hooksPath=", "commit", "-m", "bad committed snapshot");
    const head = git(root, "rev-parse", "HEAD").trim();
    const pushed = push(root, head, base);
    assert.notEqual(pushed.status, 0, output(pushed));
    assert.match(output(pushed), /formatter|formatting/i);
    unchanged(root, path, bad, good);
    git(root, "add", path);
    const accepted = runHook(root, "pre-commit");
    assert.equal(accepted.status, 0, output(accepted));
  });
}

test("pre-push rejects non-HEAD TypeScript errors; pre-commit checks formatting only", (t) => {
  const root = webFixture(t);
  const base = git(root, "rev-parse", "HEAD").trim();
  const path = "web/presentation/src/value.ts";
  const bad = 'export const value: number = "wrong";\n';
  const good = "export const value: number = 1;\n";
  writeFileSync(join(root, path), bad);
  git(root, "add", path);
  const preCommit = runHook(root, "pre-commit");
  assert.equal(preCommit.status, 0, output(preCommit));
  const head = commit(root);
  git(root, "checkout", "--detach", base);
  const index = git(root, "ls-files", "--stage", "-z");
  const rejected = push(root, head, base);
  assert.notEqual(rejected.status, 0, output(rejected));
  assert.match(output(rejected), /TS2322/);
  assert.equal(git(root, "rev-parse", "HEAD").trim(), base);
  assert.equal(git(root, "ls-files", "--stage", "-z"), index);
  unchanged(root, path, good, good);
  writeFileSync(join(root, path), bad);
  git(root, "add", path);
  writeFileSync(join(root, path), "unfinished source\n");
  const accepted = push(root, base, head);
  assert.equal(accepted.status, 0, output(accepted));
  unchanged(root, path, bad, "unfinished source\n");
});

test("pre-push uses committed tsconfig and rejects missing or mismatched installed dependencies", (t) => {
  const root = webFixture(t);
  const base = git(root, "rev-parse", "HEAD").trim();
  const path = "web/presentation/src/value.ts";
  writeFileSync(join(root, path), "export const value: number = 2;\n");
  const head = commit(root);
  for (const file of [
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "package.json",
    "web/presentation/package.json",
  ]) {
    const original = readFileSync(join(root, file));
    writeFileSync(join(root, file), `${original}\n`);
    const result = push(root, head, base);
    assert.notEqual(result.status, 0, output(result));
    assert.match(output(result), /Web snapshot dependencies do not match/);
    writeFileSync(join(root, file), original);
  }
  // A worktree override must not hide the pushed configuration's stricter rule.
  writeFileSync(join(root, path), "export function value(input) {\n  return input;\n}\n");
  const bad = commit(root);
  const config = join(root, "web/presentation/tsconfig.json");
  writeFileSync(config, readFileSync(config, "utf8").replace('"strict": true', '"strict": false'));
  const rejected = push(root, bad, head);
  assert.notEqual(rejected.status, 0, output(rejected));
  assert.match(output(rejected), /TS7006/);
  rmSync(join(root, "web/presentation/node_modules"), { recursive: true });
  const missing = push(root, head, base);
  assert.notEqual(missing.status, 0, output(missing));
  assert.match(output(missing), /pnpm install --frozen-lockfile/);
});

test("deletion, rename and nested config changes trigger checks of remaining sources", (t) => {
  const root = fixture(t);
  mkdirSync(join(root, "nested"));
  writeFileSync(join(root, "nested/bad.ts"), "export const bad=1\n");
  writeFileSync(join(root, "nested/other.css"), "body {}\n");
  commit(root);
  git(root, "rm", "nested/other.css");
  let result = runHook(root, "pre-commit");
  assert.notEqual(result.status, 0, output(result));
  assert.match(output(result), /nested[/\\]bad.ts/);
  git(root, "reset", "--hard", "HEAD");
  git(root, "mv", "nested/other.css", "nested/other.txt");
  result = runHook(root, "pre-commit");
  assert.notEqual(result.status, 0, output(result));
  assert.match(output(result), /nested[/\\]bad.ts/);
  git(root, "reset", "--hard", "HEAD");
  for (const path of ["nested/package.json", "nested/tsconfig.json", "pnpm-workspace.yaml"]) {
    writeFileSync(join(root, path), path.endsWith("yaml") ? "packages: []\n" : "{}\n");
    git(root, "add", path);
    result = runHook(root, "pre-commit");
    assert.notEqual(result.status, 0, output(result));
    assert.match(output(result), /nested[/\\]bad.ts/);
    git(root, "reset", "--hard", "HEAD");
  }
});

test("Biome excludes generated output and format repair reaches a fixed point", (t) => {
  const root = fixture(t);
  for (const directory of ["dist", "target", "artifacts", ".venv"]) {
    // .venv is a shared read-only link in the fixture, so replace it locally.
    if (directory === ".venv") rmSync(join(root, directory), { recursive: true });
    mkdirSync(join(root, directory), { recursive: true });
    writeFileSync(join(root, directory, "bad.ts"), "not valid TypeScript {{{");
  }
  writeFileSync(join(root, "source.ts"), "export const value=1\n");
  let result = command(root, process.execPath, ["scripts/checks.mjs", "javascript-fix"]);
  assert.equal(result.status, 0, output(result));
  const fixed = readFileSync(join(root, "source.ts"), "utf8");
  result = command(root, process.execPath, ["scripts/checks.mjs", "javascript"]);
  assert.equal(result.status, 0, output(result));
  result = command(root, process.execPath, ["scripts/checks.mjs", "javascript-fix"]);
  assert.equal(result.status, 0, output(result));
  assert.equal(readFileSync(join(root, "source.ts"), "utf8"), fixed);
});

test("pre-push rejects an installed graph with a different lockfile", (t) => {
  const root = webFixture(t);
  const base = git(root, "rev-parse", "HEAD").trim();
  writeFileSync(join(root, "web/presentation/src/value.ts"), "export const value: number = 2;\n");
  const head = commit(root);
  rmSync(join(root, "node_modules"), { recursive: true });
  mkdirSync(join(root, "node_modules/.pnpm"), { recursive: true });
  symlinkSync(
    join(source, "node_modules/@biomejs"),
    join(root, "node_modules/@biomejs"),
    process.platform === "win32" ? "junction" : "dir",
  );
  writeFileSync(join(root, "node_modules/.pnpm/lock.yaml"), "lockfileVersion: '9.0'\n");
  const result = push(root, head, base);
  assert.notEqual(result.status, 0, output(result));
  assert.match(output(result), /Web snapshot dependencies do not match/);
});
