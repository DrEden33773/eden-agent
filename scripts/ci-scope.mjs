// Select associated checks before installing tools; unknown inputs fail conservative.
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const FAMILIES = ["markdown", "python", "javascript", "rust", "native", "hooks"];

export function scopeFor(event, paths) {
  const selected = new Set();
  const reasons = [];
  const all = (reason) => {
    for (const family of FAMILIES) selected.add(family);
    reasons.push(reason);
  };
  if (!["pull_request", "push"].includes(event) || paths.length === 0)
    all("explicit run or unavailable comparison");
  for (const path of paths) {
    if (
      path.startsWith(".github/") ||
      path.startsWith(".githooks/") ||
      path.startsWith("scripts/ci-") ||
      path.startsWith("scripts/test_ci_") ||
      path.startsWith("tests/development-hooks") ||
      [
        "scripts/checks.mjs",
        "scripts/hooks.mjs",
        "scripts/install-hooks.mjs",
        "scripts/format-editor.mjs",
        "scripts/install-formatter.mjs",
        "package.json",
        "pnpm-workspace.yaml",
        ".gitignore",
      ].includes(path)
    ) {
      all(`shared execution input: ${path}`);
      continue;
    }
    if (
      path.startsWith("crates/eden-fmt/") ||
      /(^|\/)(Cargo\.(toml|lock)|\.?rustfmt\.toml|\.?clippy\.toml|rust-toolchain(\.toml)?)$/.test(
        path,
      ) ||
      path === "rustfmt-toolchain" ||
      path.startsWith(".cargo/") ||
      [
        ".markdownlint-cli2.jsonc",
        "pnpm-lock.yaml",
        "biome.json",
        "biome.jsonc",
        "pyproject.toml",
        "uv.lock",
      ].includes(path)
    )
      selected.add("hooks");
    if (path === "README.md" || path === "AGENTS.md" || /^docs\/.*\.md$/.test(path)) {
      selected.add("markdown");
    } else if (path === ".markdownlint-cli2.jsonc") {
      selected.add("markdown");
      selected.add("native");
    } else if (path === "pnpm-lock.yaml") {
      selected.add("markdown");
      selected.add("javascript");
      selected.add("native");
      selected.add("hooks");
    } else if (["biome.json", "biome.jsonc"].includes(path)) {
      selected.add("javascript");
      selected.add("native");
    } else if (/\.py$/.test(path) || ["pyproject.toml", "uv.lock"].includes(path)) {
      selected.add("python");
      selected.add("native");
    } else if (path.startsWith("web/presentation/")) {
      selected.add("javascript");
    } else if (/\.mjs$/.test(path)) {
      selected.add("javascript");
      selected.add("native");
    } else if (
      /\.rs$/.test(path) ||
      /(^|\/)(Cargo\.(toml|lock)|\.?rustfmt\.toml|\.?clippy\.toml|rust-toolchain(\.toml)?)$/.test(
        path,
      ) ||
      path.startsWith(".cargo/")
    ) {
      selected.add("rust");
      selected.add("native");
    } else {
      all(`unknown or distribution input: ${path}`);
      continue;
    }
    reasons.push(`associated input: ${path}`);
  }
  return {
    ...Object.fromEntries(FAMILIES.map((family) => [family, selected.has(family)])),
    reasons,
    paths,
  };
}

export function changedPaths(base, head, root = process.cwd(), event = "pull_request") {
  for (const sha of [base, head]) {
    if (!/^[a-f0-9]{40}$/.test(sha)) throw new Error("CI comparison needs exact commit IDs");
  }
  // Disable rename detection so both the deleted and added path participate.
  const result = spawnSync(
    "git",
    [
      "diff",
      "--name-only",
      "--no-renames",
      "-z",
      event === "pull_request" ? `${base}...${head}` : `${base}..${head}`,
      "--",
    ],
    {
      cwd: root,
      encoding: "utf8",
    },
  );
  if (result.error || result.status !== 0) {
    throw new Error(`Unable to inspect CI changes: ${result.error?.message ?? result.stderr}`);
  }
  return result.stdout.split("\0").filter(Boolean);
}

