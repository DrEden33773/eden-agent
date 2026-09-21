// rust-analyzer supplies stdin; build/run the same stable-compiled adapter as the CLI.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const result = spawnSync("cargo", ["run", "--quiet", "--locked", "-p", "eden-fmt", "--", "stdin"], {
  cwd: root,
  stdio: "inherit",
});
if (result.error) console.error(result.error.message);
process.exitCode = result.status ?? 1;
