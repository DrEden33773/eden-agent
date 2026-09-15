// A conservative CI decision: only prose-only PRs may omit native execution.
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
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

export function qualityPass({ lint, native, required }) {
  return (
    lint === "success" &&
    ((required === "true" && native === "success") ||
      (required === "false" && native === "skipped"))
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === "gate") {
    const result = {
      lint: process.env.LINT_RESULT,
      native: process.env.NATIVE_RESULT,
      required: process.env.NATIVE_REQUIRED,
    };
    console.log(JSON.stringify(result));
    if (!qualityPass(result)) process.exitCode = 1;
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
