// The pinned source omits generated model JSON. Hydrate that data from the exact npm release.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { gunzipSync } from "node:zlib";

const reference = process.env.PI_REFERENCE_ROOT;
assert(reference, "PI_REFERENCE_ROOT must name the pinned Pi checkout");
const root = path.resolve(reference);
const expectedCommit = "d981de1229ef899957bbe968bc8dcda02a21f477";
assert.equal(
  execFileSync("git", ["-C", root, "rev-parse", "HEAD"], { encoding: "utf8" }).trim(),
  expectedCommit,
);
execFileSync("git", ["-C", root, "diff", "--quiet", "HEAD"]);
const packageName = "@earendil-works/pi-ai@0.85.1";
const integrity =
  "sha512-+VgVIJDkDO2efYJKEEqvPTH4zmnIaXdAppGbO+vKFA9qy5PdhFiAenuFAkU+oiCSfOC4dMHDyrjdQeL4ZoC5CQ==";
const scratch = await mkdtemp(path.join(os.tmpdir(), "eden-pi-data-"));
try {
  const packed =
    process.platform === "win32"
      ? execFileSync(
          process.env.ComSpec || "cmd.exe",
          ["/d", "/s", "/c", "npm pack @earendil-works/pi-ai@0.85.1 --json --ignore-scripts"],
          { cwd: scratch, encoding: "utf8" },
        )
      : execFileSync("npm", ["pack", packageName, "--json", "--ignore-scripts"], {
          cwd: scratch,
          encoding: "utf8",
        });
  const metadata = JSON.parse(packed);
  const entry = Array.isArray(metadata) ? metadata[0] : Object.values(metadata)[0];
  assert.equal(entry.filename, "earendil-works-pi-ai-0.85.1.tgz");
  const archive = await readFile(path.join(scratch, entry.filename));
  assert.equal(`sha512-${createHash("sha512").update(archive).digest("base64")}`, integrity);
  const tar = gunzipSync(archive);
  const destination = path.join(root, "packages/ai/src/providers/data");
  await mkdir(destination, { recursive: true });
  const files = {};
  const prefix = "package/dist/providers/data/";
  // Read only regular JSON entries from the integrity-checked, fixed npm tarball.
  // No archive path is used as a write target until its basename is allowlisted.
  for (let offset = 0; offset + 512 <= tar.length; ) {
    const header = tar.subarray(offset, offset + 512);
    const field = (start, end) =>
      header.subarray(start, end).toString("utf8").replace(/\0.*$/s, "");
    const name = field(0, 100);
    if (!name) break;
    const size = Number.parseInt(field(124, 136).trim(), 8);
    assert(Number.isSafeInteger(size) && size >= 0 && offset + 512 + size <= tar.length);
    const kind = field(156, 157);
    if ((kind === "0" || kind === "") && name.startsWith(prefix)) {
      const filename = name.slice(prefix.length);
      assert(
        /^(?:[a-z0-9-]+|\.manifest)\.json$/.test(filename),
        `unexpected model data path: ${name}`,
      );
      const content = tar.subarray(offset + 512, offset + 512 + size);
      JSON.parse(content.toString("utf8"));
      await writeFile(path.join(destination, filename), content);
      files[filename] = createHash("sha256").update(content).digest("hex");
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  assert.equal(Object.keys(files).length, 40, "exact release model data set changed");
  execFileSync("git", ["-C", root, "diff", "--quiet", "HEAD"]);
  const receipt = {
    commit: expectedCommit,
    source: "exact npm release generated data; no runtime source replacement",
    package: packageName,
    integrity,
    files,
  };
  await writeFile(
    path.join(root, ".pi-model-data-receipt.json"),
    `${JSON.stringify(receipt, null, 2)}\n`,
  );
  console.log(
    JSON.stringify({ package: packageName, integrity, files: Object.keys(files).length }),
  );
} finally {
  await rm(scratch, { recursive: true, force: true });
}
