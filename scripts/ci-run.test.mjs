import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

test("failed phases retain output and timings and return the original failure", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-run-"));
  try {
    const driver = fileURLToPath(new URL("./ci-run.mjs", import.meta.url));
    const result = spawnSync(
      process.execPath,
      [
        driver,
        "failure",
        process.execPath,
        "-e",
        "console.log('Compiling fixture'); console.error('expected failure'); process.exitCode = 7;",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(result.status, 7);
    const timing = JSON.parse(readFileSync(join(root, "artifacts/ci/failure.json"), "utf8"));
    assert.equal(timing.exit_code, 7);
    assert.equal(timing.cargo_compiling_lines, 1);
    assert.ok(timing.duration_seconds > 0);
    const log = readFileSync(join(root, "artifacts/ci/failure.log"), "utf8");
    assert.match(log, /Compiling fixture/);
    assert.match(log, /expected failure/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