// Reuse inherits the PR's selected coverage. Cache warmth is not test evidence.
export function qualityPass({ event, scope, proof, verified, lint, native, selected }) {
  if (scope !== "success" || !FAMILIES.every((key) => typeof selected?.[key] === "boolean"))
    return false;
  if (selected.hooks && !selected.native) return false;
  if (event === "push" && proof === "success" && verified === "true")
    return lint === "skipped" && native === "skipped";
  return lint === "success" && native === (selected.native ? "success" : "skipped");
}

// Hooks verify contributor tooling only when its inputs change.
export function hooksPass(selected, steps) {
  return (
    selected?.native === true &&
    typeof selected.hooks === "boolean" &&
    steps.hooks?.outcome === (selected.hooks ? "success" : "skipped")
  );
}

export function staticPass(selected, steps) {
  const checks = {
    markdown: ["markdown"],
    python: ["python"],
    javascript: ["javascript"],
    rust: ["formatter", "format", "doc"],
  };
  return Object.entries(checks).every(
    ([family, ids]) =>
      typeof selected?.[family] === "boolean" &&
      ids.every((id) => steps[id]?.outcome === (selected[family] ? "success" : "skipped")),
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === "gate") {
    const result = {
      event: process.env.GITHUB_EVENT_NAME,
      scope: process.env.SCOPE_RESULT,
      proof: process.env.PROOF_RESULT,
      verified: process.env.PROOF_VERIFIED,
      lint: process.env.LINT_RESULT,
      native: process.env.NATIVE_RESULT,
      selected: JSON.parse(process.env.CI_SELECTED || "null"),
    };
    console.log(JSON.stringify(result));
    if (!qualityPass(result)) process.exitCode = 1;
  } else if (process.argv[2] === "hooks-gate") {
    const selected = JSON.parse(process.env.CI_SELECTED || "null");
    const steps = JSON.parse(process.env.CI_STEPS || "{}");
    const passed = hooksPass(selected, steps);
    const receipt = {
      selected: selected?.hooks,
      status: !passed ? "failed" : selected.hooks ? "passed" : "not_selected",
      outcome: steps.hooks?.outcome ?? "missing",
    };
    mkdirSync("artifacts/ci", { recursive: true });
    writeFileSync("artifacts/ci/hooks-selection.json", `${JSON.stringify(receipt, null, 2)}\n`);
    console.log(JSON.stringify(receipt));
    if (!passed) process.exitCode = 1;
  } else if (process.argv[2] === "static-gate") {
    if (
      !staticPass(
        JSON.parse(process.env.CI_SELECTED || "null"),
        JSON.parse(process.env.CI_STEPS || "{}"),
      )
    )
      process.exitCode = 1;
  } else {
    const event = process.env.GITHUB_EVENT_NAME;
    const paths = ["pull_request", "push"].includes(event)
      ? changedPaths(process.env.CI_BASE_SHA, process.env.CI_HEAD_SHA, process.cwd(), event)
      : [];
    const scope = {
      ...scopeFor(event, paths),
      event,
      base: process.env.CI_BASE_SHA,
      head: process.env.CI_HEAD_SHA,
    };
    mkdirSync("artifacts/ci", { recursive: true });
    writeFileSync("artifacts/ci/scope.json", `${JSON.stringify(scope, null, 2)}\n`);
    if (process.env.GITHUB_OUTPUT) {
      for (const key of FAMILIES)
        appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${scope[key]}\n`);
      appendFileSync(process.env.GITHUB_OUTPUT, `selected=${JSON.stringify(scope)}\n`);
    }
    console.log(JSON.stringify(scope));
  }
}
