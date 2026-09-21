// Fixed S0–S3 corpus against Pi's imported runtime modules, not an extracted reimplementation.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const expectedCommit = "d981de1229ef899957bbe968bc8dcda02a21f477";
const product = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const summaryPath = path.resolve(
  product,
  process.env.PI_REFERENCE_OUTPUT || "artifacts/pi-reference-verification.json",
);
const output = path.dirname(summaryPath);
const reference = process.env.PI_REFERENCE_ROOT;
assert(reference, "PI_REFERENCE_ROOT must name the fixed Pi checkout with npm ci dependencies");
const root = path.resolve(reference);
const commit = execFileSync("git", ["-C", root, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
assert.equal(commit, expectedCommit, "Pi reference commit changed");
execFileSync("git", ["-C", root, "diff", "--quiet", "HEAD"]);
const modelData = JSON.parse(
  await readFile(path.join(root, ".pi-model-data-receipt.json"), "utf8"),
);
assert.equal(modelData.package, "@earendil-works/pi-ai@0.85.1");
assert.equal(
  modelData.integrity,
  "sha512-+VgVIJDkDO2efYJKEEqvPTH4zmnIaXdAppGbO+vKFA9qy5PdhFiAenuFAkU+oiCSfOC4dMHDyrjdQeL4ZoC5CQ==",
);
for (const [name, expected] of Object.entries(modelData.files)) {
  assert(/^(?:[a-z0-9-]+|\.manifest)\.json$/.test(name));
  const bytes = await readFile(path.join(root, "packages/ai/src/providers/data", name));
  assert.equal(createHash("sha256").update(bytes).digest("hex"), expected);
}

const scratch = await mkdtemp(path.join(os.tmpdir(), "eden-pi-parity-"));
process.env.PI_CODING_AGENT_DIR = path.join(scratch, "pi-state");
delete process.env.PI_OFFLINE;
await mkdir(output, { recursive: true });
const sourceRequire = createRequire(path.join(root, "package.json"));
const { register } = await import(pathToFileURL(sourceRequire.resolve("tsx/esm/api")).href);
const unregister = register({ tsconfig: path.join(root, "tsconfig.json") });
const imported = (relative) => import(pathToFileURL(path.join(root, relative)).href);
const results = {};
const cases = {};
const text = (result) =>
  result.content
    .filter((item) => item.type === "text")
    .map((item) => item.text)
    .join("\n");
async function check(name, expected, operation) {
  try {
    results[name] = await operation();
    cases[name] = { passed: true, expected };
  } catch (error) {
    const failure = { message: String(error), stack: error.stack };
    results[name] = failure;
    cases[name] = { passed: false, expected, failure: failure.message };
  }
}
try {
  const [
    { createReadTool },
    { createEditTool },
    { createBashTool },
    { createGrepTool },
    templates,
    skills,
    compaction,
    loop,
    streams,
  ] = await Promise.all([
    imported("packages/coding-agent/src/core/tools/read.ts"),
    imported("packages/coding-agent/src/core/tools/edit.ts"),
    imported("packages/coding-agent/src/core/tools/bash.ts"),
    imported("packages/coding-agent/src/core/tools/grep.ts"),
    imported("packages/coding-agent/src/core/prompt-templates.ts"),
    imported("packages/coding-agent/src/core/skills.ts"),
    imported("packages/coding-agent/src/core/compaction/compaction.ts"),
    imported("packages/agent/src/agent-loop.ts"),
    imported("packages/ai/src/utils/event-stream.ts"),
  ]);
  const { ensureTool } = await imported("packages/coding-agent/src/utils/tools-manager.ts");
  const rg = await ensureTool("rg", (status) => {
    results.toolSetup ??= [];
    results.toolSetup.push(status);
  });
  assert(rg, "Pi could not locate or install its ripgrep binary");
  results.ripgrep = execFileSync(rg, ["--version"], { encoding: "utf8" }).trim();
  process.env.PI_OFFLINE = "1";
  const read = createReadTool(scratch, { autoResizeImages: false });
  const edit = createEditTool(scratch);
  const bash = createBashTool(scratch);
  const grep = createGrepTool(scratch);
  await check(
    "image_read",
    "PNG produces an inline image block containing original bytes",
    async () => {
      const png =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jR5kAAAAASUVORK5CYII=";
      await writeFile(path.join(scratch, "pixel.png"), Buffer.from(png, "base64"));
      const result = await read.execute("image", { path: "pixel.png" });
      assert(
        result.content.some(
          (item) => item.type === "image" && item.mimeType === "image/png" && item.data === png,
        ),
      );
      return result;
    },
  );
  await check(
    "read_continuation",
    "limit=1 exposes offset=2; next read returns the remaining lines",
    async () => {
      await writeFile(path.join(scratch, "lines.txt"), "alpha\nbeta\ngamma");
      const first = await read.execute("read-first", { path: "lines.txt", limit: 1 });
      const next = await read.execute("read-next", { path: "lines.txt", offset: 2 });
      assert(text(first).includes("offset=2"));
      assert.equal(text(next), "beta\ngamma");
      return { first, next };
    },
  );
  await check(
    "batch_edit_crlf_bom",
    "invalid batch leaves bytes unchanged; valid disjoint edits preserve BOM and CRLF",
    async () => {
      const file = path.join(scratch, "edit.txt");
      const original = "\ufeffalpha\r\nbeta\r\n";
      await writeFile(file, original);
      let rejected;
      try {
        await edit.execute("invalid", {
          path: "edit.txt",
          edits: [
            { oldText: "alpha", newText: "changed" },
            { oldText: "absent", newText: "wrong" },
          ],
        });
      } catch (error) {
        rejected = String(error);
      }
      assert(rejected, "invalid batch must reject");
      assert.equal(await readFile(file, "utf8"), original);
      const changed = await edit.execute("valid", {
        path: "edit.txt",
        edits: [
          { oldText: "alpha", newText: "ALPHA\nextra" },
          { oldText: "beta", newText: "BETA" },
        ],
      });
      const final = await readFile(file, "utf8");
      assert.equal(final, "\ufeffALPHA\r\nextra\r\nBETA\r\n");
      return { rejected, changed, original, final };
    },
  );
  await check(
    "shell_tail_artifact",
    "tail contains stdout/stderr markers and a complete output file is retrievable",
    async () => {
      const result = await bash.execute("shell", {
        command: "printf '%100000s\\n' x; printf 'STDOUT-TAIL\\n'; printf 'STDERR-MARKER\\n' >&2",
      });
      assert(text(result).includes("STDOUT-TAIL"));
      assert(text(result).includes("STDERR-MARKER"));
      const full = await readFile(result.details.fullOutputPath);
      assert(full.length > 100000);
      assert(
        full.includes(Buffer.from("STDOUT-TAIL")) && full.includes(Buffer.from("STDERR-MARKER")),
      );
      const retained = path.join(output, "pi-reference-shell-output.bin");
      await copyFile(result.details.fullOutputPath, retained);
      await rm(result.details.fullOutputPath);
      return { result, fullBytes: full.length, retained };
    },
  );
  await check(
    "template_windows_paths",
    "bare and quoted Windows arguments preserve backslashes through real template expansion",
    async () => {
      const file = path.join(scratch, "path-template.md");
      await writeFile(file, "---\ndescription: path fixture\n---\nPath=$1");
      const loaded = templates.loadPromptTemplates({
        cwd: scratch,
        agentDir: path.join(scratch, "pi-state"),
        promptPaths: [file],
        includeDefaults: false,
      });
      const bare = templates.expandPromptTemplate("/path-template C:\\repo\\src", loaded);
      const quoted = templates.expandPromptTemplate('/path-template "C:\\repo\\src"', loaded);
      assert.equal(bare.trim(), "Path=C:\\repo\\src");
      assert.equal(quoted.trim(), "Path=C:\\repo\\src");
      return { loaded, bare, quoted };
    },
  );
  await check(
    "skill_bom_ignore_isolation",
    "BOM skill loads; ignored/hidden/dependency entries stay excluded; malformed optional entry has a diagnostic",
    async () => {
      const directory = path.join(scratch, "skills");
      for (const name of ["good", "ignored", ".hidden", "node_modules/dependency", "broken"])
        await mkdir(path.join(directory, name), { recursive: true });
      const body = (name) => `---\nname: ${name}\ndescription: fixture ${name}\n---\nInstructions`;
      await writeFile(path.join(directory, "good/SKILL.md"), `\ufeff${body("good")}`);
      await writeFile(path.join(directory, "ignored/SKILL.md"), body("ignored"));
      await writeFile(path.join(directory, ".hidden/SKILL.md"), body("hidden"));
      await writeFile(path.join(directory, "node_modules/dependency/SKILL.md"), body("dependency"));
      await writeFile(
        path.join(directory, "broken/SKILL.md"),
        "---\ndescription: [unterminated\n---\nBroken",
      );
      await writeFile(path.join(directory, ".gitignore"), "ignored/\n");
      const result = skills.loadSkillsFromDir({ dir: directory, source: "path" });
      assert.deepEqual(
        result.skills.map((skill) => skill.name),
        ["good"],
      );
      assert(
        result.diagnostics.some((diagnostic) => JSON.stringify(diagnostic).includes("broken")),
      );
      return result;
    },
  );
  await check(
    "grep_unicode_include_context_large",
    "Unicode regex, positive include, adjacent context and an explicit >10MiB file tail are searchable",
    async () => {
      const directory = path.join(scratch, "search");
      await mkdir(directory);
      await writeFile(path.join(directory, "keep.txt"), "before\nαβ\n猫\nafter\n");
      await writeFile(path.join(directory, "skip.log"), "αβ\n");
      const unicode = await grep.execute("unicode", {
        path: directory,
        pattern: "\\p{Greek}+|猫",
        glob: "*.txt",
        context: 1,
      });
      assert(text(unicode).includes("αβ") && text(unicode).includes("猫"));
      assert(text(unicode).includes("before") && text(unicode).includes("after"));
      assert(!text(unicode).includes("skip.log"));
      const greek = await grep.execute("greek", {
        path: directory,
        pattern: "\\p{Greek}+",
        glob: "*.txt",
      });
      assert(text(greek).includes("αβ"));
      const largePath = path.join(directory, "large.txt");
      await writeFile(largePath, `${"padding\n".repeat(1_400_000)}FINAL-LARGE-MARKER\n`);
      const large = await grep.execute("large", {
        path: largePath,
        pattern: "FINAL-LARGE-MARKER",
        literal: true,
      });
      assert(text(large).includes("FINAL-LARGE-MARKER"));
      return { unicode, greek, large, largeBytes: (await readFile(largePath)).length };
    },
  );
  await check(
    "queue_order",
    "actual agentLoop sends steering one/two before follow-up one/two",
    async () => {
      const user = (value) => ({ role: "user", content: value, timestamp: 1 });
      const steering = [user("steer-one"), user("steer-two")];
      const follow = [user("follow-one"), user("follow-two")];
      const requests = [];
      const events = [];
      const model = {
        id: "controlled",
        api: "openai-responses",
        provider: "openai",
        name: "controlled",
        reasoning: false,
        input: ["text"],
        contextWindow: 100000,
        maxTokens: 1000,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      };
      const stream = loop.agentLoop(
        [user("initial")],
        { systemPrompt: "fixture", messages: [], tools: [] },
        {
          model,
          convertToLlm: (messages) => messages,
          getSteeringMessages: async () => steering.splice(0, 1),
          getFollowUpMessages: async () => follow.splice(0, 1),
        },
        undefined,
        (_model, context) => {
          requests.push(structuredClone(context.messages));
          const message = {
            role: "assistant",
            content: [{ type: "text", text: "answer" }],
            api: model.api,
            provider: model.provider,
            model: model.id,
            usage: {
              input: 1,
              output: 1,
              cacheRead: 0,
              cacheWrite: 0,
              totalTokens: 2,
              cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
            },
            stopReason: "stop",
            timestamp: 1,
          };
          const response = new streams.AssistantMessageEventStream();
          response.push({ type: "done", reason: "stop", message });
          return response;
        },
      );
      for await (const event of stream) events.push(event);
      await stream.result();
      const labels = ["steer-one", "steer-two", "follow-one", "follow-two"];
      const additions = requests.map((request, index) =>
        labels.filter(
          (label) =>
            JSON.stringify(request).includes(label) &&
            (index === 0 || !JSON.stringify(requests[index - 1]).includes(label)),
        ),
      );
      assert.deepEqual(
        additions,
        labels.map((label) => [label]),
      );
      return { requests, additions, events };
    },
  );
  await check(
    "usage_reserve",
    "90000 tokens exceeds the 100000-window minus 16384 reserve",
    async () => {
      const settings = compaction.DEFAULT_COMPACTION_SETTINGS;
      const actual = compaction.shouldCompact(90000, 100000, settings);
      assert.equal(actual, true);
      return { usage: 90000, window: 100000, settings, actual };
    },
  );
} catch (error) {
  cases.import_or_setup = { passed: false, failure: String(error) };
  results.import_or_setup = { message: String(error), stack: error.stack };
} finally {
  await unregister();
  await rm(scratch, { recursive: true, force: true });
}
const report = {
  corpus: "s0-s3-pi-core-v1",
  commit,
  modelData,
  platform: process.platform,
  arch: process.arch,
  node: process.version,
  evidence:
    "Imported Pi tool, resource and agent runtime modules; controlled model stream; not the full Pi CLI or a real provider",
  cases,
};
await writeFile(
  path.join(output, "pi-reference-raw.json"),
  `${JSON.stringify(results, null, 2)}\n`,
);
await writeFile(summaryPath, `${JSON.stringify(report, null, 2)}\n`);
const failures = Object.entries(cases)
  .filter(([, result]) => !result.passed)
  .map(([name]) => name);
console.log(
  JSON.stringify({
    commit,
    platform: process.platform,
    cases: Object.keys(cases).length,
    failures,
    report: path.relative(product, summaryPath),
  }),
);
if (failures.length) process.exitCode = 1;
