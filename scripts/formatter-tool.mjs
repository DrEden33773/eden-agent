// A content identity prevents editor saves and snapshots from using stale tools.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const project = fileURLToPath(new URL("../", import.meta.url));
const executable = process.platform === "win32" ? "eden-fmt.exe" : "eden-fmt";

export function formatterIdentity(root) {
  const hash = createHash("sha256");
  function add(path) {
    const full = join(root, path);
    if (!existsSync(full)) return;
    hash.update(path).update("\0").update(readFileSync(full)).update("\0");
  }
  function tree(path) {
    if (!existsSync(join(root, path))) return;
    for (const entry of readdirSync(join(root, path), { withFileTypes: true }).sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      const next = `${path}/${entry.name}`;
      if (entry.isDirectory()) tree(next);
      else add(next);
    }
  }
  for (const path of [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rustfmt-toolchain",
    "rustfmt.toml",
    "scripts/formatter-tool.mjs",
  ])
    add(path);
  tree(".cargo");
  tree("crates/eden-fmt");
  return hash.digest("hex");
}

export function formatterTool(root = project, toolRoot = project, prepare = false) {
  const identity = formatterIdentity(root);
  const directory = join(toolRoot, "target", "formatter", identity);
  const binary = join(directory, executable);
  if (existsSync(binary)) return binary;
  if (!prepare)
    throw new Error(
      "eden-fmt is missing or out of date. Run pnpm rust:prepare in the product clone, then save again.",
    );
  const target = join(toolRoot, "target", "formatter-build");
  const result = spawnSync(
    "cargo",
    ["build", "--release", "--locked", "-p", "eden-fmt", "--target-dir", target],
    { cwd: root, stdio: "inherit" },
  );
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error("Could not prepare eden-fmt");
  if (formatterIdentity(root) !== identity)
    throw new Error("Formatter inputs changed during preparation; run pnpm rust:prepare again.");
  const parent = join(toolRoot, "target", "formatter");
  mkdirSync(parent, { recursive: true });
  const temporary = mkdtempSync(join(parent, "prepare-"));
  try {
    copyFileSync(join(target, "release", executable), join(temporary, executable));
    // Publish the complete executable, never a partly copied file to a save.
    try {
      renameSync(temporary, directory);
    } catch (error) {
      if (!existsSync(binary)) throw error;
    }
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
  return binary;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    console.log(formatterTool(project, project, true));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
