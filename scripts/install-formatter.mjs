// Install the official formatter independently of the product compiler.
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";

const pin = readFileSync(new URL("../rustfmt-toolchain", import.meta.url), "utf8").trim();
const result = spawnSync(
  "rustup",
  [
    "toolchain",
    "install",
    pin,
    "--profile",
    "minimal",
    "--component",
    "rustfmt",
    "--no-self-update",
  ],
  { stdio: "inherit" },
);
if (result.error) console.error(result.error.message);
process.exitCode = result.status ?? 1;
