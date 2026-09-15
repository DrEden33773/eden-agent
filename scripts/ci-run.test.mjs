import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
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

test("active phases persist output and a running receipt before completion", async () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-active-"));
  const driver = fileURLToPath(new URL("./ci-run.mjs", import.meta.url));
  const child = spawn(
    process.execPath,
    [
      driver,
      "active",
      process.execPath,
      "-e",
      "console.log('ready'); process.stdin.once('data', () => process.exit(0));",
    ],
    { cwd: root, stdio: ["pipe", "pipe", "pipe"] },
  );
  const exited = once(child, "exit");
  try {
    await once(child.stdout, "data", { signal: AbortSignal.timeout(5000) });
    assert.match(readFileSync(join(root, "artifacts/ci/active.log"), "utf8"), /ready/);
    assert.equal(
      JSON.parse(readFileSync(join(root, "artifacts/ci/active.json"), "utf8")).status,
      "running",
    );
    child.stdin.end("release\n");
    assert.equal((await exited)[0], 0);
    assert.equal(
      JSON.parse(readFileSync(join(root, "artifacts/ci/active.json"), "utf8")).status,
      "passed",
    );
  } finally {
    child.stdin.end("release\n");
    await exited;
    rmSync(root, { recursive: true, force: true });
  }
});

if (process.platform === "win32" || process.env.EDEN_TEST_POWERSHELL === "1") {
  test("Windows workflow steps cannot hide an earlier command failure", () => {
    const root = mkdtempSync(join(tmpdir(), "eden-ci-pwsh-"));
    try {
      const driver = fileURLToPath(new URL("./ci-run.mjs", import.meta.url));
      const workflow = readFileSync(
        new URL("../.github/workflows/quality.yml", import.meta.url),
        "utf8",
      );
      const blocks = [...workflow.matchAll(/^ {8}run: (?:\|\n((?: {10}.*\n|\n)+)|([^\n]+)\n)/gm)]
        .map((match) =>
          (match[1] ?? match[2])
            .split("\n")
            .filter((line) => line.includes("node scripts/ci-run.mjs")),
        )
        .filter((lines) => lines.length > 0);
      assert.ok(blocks.length > 0, "No workflow command steps were checked");
      const quote = (value) => `'${value.replaceAll("'", "''")}'`;
      const distinctGroups = blocks.filter(
        (lines, index) => blocks.findIndex((other) => other.length === lines.length) === index,
      );
      for (const [index, lines] of distinctGroups.entries()) {
        // Preserve the real workflow's command grouping. Replace tools with a
        // controlled first failure and later success, then use the runner's pwsh footer.
        const commands = lines.map(
          (_, number) =>
            `& ${quote(process.execPath)} ${quote(driver)} phase-${index}-${number} ${quote(process.execPath)} -e ${quote(`process.exit(${number === 0 ? 7 : 0})`)}`,
        );
        const body = [
          "$ErrorActionPreference = 'stop'",
          ...commands,
          "if (Test-Path -LiteralPath variable:\\LASTEXITCODE) { exit $LASTEXITCODE }",
        ].join("\n");
        const result = spawnSync(
          "pwsh",
          ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", body],
          { cwd: root, encoding: "utf8" },
        );
        assert.equal(
          result.status,
          7,
          `Workflow step swallowed its first failure: ${lines.join("; ")}\n${result.stderr}`,
        );
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}
