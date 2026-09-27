import { spawnSync } from "node:child_process";
import { cpuBudget } from "./cpu-budget.mjs";
import { formatterTool } from "./formatter-tool.mjs";

const files = [
  "tests/development-hooks.test.mjs",
  "tests/development-hooks-push.test.mjs",
  "tests/development-hooks-web.test.mjs",
];
const budget = cpuBudget();
const workers = Math.min(budget, files.length);
formatterTool(undefined, undefined, true);
console.log(
  `Hook files: CPU budget=${budget}, workers=${workers}, child budget=${Math.max(1, Math.floor(budget / workers))}`,
);
const result = spawnSync(process.execPath, ["--test", `--test-concurrency=${workers}`, ...files], {
  stdio: "inherit",
  env: {
    ...process.env,
    EDEN_CPU_BUDGET: String(Math.max(1, Math.floor(budget / workers))),
    ...(!process.env.CARGO_BUILD_JOBS && !process.env.CARGO_MAKEFLAGS
      ? { CARGO_BUILD_JOBS: String(Math.max(1, Math.floor(budget / workers))) }
      : {}),
  },
});
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
