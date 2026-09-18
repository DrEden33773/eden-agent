// Shared check commands for contributors, hooks and CI.
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const project = fileURLToPath(new URL("../", import.meta.url));
function run(program, args, root, env) {
  const result = spawnSync(program, args, { cwd: root, env, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0)
    throw new Error(`${program} ${args.join(" ")} failed (${result.signal ?? result.status})`);
}
export function manifests(root) {
  const roots = existsSync(join(root, "Cargo.toml")) ? ["Cargo.toml"] : [];
  const authors = join(root, "tests", "contract-authors");
  if (existsSync(authors)) {
    for (const entry of readdirSync(authors, { withFileTypes: true }).sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      if (entry.isDirectory() && existsSync(join(authors, entry.name, "Cargo.toml"))) {
        roots.push(`tests/contract-authors/${entry.name}/Cargo.toml`);
      }
    }
  }
  return roots;
}
export function check(
  kind,
  { root = project, toolRoot = project, env = process.env, extra = [] } = {},
) {
  if (kind === "markdown" || kind === "markdown-fix") {
    const cli = join(toolRoot, "node_modules", "markdownlint-cli2", "markdownlint-cli2-bin.mjs");
    if (!existsSync(cli))
      throw new Error(
        "Markdown checks need local dependencies. Run pnpm install --frozen-lockfile.",
      );
    run(
      process.execPath,
      [cli, "**/*.[mM][dD]", ...(kind === "markdown-fix" ? ["--fix"] : [])],
      root,
      env,
    );
    return;
  }
  if (kind.startsWith("python")) {
    const python = join(
      toolRoot,
      ".venv",
      process.platform === "win32" ? "Scripts/python.exe" : "bin/python",
    );
    if (!existsSync(python))
      throw new Error("Python checks need local tools. Run uv sync --locked.");
    run(
      python,
      [
        "-c",
        `from importlib.metadata import version
from pathlib import Path
import tomllib

config = tomllib.loads(Path("pyproject.toml").read_text(encoding="utf-8"))
for dependency in config["dependency-groups"]["dev"]:
    name, expected = dependency.split("==")
    if version(name) != expected:
        raise RuntimeError(f"Tool version mismatch: {name}; run uv sync --locked")
`,
      ],
      root,
      env,
    );
    const steps = {
      "python-format": [["ruff", "format", "."]],
      "python-format-check": [["ruff", "format", "--check", "."]],
      "python-lint": [["ruff", "check", "."]],
      "python-types": [["basedpyright", "--project", "pyproject.toml"]],
      python: [
        ["ruff", "format", "--check", "."],
        ["ruff", "check", "."],
        ["basedpyright", "--project", "pyproject.toml"],
      ],
    };
    if (!steps[kind]) throw new Error(`Unknown check: ${kind}`);
    for (const args of steps[kind])
      run(python, ["-m", ...args], root, {
        ...env,
        PYTHONDONTWRITEBYTECODE: "1",
        RUFF_NO_CACHE: "true",
      });
    return;
  }
  if (kind === "javascript" || kind === "javascript-fix") {
    const cli = join(toolRoot, "node_modules", "@biomejs", "biome", "bin", "biome");
    if (!existsSync(cli))
      throw new Error(
        "JavaScript checks need local dependencies. Run pnpm install --frozen-lockfile.",
      );
    run(
      process.execPath,
      [cli, "check", ...(kind === "javascript-fix" ? ["--write"] : []), "."],
      root,
      env,
    );
    return;
  }
  if (!["fmt", "format", "clippy", "clippy-fix", "test"].includes(kind))
    throw new Error(`Unknown check: ${kind}`);
  const rustEnv = { ...env, CARGO_TARGET_DIR: env.CARGO_TARGET_DIR || join(root, "target") };
  // `cargo test --workspace` covers the root workspace, so the test kind runs
  // only the independent author projects that workspace excludes.
  const selected =
    kind === "test"
      ? manifests(root).filter((manifest) => manifest !== "Cargo.toml")
      : manifests(root);
  const failures = [];
  for (const manifest of selected) {
    console.log(`${kind}: ${manifest}`);
    const args =
      kind === "test"
        ? ["test", "--manifest-path", manifest, "--locked", "--no-fail-fast"]
        : kind === "fmt" || kind === "format"
          ? [
              "fmt",
              "--manifest-path",
              manifest,
              "--all",
              ...(kind === "fmt" ? ["--", "--check"] : []),
            ]
          : [
              "clippy",
              "--manifest-path",
              manifest,
              "--workspace",
              "--all-targets",
              "--locked",
              ...(kind === "clippy-fix" ? ["--fix", ...extra] : []),
              "--",
              "-D",
              "warnings",
            ];
    try {
      run("cargo", args, root, rustEnv);
    } catch (error) {
      // Each author project is its own workspace, so one failure must not hide
      // the results of the projects that would have run after it.
      if (kind !== "test") throw error;
      failures.push(error.message);
    }
  }
  if (failures.length)
    throw new Error(
      `${failures.length} independent author projects failed:\n${failures.join("\n")}`,
    );
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    check(process.argv[2], { extra: process.argv.slice(3) });
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
