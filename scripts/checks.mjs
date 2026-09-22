// Shared check commands for contributors, hooks and CI.
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const project = fileURLToPath(new URL("../", import.meta.url));
// `missing_docs` reaches rustdoc through the workspace lint table, so a build
// already fails on an undocumented item. These four catch what a build cannot:
// a link to an item that moved, and doc text rustdoc cannot read. A RUSTDOCFLAGS
// already in the environment is kept and the four lints are added to it, so a
// contributor can change the baseline without losing what this check is for.
// The "doc" check documents the workspace only: a fixture author under tests/
// that never ships documentation is not what docs/development-checks.md covers.
const DOC_LINTS = [
  "-D",
  "rustdoc::broken_intra_doc_links",
  "-D",
  "rustdoc::bare_urls",
  "-D",
  "rustdoc::invalid_html_tags",
  "-D",
  "rustdoc::invalid_rust_codeblocks",
].join(" ");
const docFlags = (env) => [env.RUSTDOCFLAGS?.trim() || "-D warnings", DOC_LINTS].join(" ");
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
// The rustup shim adds 35-40 ms to every cargo invocation. `rustup which`
// resolves the pinned toolchain through rust-toolchain.toml, the way a shim
// invocation would, and the shim stays the fallback for anything it cannot
// resolve. RUSTUP_TOOLCHAIN is deliberately not set: it would override that
// repository override and turn a missing toolchain into a download attempt
// instead of an error.
let toolchain;
function cargoFor(root) {
  if (toolchain) return toolchain;
  const result = spawnSync("rustup", ["which", "cargo"], { cwd: root, encoding: "utf8" });
  const program = result.status === 0 ? result.stdout.trim() : "";
  const directory = program && existsSync(program) ? dirname(program) : "";
  toolchain = { program: directory ? program : "cargo", directory };
  return toolchain;
}

// Without this the located cargo would still resolve rustc, cargo-fmt and
// cargo-clippy through the shims, one process at a time.
function withToolchainPath(env, directory) {
  if (!directory) return env;
  const name = Object.keys(env).find((key) => key.toUpperCase() === "PATH") ?? "PATH";
  return { ...env, [name]: `${directory}${delimiter}${env[name] ?? ""}` };
}

// The version probe depends only on the snapshot's declared versions, so each
// snapshot is checked once per process rather than once per check call.
const verifiedPythonRoots = new Set();

// eden-fmt formats the json! and select! bodies rustfmt cannot reach. It is a
// workspace member, so it is resolved from the tree that owns it rather than
// from the snapshot being checked: a hook snapshot has no build outputs.
function edenFmt(toolRoot, env) {
  const name = process.platform === "win32" ? "eden-fmt.exe" : "eden-fmt";
  const candidates = [
    env.EDEN_FMT,
    env.CARGO_TARGET_DIR ? join(env.CARGO_TARGET_DIR, "debug", name) : "",
    join(toolRoot, "target", "debug", name),
    join(toolRoot, "target", "hooks", "debug", name),
  ].filter(Boolean);
  const found = candidates.find((candidate) => existsSync(candidate));
  if (!found)
    throw new Error("eden-fmt is not built. Run cargo build --locked -p eden-fmt in this clone.");
  return found;
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
    const verified = verifiedPythonRoots.has(String(root));
    verifiedPythonRoots.add(String(root));
    if (!verified)
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
  if (!["fmt", "format", "clippy", "clippy-fix", "doc", "test"].includes(kind))
    throw new Error(`Unknown check: ${kind}`);
  const cargo = cargoFor(toolRoot);
  const rustEnv = withToolchainPath(
    {
      ...env,
      CARGO_TARGET_DIR: env.CARGO_TARGET_DIR || join(root, "target"),
      ...(kind === "doc" ? { RUSTDOCFLAGS: docFlags(env) } : {}),
    },
    cargo.directory,
  );
  if (kind === "doc") {
    console.log(`${kind}: cargo doc --workspace --no-deps --locked`);
    run(cargo.program, ["doc", "--workspace", "--no-deps", "--locked"], root, rustEnv);
    return;
  }
  // `cargo test --workspace` covers the root workspace, so the test kind runs
  // only the independent author projects that workspace excludes.
  const selected =
    kind === "test"
      ? manifests(root).filter((manifest) => manifest !== "Cargo.toml")
      : manifests(root);
  const formatter = kind === "fmt" || kind === "format" ? edenFmt(toolRoot, env) : "";
  if (formatter) {
    const expected = readFileSync(join(root, "rustfmt-toolchain"), "utf8").trim();
    const identity = spawnSync(formatter, ["--version"], { env: rustEnv, encoding: "utf8" });
    if (identity.status !== 0 || identity.stdout.trim() !== `eden-fmt ${expected}`)
      throw new Error(
        "eden-fmt version differs from this snapshot; run cargo build --locked -p eden-fmt.",
      );
  }
  // Each manifest owns its own tree: the root run leaves the independent
  // authors to the runs their own manifests get below.
  const owned = manifests(root).map((other) => dirname(other));
  const nested = (directory) =>
    owned
      .filter((other) => other !== directory)
      .filter((other) => other.startsWith(directory === "." ? "" : `${directory}/`))
      .flatMap((other) => ["--skip", other]);
  const failures = [];
  for (const manifest of selected) {
    if (kind === "test") {
      const metadata = spawnSync(
        cargo.program,
        ["metadata", "--no-deps", "--format-version=1", "--locked", "--manifest-path", manifest],
        { cwd: root, env: rustEnv, encoding: "utf8" },
      );
      if (metadata.error || metadata.status !== 0)
        throw new Error(
          `Cannot select author tests: ${metadata.error?.message ?? metadata.stderr}`,
        );
      const data = JSON.parse(metadata.stdout);
      if (
        !data.packages
          .filter((pkg) => data.workspace_members.includes(pkg.id))
          .some((pkg) => pkg.targets.some((target) => target.test || target.doctest))
      ) {
        console.log(`test: ${manifest} has no enabled test targets`);
        continue;
      }
    }
    console.log(`${kind}: ${manifest}`);
    const args =
      kind === "test"
        ? ["test", "--manifest-path", manifest, "--locked", "--no-fail-fast", ...extra]
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
      if (!formatter) run(cargo.program, args, root, rustEnv);
      if (formatter) {
        const directory = dirname(manifest);
        run(
          formatter,
          [kind === "fmt" ? "check" : "write", directory, ...nested(directory)],
          root,
          rustEnv,
        );
      }
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
