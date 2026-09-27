// rust-analyzer supplies stdin; tool preparation stays outside the save path.
import { spawnSync } from "node:child_process";
import { formatterTool, project } from "./formatter-tool.mjs";

try {
  const result = spawnSync(formatterTool(), ["stdin"], { cwd: project, stdio: "inherit" });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
