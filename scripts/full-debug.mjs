// Keep full debugger artifacts separate from the everyday debug=1 graph.
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { project } from "./formatter-tool.mjs";

const [program = "cargo", ...args] = process.argv.slice(2);
const result = spawnSync(program, args, {
  stdio: "inherit",
  env: {
    ...process.env,
    CARGO_PROFILE_DEV_DEBUG: "2",
    CARGO_PROFILE_TEST_DEBUG: "2",
    CARGO_TARGET_DIR: join(project, "target", "full-debug"),
  },
});
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
