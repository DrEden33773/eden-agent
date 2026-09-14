import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
execFileSync("git", ["-C", root, "config", "--local", "core.hooksPath", ".githooks"], { stdio: "inherit" });
console.log("Installed this repository's pre-commit and pre-push hooks.");
