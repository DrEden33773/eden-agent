import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { changedPaths, FAMILIES, qualityPass, scopeFor, staticPass } from "./ci-scope.mjs";

test("associated families union inputs and keep native suites together", () => {
  const selected = (paths, event = "pull_request") =>
    FAMILIES.filter((key) => scopeFor(event, paths)[key]);
  for (const event of ["pull_request", "push"]) {
    assert.deepEqual(selected(["README.md", "docs/a/b.md", "AGENTS.md"], event), ["markdown"]);
    assert.deepEqual(selected(["scripts/install.py"], event), ["python", "native"]);
    assert.deepEqual(selected(["scripts/prepare-pi-reference.mjs"], event), [
      "javascript",
      "native",
    ]);
    assert.deepEqual(selected(["crates/a/src/lib.rs", "README.md"], event), [
      "markdown",
      "rust",
      "native",
    ]);
  }
  for (const path of [
    "Cargo.lock",
    "tests/contract-authors/a/Cargo.toml",
    ".cargo/config.toml",
    "rust-toolchain.toml",
    "rustfmt.toml",
    "clippy.toml",
  ])
    assert.deepEqual(selected([path]), ["rust", "native"]);
  assert.deepEqual(selected([".markdownlint-cli2.jsonc"]), ["markdown", "native"]);
  assert.deepEqual(selected(["biome.json"]), ["javascript", "native"]);
  for (const path of [
    "scripts/ci-scope.mjs",
    "scripts/ci-proof.test.mjs",
    "scripts/test_ci_cache.py",
    "scripts/checks.mjs",
    "scripts/install-hooks.mjs",
    "tests/development-hooks.test.mjs",
    ".githooks/pre-commit",
    ".github/workflows/quality.yml",
    "package.json",
    ".gitignore",
    "THIRD_PARTY_NOTICES.md",
    "LICENSE",
    "new-fixture.json",
  ])
    assert.deepEqual(selected([path]), FAMILIES, path);
  assert.deepEqual(selected(["pnpm-lock.yaml"]), ["markdown", "javascript", "native"]);
  for (const path of ["pyproject.toml", "uv.lock"])
    assert.deepEqual(selected([path]), ["python", "native"]);
  assert.deepEqual(selected([]), FAMILIES);
  assert.deepEqual(selected(["README.md"], "workflow_dispatch"), FAMILIES);
});

test("renaming executable input into docs retains the removed native path", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-scope-"));
  try {
    function git(...args) {
      const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
      assert.equal(result.status, 0, result.stderr);
      return result.stdout.trim();
    }
    git("init", "-q");
    git("config", "user.name", "CI scope fixture");
    git("config", "user.email", "fixture@example.invalid");
    writeFileSync(join(root, "build.rs"), "// fixture\n");
    git("add", ".");
    git("-c", "commit.gpgsign=false", "commit", "-qm", "base");
    const base = git("rev-parse", "HEAD");
    mkdirSync(join(root, "docs"));
    renameSync(join(root, "build.rs"), join(root, "docs", "sample.md"));
    git("add", "-A");
    git("-c", "commit.gpgsign=false", "commit", "-qm", "rename");
    const paths = changedPaths(base, git("rev-parse", "HEAD"), root);
    assert.deepEqual(paths, ["build.rs", "docs/sample.md"]);
    assert.equal(scopeFor("pull_request", paths).native, true);
    assert.throws(() => changedPaths("--invalid", base, root), /exact commit IDs/);
    assert.throws(() => changedPaths("f".repeat(40), base, root), /Unable to inspect/);
    const head = git("rev-parse", "HEAD");
    git("checkout", "--detach", base);
    writeFileSync(join(root, "README.md"), "base advanced\n");
    git("add", ".");
    git("-c", "commit.gpgsign=false", "commit", "-qm", "base advances");
    const advanced = git("rev-parse", "HEAD");
    assert.deepEqual(changedPaths(advanced, head, root), ["build.rs", "docs/sample.md"]);
    assert.deepEqual(changedPaths(advanced, head, root, "push"), [
      "README.md",
      "build.rs",
      "docs/sample.md",
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Quality requires successful scope and every selected result", () => {
  const valid = {
    event: "pull_request",
    scope: "success",
    selected: scopeFor("pull_request", ["README.md"]),
    lint: "success",
    native: "skipped",
  };
  assert.equal(qualityPass(valid), true);
  for (const status of ["failure", "cancelled", "skipped", undefined]) {
    assert.equal(qualityPass({ ...valid, scope: status }), false);
    assert.equal(qualityPass({ ...valid, lint: status }), false);
    assert.equal(
      qualityPass({ ...valid, selected: { ...valid.selected, native: true }, native: status }),
      false,
    );
  }
  assert.equal(qualityPass({ ...valid, selected: { native: false } }), false);
  assert.equal(qualityPass({ ...valid, native: "success" }), false);
});

test("main proof reuses selected coverage independently of cache availability", () => {
  const selected = scopeFor("push", ["README.md"]);
  const proven = {
    event: "push",
    scope: "success",
    selected,
    proof: "success",
    verified: "true",
    lint: "skipped",
    native: "skipped",
  };
  assert.equal(qualityPass(proven), true);
  for (const proof of ["failure", "cancelled", "skipped", undefined]) {
    assert.equal(qualityPass({ ...proven, proof }), false);
    assert.equal(qualityPass({ ...proven, proof, lint: "success" }), true);
  }
  assert.equal(qualityPass({ ...proven, verified: "false" }), false);
  assert.equal(qualityPass({ ...proven, lint: "success" }), false);
  assert.equal(qualityPass({ ...proven, scope: "failure" }), false);
});

test("static gate rejects missing, skipped, failed or cancelled selected steps", () => {
  const selected = scopeFor("pull_request", ["README.md"]);
  const steps = Object.fromEntries(
    ["markdown", "python", "javascript", "formatter", "format", "doc"].map((id) => [
      id,
      { outcome: id === "markdown" ? "success" : "skipped" },
    ]),
  );
  assert.equal(staticPass(selected, steps), true);
  for (const outcome of ["failure", "cancelled", "skipped", undefined])
    assert.equal(staticPass(selected, { ...steps, markdown: { outcome } }), false);
  assert.equal(staticPass(selected, { ...steps, doc: { outcome: "success" } }), false);
  assert.equal(staticPass({}, steps), false);
  const all = scopeFor("workflow_dispatch", []);
  const success = Object.fromEntries(Object.keys(steps).map((id) => [id, { outcome: "success" }]));
  assert.equal(staticPass(all, success), true);
  for (const id of Object.keys(success))
    assert.equal(staticPass(all, { ...success, [id]: { outcome: "skipped" } }), false);
});
