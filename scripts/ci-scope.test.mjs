import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { changedPaths, FAMILIES, piPass, qualityPass, scopeFor, staticPass } from "./ci-scope.mjs";

test("associated families union inputs and keep native suites together", () => {
  const selected = (paths, event = "pull_request") =>
    FAMILIES.filter((key) => scopeFor(event, paths)[key]);
  for (const event of ["pull_request", "push"]) {
    assert.deepEqual(selected(["README.md", "docs/a/b.md", "AGENTS.md"], event), ["markdown"]);
    assert.deepEqual(selected(["scripts/install.py"], event), ["python", "native"]);
    assert.deepEqual(selected(["scripts/prepare-pi-reference.mjs"], event), [
      "javascript",
      "native",
      "pi_reference",
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
  assert.deepEqual(selected(["pnpm-lock.yaml"]), [
    "markdown",
    "javascript",
    "native",
    "pi_reference",
  ]);
  for (const path of ["pyproject.toml", "uv.lock"])
    assert.deepEqual(selected([path]), ["python", "native"]);
  assert.deepEqual(selected([]), FAMILIES);
  assert.deepEqual(selected(["README.md"], "workflow_dispatch"), FAMILIES);
});

test("Pi refresh follows reference inputs while Eden keeps its native acceptance", () => {
  for (const event of ["pull_request", "push"]) {
    for (const path of [
      "crates/eden-coding/src/lib.rs",
      "scripts/verify-coding.py",
      "scripts/verify-archive.py",
      "scripts/http_fixture.py",
      "Cargo.lock",
    ]) {
      const selected = scopeFor(event, [path]);
      assert.equal(selected.native, true, path);
      assert.equal(selected.pi_reference, false, path);
    }
    for (const path of [
      "scripts/prepare-pi-reference.mjs",
      "scripts/verify-pi-reference.mjs",
      "pnpm-lock.yaml",
      "package.json",
      ".github/workflows/native-verify.yml",
      "scripts/ci-scope.mjs",
      "new-reference-data.json",
    ]) {
      const selected = scopeFor(event, ["crates/eden-coding/src/lib.rs", path]);
      assert.equal(selected.pi_reference, true, path);
      assert.equal(selected.native, true, path);
    }
  }
});

test("Pi gate distinguishes unselected steps from failed or missing verification", () => {
  const ids = ["pi-checkout", "pi-deps", "pi-prepare", "pi-corpus"];
  for (const chosen of [true, false]) {
    const selected = scopeFor("pull_request", [
      chosen ? "scripts/verify-pi-reference.mjs" : "crates/eden-coding/src/lib.rs",
    ]);
    const expected = chosen ? "success" : "skipped";
    const steps = Object.fromEntries(ids.map((id) => [id, { outcome: expected }]));
    assert.equal(piPass(selected, steps), true);
    for (const id of ids) {
      for (const outcome of ["success", "skipped", "failure", "cancelled", undefined]) {
        if (outcome !== expected)
          assert.equal(piPass(selected, { ...steps, [id]: { outcome } }), false);
      }
      const missing = { ...steps };
      delete missing[id];
      assert.equal(piPass(selected, missing), false);
    }
    for (const invalid of [undefined, "true", null])
      assert.equal(piPass({ ...selected, pi_reference: invalid }, steps), false);
    assert.equal(piPass({ ...selected, native: false }, steps), false);
  }
  assert.equal(piPass(null, {}), false);
});

test("Pi gate CLI persists selection and propagates rejected step outcomes", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-pi-gate-"));
  try {
    const script = new URL("./ci-scope.mjs", import.meta.url);
    const selected = scopeFor("pull_request", ["crates/eden-coding/src/lib.rs"]);
    const steps = Object.fromEntries(
      ["pi-checkout", "pi-deps", "pi-prepare", "pi-corpus"].map((id) => [
        id,
        { outcome: "skipped" },
      ]),
    );
    for (const chosen of [false, true]) {
      const result = spawnSync(process.execPath, [fileURLToPath(script), "pi-gate"], {
        cwd: root,
        env: {
          ...process.env,
          CI_SELECTED: JSON.stringify({ ...selected, pi_reference: chosen }),
          CI_STEPS: JSON.stringify(steps),
        },
      });
      assert.equal(result.status, chosen ? 1 : 0, result.stderr?.toString());
      const receipt = JSON.parse(readFileSync(join(root, "artifacts/ci/pi-selection.json")));
      assert.equal(receipt.status, chosen ? "failed" : "not_selected");
      assert.equal(receipt.selected.pi_reference, chosen);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("native workflow forwards selection and gates all Pi preparation and execution", () => {
  const quality = readFileSync(
    new URL("../.github/workflows/quality.yml", import.meta.url),
    "utf8",
  );
  const native = readFileSync(
    new URL("../.github/workflows/native-verify.yml", import.meta.url),
    "utf8",
  );
  for (const job of ["native-pr", "native-main"]) {
    const body = quality.split(`  ${job}:\n`)[1].split(/\n {2}[\w-]+:/)[0];
    assert.match(body, /selected: \$\{\{ needs\.scope\.outputs\.selected \}\}/);
  }
  const steps = native.split(/\n {6}- /).slice(1);
  for (const id of ["pi-checkout", "pi-deps", "pi-prepare", "pi-corpus"]) {
    const matches = steps.filter((step) => step.includes(`id: ${id}\n`));
    assert.equal(matches.length, 1, id);
    assert.match(matches[0], /if: \$\{\{ fromJSON\(inputs\.selected\)\.pi_reference == true \}\}/);
  }
  const gate = steps.find((step) => step.includes("node scripts/ci-scope.mjs pi-gate"));
  assert(gate);
  assert.match(gate, /if: always\(\)/);
  assert.match(gate, /CI_SELECTED: \$\{\{ inputs\.selected \}\}/);
  assert.match(gate, /CI_STEPS: \$\{\{ toJson\(steps\) \}\}/);
});

for (const source of ["build.rs", "scripts/verify-pi-reference.mjs"]) {
  test(`renaming ${source} into docs retains its required checks`, () => {
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
      mkdirSync(join(root, "scripts"));
      writeFileSync(join(root, source), "// fixture\n");
      git("add", ".");
      git("-c", "commit.gpgsign=false", "commit", "-qm", "base");
      const base = git("rev-parse", "HEAD");
      mkdirSync(join(root, "docs"));
      renameSync(join(root, source), join(root, "docs", "sample.md"));
      git("add", "-A");
      git("-c", "commit.gpgsign=false", "commit", "-qm", "rename");
      const paths = changedPaths(base, git("rev-parse", "HEAD"), root);
      assert.deepEqual(paths, [source, "docs/sample.md"].sort());
      assert.equal(scopeFor("pull_request", paths).native, true);
      assert.equal(scopeFor("pull_request", paths).pi_reference, source.endsWith(".mjs"));
      assert.throws(() => changedPaths("--invalid", base, root), /exact commit IDs/);
      assert.throws(() => changedPaths("f".repeat(40), base, root), /Unable to inspect/);
      const head = git("rev-parse", "HEAD");
      git("checkout", "--detach", base);
      writeFileSync(join(root, "README.md"), "base advanced\n");
      git("add", ".");
      git("-c", "commit.gpgsign=false", "commit", "-qm", "base advances");
      const advanced = git("rev-parse", "HEAD");
      assert.deepEqual(changedPaths(advanced, head, root), [source, "docs/sample.md"].sort());
      assert.deepEqual(
        changedPaths(advanced, head, root, "push"),
        ["README.md", source, "docs/sample.md"].sort(),
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}

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
  assert.equal(
    qualityPass({ ...valid, selected: { ...valid.selected, pi_reference: true } }),
    false,
  );
  for (const pi_reference of [undefined, null, "false"])
    assert.equal(qualityPass({ ...valid, selected: { ...valid.selected, pi_reference } }), false);
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
  for (const pi_reference of [true, false]) {
    const coverage = { ...selected, native: true, pi_reference };
    assert.equal(qualityPass({ ...proven, selected: coverage }), true);
    assert.equal(qualityPass({ ...proven, selected: coverage, verified: "false" }), false);
  }
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
