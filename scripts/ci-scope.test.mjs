import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { changedPaths, HEAVY_OSES, heavyRequired, qualityPass, scopeFor } from "./ci-scope.mjs";

test("only explicitly recognized prose PRs omit native jobs", () => {
  assert.equal(scopeFor("pull_request", ["README.md", "docs/a/b.md"]).native, false);
  for (const path of [
    "AGENTS.md",
    ".github/workflows/quality.yml",
    "scripts/ci-scope.mjs",
    "docs/sample.rs",
    "plugins/new/src/lib.rs",
    "Cargo.lock",
  ]) {
    assert.equal(scopeFor("pull_request", ["README.md", path]).native, true);
  }
  assert.equal(scopeFor("push", ["README.md"]).native, true);
  assert.equal(scopeFor("pull_request", []).native, true);
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
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Quality rejects failed, cancelled, missing and unexpectedly skipped requirements", () => {
  assert.equal(qualityPass({ lint: "success", required: "true", native: "success" }), true);
  assert.equal(qualityPass({ lint: "success", required: "false", native: "skipped" }), true);
  for (const status of ["failure", "cancelled", "skipped", undefined]) {
    assert.equal(qualityPass({ lint: "success", required: "true", native: status }), false);
    assert.equal(qualityPass({ lint: status, required: "false", native: "skipped" }), false);
  }
  assert.equal(qualityPass({ lint: "success", native: "skipped" }), false);
});

test("a main push accepts the proof only when it replaced a warm heavy run", () => {
  const proven = {
    event: "push",
    proof: "success",
    verified: "true",
    heavy: "false",
    lint: "skipped",
    native: "skipped",
  };
  assert.equal(qualityPass(proven), true);
  // The proof is not evidence for a run that skipped without it, or that ran
  // part of the heavy work anyway.
  assert.equal(qualityPass({ ...proven, proof: "failure" }), false);
  assert.equal(qualityPass({ ...proven, proof: "skipped" }), false);
  assert.equal(qualityPass({ ...proven, verified: "false" }), false);
  assert.equal(qualityPass({ ...proven, lint: "success" }), false);
  assert.equal(qualityPass({ ...proven, native: "success" }), false);
  // A heavy run is its own evidence, including when the proof could not be made.
  for (const proof of ["success", "failure", "cancelled", undefined]) {
    assert.equal(
      qualityPass({ event: "push", proof, heavy: "true", lint: "success", native: "success" }),
      true,
    );
    assert.equal(
      qualityPass({ event: "push", proof, heavy: "true", lint: "success", native: "skipped" }),
      false,
    );
    assert.equal(
      qualityPass({ event: "push", proof, heavy: "true", lint: "skipped", native: "skipped" }),
      false,
    );
  }
  // A missing verdict runs the heavy jobs, so it can never pass on the proof.
  assert.equal(
    qualityPass({
      event: "push",
      proof: "success",
      verified: "true",
      lint: "skipped",
      native: "skipped",
    }),
    false,
  );
});

test("the heavy jobs are required whenever a runner cache is cold or missing", () => {
  const warm = new Map(HEAVY_OSES.map((os) => [os, true]));
  assert.equal(heavyRequired(warm), false);
  assert.equal(heavyRequired(new Map()), true);
  for (const os of HEAVY_OSES) {
    for (const value of [false, undefined]) {
      assert.equal(heavyRequired(new Map(warm).set(os, value)), true);
    }
  }
});
