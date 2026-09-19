// Each scenario owns its Git repositories and Cargo target; installed tools are read-only.
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const source = fileURLToPath(new URL("../", import.meta.url));

// A hook scenario runs in a temporary repository with no build outputs, so the
// macro formatter is taken from this checkout; it is built once when missing.
const formatter = join(
  source,
  "target",
  "debug",
  process.platform === "win32" ? "eden-fmt.exe" : "eden-fmt",
);
if (!existsSync(formatter)) {
  const build = spawnSync("cargo", ["build", "--locked", "-p", "eden-fmt"], {
    cwd: source,
    stdio: "inherit",
  });
  assert.equal(build.status, 0, "cargo build --locked -p eden-fmt");
}
process.env.EDEN_FMT = formatter;
export function git(root, ...args) {
  return execFileSync("git", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}
export function command(root, program, args, options = {}) {
  return spawnSync(program, args, { cwd: root, encoding: "utf8", ...options });
}
export function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "eden-hooks-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  git(root, "init", "-b", "main");
  git(root, "config", "user.name", "Hook Test");
  git(root, "config", "user.email", "hooks@example.invalid");
  for (const name of [
    ".githooks",
    "scripts",
    ".markdownlint-cli2.jsonc",
    "pyproject.toml",
    "uv.lock",
    "biome.json",
    "package.json",
    "rustfmt.toml",
    "rust-toolchain.toml",
    ".gitattributes",
  ]) {
    if (existsSync(join(source, name)))
      cpSync(join(source, name), join(root, name), { recursive: true });
  }
  symlinkSync(
    join(source, "node_modules"),
    join(root, "node_modules"),
    process.platform === "win32" ? "junction" : "dir",
  );
  symlinkSync(
    join(source, ".venv"),
    join(root, ".venv"),
    process.platform === "win32" ? "junction" : "dir",
  );
  writeFileSync(join(root, ".gitignore"), "node_modules\n.venv\ntarget/\n");
  mkdirSync(join(root, "src"));
  writeFileSync(
    join(root, "Cargo.toml"),
    '[package]\nname = "hooks-fixture"\nversion = "0.1.0"\nedition = "2024"\n',
  );
  writeFileSync(
    join(root, "Cargo.lock"),
    'version = 4\n\n[[package]]\nname = "hooks-fixture"\nversion = "0.1.0"\n',
  );
  writeFileSync(join(root, "src/lib.rs"), "pub fn value() -> u32 {\n    1\n}\n");
  writeFileSync(join(root, "README.md"), "# Fixture\n\nValid documentation.\n");
  git(root, "add", ".");
  git(root, "-c", "core.hooksPath=", "commit", "-m", "baseline");
  const setup = command(root, process.execPath, ["scripts/install-hooks.mjs"]);
  assert.equal(setup.status, 0, setup.stderr);
  return root;
}
export function unchanged(root, path, staged, worktree) {
  assert.equal(git(root, "show", `:${path}`), staged);
  assert.equal(readFileSync(join(root, path), "utf8"), worktree);
}
