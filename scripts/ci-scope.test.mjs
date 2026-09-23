import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import {
  changedPaths,
  checksMatrix,
  FAMILIES,
  hooksPass,
  qualityPass,
  scopeFor,
  staticPass,
} from "./ci-scope.mjs";

test("associated inputs select hooks only for tooling and configuration", () => {
  const selected = (paths, event = "pull_request") =>
    FAMILIES.filter((key) => scopeFor(event, paths)[key]);
  for (const event of ["pull_request", "push"]) {
    assert.deepEqual(selected(["README.md", "docs/a/b.md", "AGENTS.md"], event), ["markdown"]);
    assert.deepEqual(selected(["scripts/install.py"], event), ["python", "native"]);
    assert.deepEqual(selected(["scripts/verify-pi-reference.mjs"], event), [
      "javascript",
      "native",
    ]);
    assert.deepEqual(selected(["crates/a/src/lib.rs", "README.md"], event), [
      "markdown",
      "rust",
      "native",
    ]);
    assert.deepEqual(selected(["crates/eden-fmt/src/lib.rs"], event), ["rust", "native", "hooks"]);
  }
  for (const path of [
    "Cargo.lock",
    "tests/contract-authors/a/Cargo.toml",
    ".cargo/config.toml",
    "rust-toolchain.toml",
    "rustfmt.toml",
    "clippy.toml",
  ])
    assert.deepEqual(selected([path]), ["rust", "native", "hooks"]);
  assert.deepEqual(selected([".markdownlint-cli2.jsonc"]), ["markdown", "native", "hooks"]);
  assert.deepEqual(selected(["biome.json"]), ["javascript", "native", "hooks"]);
  for (const path of [
    "scripts/ci-scope.mjs",
    "scripts/ci-proof.test.mjs",
    "scripts/test_ci_cache.py",
    "scripts/checks.mjs",
    "scripts/install-hooks.mjs",
    "scripts/format-editor.mjs",
    "scripts/install-formatter.mjs",
    "tests/development-hooks.test.mjs",
    ".githooks/pre-commit",
    ".github/workflows/quality.yml",
    "package.json",
    "pnpm-workspace.yaml",
    ".gitignore",
    "LICENSE",
    "new-fixture.json",
    "rustfmt-toolchain",
  ])
    assert.deepEqual(selected([path]), FAMILIES, path);
  assert.deepEqual(selected(["pnpm-lock.yaml"]), ["markdown", "javascript", "native", "hooks"]);
  assert.deepEqual(selected(["web/presentation/src/main.tsx"]), ["javascript"]);
  assert.deepEqual(selected(["web/presentation/package.json"]), ["javascript", "native", "hooks"]);
  for (const path of ["pyproject.toml", "uv.lock"])
    assert.deepEqual(selected([path]), ["python", "native", "hooks"]);
  assert.deepEqual(selected(["crates/a/src/lib.rs", "uv.lock"]), [
    "python",
    "rust",
    "native",
    "hooks",
  ]);
  assert.deepEqual(selected([]), FAMILIES);
  assert.deepEqual(selected(["README.md"], "workflow_dispatch"), FAMILIES);
});

test("Hook gate distinguishes unselected steps from failed or missing verification", () => {
  const ids = ["hooks"];
  for (const chosen of [true, false]) {
    const selected = scopeFor("pull_request", [
      chosen ? "scripts/checks.mjs" : "crates/eden-coding/src/lib.rs",
    ]);
    const expected = chosen ? "success" : "skipped";
    const steps = Object.fromEntries(ids.map((id) => [id, { outcome: expected }]));
    assert.equal(hooksPass(selected, steps), true);
    for (const id of ids) {
      for (const outcome of ["success", "skipped", "failure", "cancelled", undefined]) {
        if (outcome !== expected)
          assert.equal(hooksPass(selected, { ...steps, [id]: { outcome } }), false);
      }
      const missing = { ...steps };
      delete missing[id];
      assert.equal(hooksPass(selected, missing), false);
    }
    for (const invalid of [undefined, "true", null])
      assert.equal(hooksPass({ ...selected, hooks: invalid }, steps), false);
    assert.equal(hooksPass({ ...selected, native: false }, steps), false);
  }
  assert.equal(hooksPass(null, {}), false);
});

