// Snapshot checks never rewrite the contributor's worktree or index.
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { check } from "./checks.mjs";

const git = (...args) => execFileSync("git", args, { maxBuffer: 64 * 1024 * 1024 });
const root = git("rev-parse", "--show-toplevel").toString().trim();
process.chdir(root);
const paths = (bytes) => bytes.toString().split("\0").filter(Boolean);
const markdown = (path) => /\.md$/i.test(path) || [".markdownlint-cli2.jsonc", ".gitignore"].includes(path);
const rust = (path) => /\.rs$/.test(path) || /(^|\/)(Cargo\.(toml|lock)|\.?rustfmt\.toml|\.?clippy\.toml|rust-toolchain(\.toml)?)$/.test(path) || path.startsWith(".cargo/");
function environment() {
  const env = { ...process.env, CARGO_TARGET_DIR: join(root, "target", "hooks") };
  // Cargo/build scripts in the snapshot must not accidentally discover the real Git index.
  for (const name of git("rev-parse", "--local-env-vars").toString().trim().split("\n")) delete env[name];
  return env;
}
function snapshot(entries, isTree = false, omitted = new Set()) {
  const directory = mkdtempSync(join(tmpdir(), "eden-check-index-"));
  try {
    for (const entry of paths(entries)) {
      const tab = entry.indexOf("\t");
      const fields = entry.slice(0, tab).split(" ");
      const [mode, object, stage] = isTree ? [fields[0], fields[2], "0"] : fields;
      const path = entry.slice(tab + 1);
      if (omitted.has(path)) continue;
      if (stage !== "0" || !["100644", "100755"].includes(mode)) throw new Error(`Cannot check unresolved or non-file index entry: ${path}`);
      const destination = join(directory, path);
      mkdirSync(dirname(destination), { recursive: true });
      writeFileSync(destination, git("cat-file", "blob", object), { mode: mode === "100755" ? 0o755 : 0o644 });
    }
    return directory;
  } catch (error) { rmSync(directory, { recursive: true, force: true }); throw error; }
}
function preCommit() {
  const changed = paths(git("diff", "--cached", "--ita-invisible-in-index", "--name-only", "--no-renames", "--diff-filter=ACDMRT", "-z"));
  const kinds = [...(changed.some(markdown) ? ["markdown"] : []), ...(changed.some(rust) ? ["fmt"] : [])];
  if (!kinds.length) return;
  // ls-files includes git add -N placeholders, while the commit tree does not.
  const additions = (visibility) => paths(git("diff", "--cached", visibility, "--no-renames", "--name-only", "--diff-filter=A", "-z"));
  const committed = new Set(additions("--ita-invisible-in-index"));
  const omitted = new Set(additions("--ita-visible-in-index").filter((path) => !committed.has(path)));
  // Re-adding a removed HEAD path with -N is a deletion in the actual commit tree.
  for (const path of paths(git("diff", "--cached", "--ita-invisible-in-index", "--no-renames", "--name-only", "--diff-filter=D", "-z"))) omitted.add(path);
  const directory = snapshot(git("ls-files", "--stage", "-z"), false, omitted);
  try {
    console.log("Checking the staged snapshot (no source or index writes).");
    for (const kind of kinds) check(kind, { root: directory, toolRoot: root, env: environment() });
  } finally { rmSync(directory, { recursive: true, force: true }); }
}
function prePush() {
  const checked = new Set();
  for (const line of readFileSync(0, "utf8").trim().split("\n").filter(Boolean)) {
    const [localRef, localOid, , remoteOid] = line.trim().split(/\s+/);
    if (/^0+$/.test(localOid)) continue; // Deleting a ref does not publish code.
    const commit = git("rev-parse", `${localOid}^{commit}`).toString().trim();
    if (checked.has(commit)) continue;
    const hasRemote = !/^0+$/.test(remoteOid) && spawnSync("git", ["cat-file", "-e", `${remoteOid}^{commit}`], { stdio: "ignore" }).status === 0;
    const changed = hasRemote
      ? paths(git("diff", "--name-only", "--no-renames", "-z", remoteOid, commit))
      : paths(git("ls-tree", "-r", "--name-only", "-z", commit));
    if (!changed.some(rust)) continue;
    const directory = snapshot(git("ls-tree", "-r", "-z", commit), true);
    try {
      console.log(`Clippy for pushed ${localRef} at ${commit} (isolated commit snapshot).`);
      check("clippy", { root: directory, toolRoot: root, env: environment() });
      checked.add(commit);
    } finally { rmSync(directory, { recursive: true, force: true }); }
  }
}
try {
  if (process.argv[2] === "pre-commit") preCommit();
  else if (process.argv[2] === "pre-push") prePush();
  else throw new Error("Expected pre-commit or pre-push");
} catch (error) { console.error(error.message); process.exitCode = 1; }
