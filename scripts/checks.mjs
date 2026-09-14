// Shared check commands for contributors, hooks and CI.
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const project = fileURLToPath(new URL("../", import.meta.url));
function run(program, args, root, env) {
  const result = spawnSync(program, args, { cwd: root, env, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${program} ${args.join(" ")} failed (${result.signal ?? result.status})`);
}
export function manifests(root) {
  const roots = existsSync(join(root, "Cargo.toml")) ? ["Cargo.toml"] : [];
  const authors = join(root, "tests", "contract-authors");
  if (existsSync(authors)) {
    for (const entry of readdirSync(authors, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (entry.isDirectory() && existsSync(join(authors, entry.name, "Cargo.toml"))) {
        roots.push(`tests/contract-authors/${entry.name}/Cargo.toml`);
      }
    }
  }
  return roots;
}
export function check(kind, { root = project, toolRoot = project, env = process.env, extra = [] } = {}) {
  if (kind === "markdown" || kind === "markdown-fix") {
    const cli = join(toolRoot, "node_modules", "markdownlint-cli2", "markdownlint-cli2-bin.mjs");
    if (!existsSync(cli)) throw new Error("Markdown checks need local dependencies. Run pnpm install --frozen-lockfile.");
    run(process.execPath, [cli, "**/*.[mM][dD]", ...(kind === "markdown-fix" ? ["--fix"] : [])], root, env);
    return;
  }
  if (!["fmt", "format", "clippy", "clippy-fix"].includes(kind)) throw new Error(`Unknown check: ${kind}`);
  const rustEnv = { ...env, CARGO_TARGET_DIR: env.CARGO_TARGET_DIR || join(root, "target") };
  for (const manifest of manifests(root)) {
    console.log(`${kind}: ${manifest}`);
    const args = kind === "fmt" || kind === "format"
      ? ["fmt", "--manifest-path", manifest, "--all", ...(kind === "fmt" ? ["--", "--check"] : [])]
      : ["clippy", "--manifest-path", manifest, "--workspace", "--all-targets", "--locked", ...(kind === "clippy-fix" ? ["--fix", ...extra] : []), "--", "-D", "warnings"];
    run("cargo", args, root, rustEnv);
  }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { check(process.argv[2], { extra: process.argv.slice(3) }); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
