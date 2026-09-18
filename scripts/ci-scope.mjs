// A conservative CI decision: only prose-only PRs may omit native execution.
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export function scopeFor(event, paths) {
  const docsOnly =
    paths.length > 0 && paths.every((path) => path === "README.md" || /^docs\/.*\.md$/.test(path));
  return {
    native: event !== "pull_request" || !docsOnly,
    reason:
      event !== "pull_request"
        ? "main or explicit run"
        : docsOnly
          ? "prose-only PR"
          : "native or unknown input changed",
    paths,
  };
}

export function changedPaths(base, head, root = process.cwd()) {
  for (const sha of [base, head]) {
    if (!/^[a-f0-9]{40}$/.test(sha)) throw new Error("CI comparison needs exact commit IDs");
  }
  // Disable rename detection so both the deleted and added path participate.
  const result = spawnSync(
    "git",
    ["diff", "--name-only", "--no-renames", "-z", `${base}...${head}`, "--"],
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

/// The three native runners whose caches a main push has to keep warm.
export const HEAVY_OSES = ["ubuntu-24.04", "macos-14", "windows-2022"];

/// Whether the heavy jobs must run instead of trusting the tree proof.
///
/// A missing, unreadable or cold state for any runner means the caches a main
/// push is the only writer of are not warm, so the full run has to happen.
export function heavyRequired(states) {
  return !HEAVY_OSES.every((os) => states.get(os) === true);
}

export function qualityPass({ event, proof, verified, heavy, lint, native, required }) {
  if (event === "push") {
    // The proof replaces the heavy run only when it succeeded and the caches are
    // warm; every other outcome needs the full verification as its evidence.
    if (heavy === "false")
      return (
        proof === "success" && verified === "true" && lint === "skipped" && native === "skipped"
      );
    return lint === "success" && native === "success";
  }
  return (
    lint === "success" &&
    ((required === "true" && native === "success") ||
      (required === "false" && native === "skipped"))
  );
}

/// Collect the per-runner cache states the cache-state job uploaded.
function cacheStates() {
  const states = new Map();
  if (process.env.CACHE_STATE_RESULT !== "success") return states;
  const directory = process.env.CACHE_STATE_DIR ?? "artifacts/cache-state";
  let entries = [];
  try {
    entries = readdirSync(directory);
  } catch {
    // No downloaded state is a cold state, not a decision failure.
    return states;
  }
  for (const entry of entries) {
    if (!entry.endsWith(".json")) continue;
    try {
      const state = JSON.parse(readFileSync(resolve(directory, entry), "utf8"));
      states.set(state.os, state.warm === true);
    } catch {
      return new Map();
    }
  }
  return states;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === "gate") {
    const result = {
      event: process.env.GITHUB_EVENT_NAME,
      proof: process.env.PROOF_RESULT,
      verified: process.env.PROOF_VERIFIED,
      heavy: process.env.HEAVY_REQUIRED,
      lint: process.env.LINT_RESULT,
      native: process.env.NATIVE_RESULT,
      required: process.env.NATIVE_REQUIRED,
    };
    console.log(JSON.stringify(result));
    if (!qualityPass(result)) process.exitCode = 1;
  } else if (process.argv[2] === "cache-state") {
    const states = cacheStates();
    const heavy = heavyRequired(states);
    const result = { states: Object.fromEntries(states), heavy };
    mkdirSync("artifacts/ci", { recursive: true });
    writeFileSync("artifacts/ci/heavy.json", `${JSON.stringify(result, null, 2)}\n`);
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `heavy=${heavy}\n`);
    console.log(JSON.stringify(result));
  } else {
    const event = process.env.GITHUB_EVENT_NAME;
    const paths =
      event === "pull_request"
        ? changedPaths(process.env.CI_BASE_SHA, process.env.CI_HEAD_SHA)
        : [];
    const scope = scopeFor(event, paths);
    mkdirSync("artifacts/ci", { recursive: true });
    writeFileSync("artifacts/ci/scope.json", `${JSON.stringify(scope, null, 2)}\n`);
    if (process.env.GITHUB_OUTPUT)
      appendFileSync(process.env.GITHUB_OUTPUT, `native=${scope.native}\n`);
    console.log(JSON.stringify(scope));
  }
}
