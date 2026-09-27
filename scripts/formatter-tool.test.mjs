import assert from "node:assert/strict";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { cpuBudget } from "./cpu-budget.mjs";
import { formatterIdentity, formatterTool, project } from "./formatter-tool.mjs";

test("prepared tools reject changed source, manifests, locks and pins", (t) => {
  const root = mkdtempSync(join(tmpdir(), "eden-formatter-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  for (const path of [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rustfmt-toolchain",
    "rustfmt.toml",
    "crates/eden-fmt",
  ]) {
    mkdirSync(join(root, "crates"), { recursive: true });
    cpSync(join(project, path), join(root, path), { recursive: true });
  }
  assert.throws(() => formatterTool(root, root), /missing or out of date/);
  const identity = formatterIdentity(root);
  const binary = join(
    root,
    "target",
    "formatter",
    identity,
    process.platform === "win32" ? "eden-fmt.exe" : "eden-fmt",
  );
  mkdirSync(join(root, "target", "formatter", identity), { recursive: true });
  writeFileSync(binary, "prepared fixture");
  assert.equal(formatterTool(root, root), binary);
  for (const path of [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rustfmt-toolchain",
    "crates/eden-fmt/Cargo.toml",
    "crates/eden-fmt/src/engine.rs",
  ]) {
    const original = readFileSync(join(root, path));
    writeFileSync(join(root, path), Buffer.concat([original, Buffer.from("\n")]));
    assert.throws(() => formatterTool(root, root), /missing or out of date/, path);
    writeFileSync(join(root, path), original);
  }
});

test("CPU budget respects explicit single-core limits and rejects invalid values", () => {
  assert.equal(cpuBudget({ EDEN_CPU_BUDGET: "1" }), 1);
  assert.equal(cpuBudget({ EDEN_CPU_BUDGET: "4" }), 4);
  assert.ok(cpuBudget({}) >= 1);
  for (const value of ["0", "-1", "1.5", "NaN", ""])
    assert.throws(() => cpuBudget({ EDEN_CPU_BUDGET: value }));
});
