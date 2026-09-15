// Persist each executed phase's duration, command, compiler counts and raw log.
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [label, program, ...args] = process.argv.slice(2);
if (!/^[a-z0-9-]+$/.test(label ?? "") || !program) {
  throw new Error("Usage: node scripts/ci-run.mjs LABEL PROGRAM [ARGS...]");
}
const directory = "artifacts/ci";
mkdirSync(directory, { recursive: true });
const startedAt = new Date().toISOString();
const started = performance.now();
const output = [];
const windowsPnpm = process.platform === "win32" && program === "pnpm";
const child = spawn(
  windowsPnpm ? process.env.ComSpec || "cmd.exe" : program,
  windowsPnpm ? ["/d", "/s", "/c", "pnpm", ...args] : args,
  { stdio: ["inherit", "pipe", "pipe"], env: { ...process.env, CARGO_TERM_COLOR: "never" } },
);
for (const [stream, destination] of [
  [child.stdout, process.stdout],
  [child.stderr, process.stderr],
]) {
  stream.on("data", (chunk) => {
    output.push(chunk);
    destination.write(chunk);
  });
}
let spawnError;
child.on("error", (error) => {
  spawnError = error.message;
});
child.on("close", (code, signal) => {
  const log = Buffer.concat(output).toString("utf8");
  writeFileSync(join(directory, `${label}.log`), log);
  writeFileSync(
    join(directory, `${label}.json`),
    `${JSON.stringify(
      {
        label,
        command: [program, ...args],
        started_at: startedAt,
        duration_seconds: (performance.now() - started) / 1000,
        exit_code: code,
        signal,
        error: spawnError,
        cargo_compiling_lines: (log.match(/^\s*Compiling /gm) ?? []).length,
        cargo_checking_lines: (log.match(/^\s*Checking /gm) ?? []).length,
      },
      null,
      2,
    )}\n`,
  );
  process.exitCode = code ?? 1;
});