test("Hook gate CLI persists selection and propagates rejected step outcomes", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-hooks-gate-"));
  try {
    const script = new URL("./ci-scope.mjs", import.meta.url);
    const selected = scopeFor("pull_request", ["crates/eden-coding/src/lib.rs"]);
    const steps = Object.fromEntries(["hooks"].map((id) => [id, { outcome: "skipped" }]));
    for (const chosen of [false, true]) {
      const result = spawnSync(process.execPath, [fileURLToPath(script), "hooks-gate"], {
        cwd: root,
        env: {
          ...process.env,
          CI_SELECTED: JSON.stringify({ ...selected, hooks: chosen }),
          CI_STEPS: JSON.stringify(steps),
        },
      });
      assert.equal(result.status, chosen ? 1 : 0, result.stderr?.toString());
      const receipt = JSON.parse(readFileSync(join(root, "artifacts/ci/hooks-selection.json")));
      assert.equal(receipt.status, chosen ? "failed" : "not_selected");
      assert.equal(receipt.selected, chosen);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("native workflow gates hooks, retires Pi and separates main cache maintenance", () => {
  const quality = readFileSync(
    new URL("../.github/workflows/quality.yml", import.meta.url),
    "utf8",
  );
  const native = readFileSync(
    new URL("../.github/workflows/native-verify.yml", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(native, /pi-reference|pi-gate|pi-corpus/);
  const steps = native.split(/\n {6}- /).slice(1);
  const hook = steps.find((step) => step.includes("id: hooks\n"));
  assert.match(hook, /fromJSON\(inputs.selected\).hooks == true/);
  assert.match(hook, /!inputs.cache-only/);
  assert.match(native, /always\(\) && !inputs.cache-only/);
  assert.match(native, /scripts\/ci-scope.mjs hooks-gate/);
  for (const step of steps.filter((step) => step.includes("actions/cache/save@"))) {
    assert.match(step, /inputs.save-cache/);
    assert.match(step, /github.ref == 'refs\/heads\/main'/);
    assert.match(step, /github.event_name != 'pull_request'/);
  }
  assert.doesNotMatch(quality, /actions\/cache\/save@/);
  const warm = quality.split("  cache-main:\n")[1].split("  quality:")[0];
  assert.match(warm, /needs.prove.outputs.verified == 'true'/);
  assert.match(warm, /cache-only: true/);
  assert.match(warm, /cache-mode: write/);
  assert.doesNotMatch(quality.split("  quality:")[1], /cache-main/);
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
      assert.equal(scopeFor("pull_request", paths).hooks, false);
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

test("matrix selects one static row and exactly the required native platforms", () => {
  const docs = scopeFor("pull_request", ["README.md"]);
  assert.deepEqual(checksMatrix(docs), { include: [{ kind: "static", os: "ubuntu-24.04" }] });
  for (const event of ["pull_request", "push", "workflow_dispatch"]) {
    const selected = scopeFor(event, ["package.json"]);
    assert.deepEqual(checksMatrix(selected), {
      include: [
        { kind: "static", os: "ubuntu-24.04" },
        ...["ubuntu-24.04", "windows-2022", "macos-14"].map((os) => ({ kind: "native", os })),
      ],
    });
  }
  for (const invalid of [null, {}, { ...docs, native: "true" }, { ...docs, hooks: true }])
    assert.throws(() => checksMatrix(invalid), /Invalid selected/);
});

test("Quality rejects failed, cancelled, missing and unexpectedly skipped check groups", () => {
  for (const event of ["pull_request", "push", "workflow_dispatch"]) {
    for (const paths of [["README.md"], ["package.json"]]) {
      const active = event === "pull_request" ? "checksPr" : "checksMain";
      const inactive = event === "pull_request" ? "checksMain" : "checksPr";
      const valid = {
        event,
        scope: "success",
        selected: scopeFor(event, paths),
        [active]: "success",
        [inactive]: "skipped",
      };
      assert.equal(qualityPass(valid), true);
      for (const status of ["failure", "cancelled", "skipped", undefined]) {
        assert.equal(qualityPass({ ...valid, scope: status }), false);
        assert.equal(qualityPass({ ...valid, [active]: status }), false);
      }
      for (const status of ["failure", "cancelled", "success", undefined])
        assert.equal(qualityPass({ ...valid, [inactive]: status }), false);
      for (const selected of [null, {}, { ...valid.selected, native: false, hooks: true }])
        assert.equal(qualityPass({ ...valid, selected }), false);
    }
  }
});

test("only successful push proof permits skipped checks; unavailable proof falls back", () => {
  const proven = {
    event: "push",
    scope: "success",
    selected: scopeFor("push", ["package.json"]),
    proof: "success",
    verified: "true",
    checksPr: "skipped",
    checksMain: "skipped",
  };
  assert.equal(qualityPass(proven), true);
  for (const proof of ["failure", "cancelled", "skipped", undefined]) {
    assert.equal(qualityPass({ ...proven, proof }), false);
    assert.equal(qualityPass({ ...proven, proof, checksMain: "success" }), true);
  }
  assert.equal(qualityPass({ ...proven, verified: "false" }), false);
  assert.equal(qualityPass({ ...proven, verified: "false", checksMain: "success" }), true);
  assert.equal(qualityPass({ ...proven, checksMain: "success" }), false);
  assert.equal(qualityPass({ ...proven, scope: "failure" }), false);
  for (const event of ["pull_request", "workflow_dispatch", undefined])
    assert.equal(qualityPass({ ...proven, event }), false);
});

test("static gate rejects missing, skipped, failed or cancelled selected steps", () => {
  const selected = scopeFor("pull_request", ["README.md"]);
  const steps = Object.fromEntries(
    [
      "markdown",
      "python",
      "javascript",
      "web_types",
      "web_build",
      "formatter",
      "format",
      "doc",
    ].map((id) => [id, { outcome: id === "markdown" ? "success" : "skipped" }]),
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

test("front-end extensions and nested configuration have explicit coverage", () => {
  for (const extension of ["js", "jsx", "mjs", "ts", "tsx", "css", "html"]) {
    const web = scopeFor("pull_request", [`web/presentation/src/sample.${extension}`]);
    assert.equal(web.javascript, true);
    assert.equal(web.native, false);
    assert.equal(web.hooks, false);
    assert.equal(scopeFor("push", [`scripts/sample.${extension}`]).javascript, true);
  }
  for (const path of [
    "web/presentation/tsconfig.json",
    "web/presentation/biome.json",
    "web/presentation/package.json",
  ]) {
    const selection = scopeFor("pull_request", [path]);
    assert.equal(selection.javascript, true);
    assert.equal(selection.native, true);
    assert.equal(selection.hooks, true);
  }
});
