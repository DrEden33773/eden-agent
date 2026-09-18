import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { proof, proveLanded, pullRequestFromSubject, resolveHead } from "./ci-proof.mjs";

function git(cwd, ...args) {
  const result = spawnSync("git", args, { cwd, encoding: "utf8" });
  assert.equal(result.status, 0, `git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
}

function commit(root, message) {
  git(root, "add", "-A");
  git(root, "-c", "commit.gpgsign=false", "commit", "-qm", message);
  return git(root, "rev-parse", "HEAD");
}

test("only a squash subject's trailing pull request number is a reference", () => {
  assert.equal(pullRequestFromSubject("Feature (#12)"), 12);
  assert.equal(pullRequestFromSubject("Feature (#12)\n\nBody (#13)"), 12);
  assert.equal(pullRequestFromSubject("Feature"), null);
  assert.equal(pullRequestFromSubject("Feature (#0)"), null);
  assert.equal(pullRequestFromSubject("Feature (#12) extra"), null);
});

test("a squashed commit proves equal to the merge of its reviewed head", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-proof-"));
  try {
    git(root, "init", "-q", "-b", "main");
    git(root, "config", "user.name", "CI proof fixture");
    git(root, "config", "user.email", "fixture@example.invalid");
    writeFileSync(join(root, "README.md"), "fixture\n");
    const base = commit(root, "base");
    git(root, "switch", "-qc", "topic");
    writeFileSync(join(root, "feature.txt"), "reviewed\n");
    const head = commit(root, "feature");
    git(root, "update-ref", "refs/remotes/pull/7/head", head);
    // The synthesized merge commit GitHub tests, then the squash GitHub lands.
    const merged = git(root, "merge-tree", "--write-tree", base, head);
    const squash = git(root, "commit-tree", merged, "-p", base, "-m", "Feature (#7)");
    git(root, "switch", "-q", "main");
    assert.deepEqual(
      proof({ commit: squash, message: "Feature (#7)", head, previous: base, root }),
      {
        verified: true,
        reason: "tree equals the verified merge",
        base,
        head,
        landed_tree: merged,
        merge_tree: merged,
      },
    );
    // A push that carried more than one commit has no reviewed base to compare
    // against, even when its tree happens to be a merge of the reviewed head.
    assert.match(
      proof({ commit: squash, message: "Feature (#7)", head, previous: "0".repeat(40), root })
        .reason,
      /push started from/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a landed tree that differs from the reviewed merge is not proven", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-proof-"));
  try {
    git(root, "init", "-q", "-b", "main");
    git(root, "config", "user.name", "CI proof fixture");
    git(root, "config", "user.email", "fixture@example.invalid");
    writeFileSync(join(root, "README.md"), "fixture\n");
    const base = commit(root, "base");
    git(root, "switch", "-qc", "topic");
    writeFileSync(join(root, "feature.txt"), "reviewed\n");
    const head = commit(root, "feature");
    git(root, "switch", "-q", "main");
    writeFileSync(join(root, "extra.txt"), "not reviewed\n");
    const changed = commit(root, "unreviewed change (#8)");
    const result = proof({
      commit: changed,
      message: "unreviewed change (#8)",
      head,
      previous: base,
      root,
    });
    assert.equal(result.verified, false);
    assert.equal(result.reason, "tree differs from the verified merge");
    assert.notEqual(result.landed_tree, result.merge_tree);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("subjects without a reference and non-squash commits are not proven", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-proof-"));
  try {
    git(root, "init", "-q", "-b", "main");
    git(root, "config", "user.name", "CI proof fixture");
    git(root, "config", "user.email", "fixture@example.invalid");
    writeFileSync(join(root, "README.md"), "fixture\n");
    const base = commit(root, "base");
    writeFileSync(join(root, "second.txt"), "second\n");
    const second = commit(root, "second");
    git(root, "update-ref", "refs/remotes/pull/9/head", base);
    assert.match(
      proof({ commit: second, message: "no reference", head: base, previous: base, root }).reason,
      /no pull request number/,
    );
    const merge = git(
      root,
      "commit-tree",
      `${second}^{tree}`,
      "-p",
      base,
      "-p",
      second,
      "-m",
      "merge",
    );
    assert.match(
      proof({ commit: merge, message: "merge (#9)", head: base, previous: base, root }).reason,
      /not a squash commit/,
    );
    assert.match(
      proof({ commit: "not-a-commit", message: "merge (#9)", head: base, previous: base, root })
        .reason,
      /no exact main commit/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("an unreachable head is reported on a single output line", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-proof-"));
  try {
    git(root, "init", "-q", "-b", "main");
    git(root, "config", "user.name", "CI proof fixture");
    git(root, "config", "user.email", "fixture@example.invalid");
    writeFileSync(join(root, "README.md"), "fixture\n");
    const base = commit(root, "base");
    git(root, "remote", "add", "origin", join(root, "absent-remote"));
    const result = proveLanded({
      commit: base,
      message: "Feature (#21)",
      previous: base,
      root,
    });
    assert.equal(result.verified, false);
    // The runner rejects a multi-line output value, so the reason must not
    // contain the newlines git puts in its failure text.
    assert.doesNotMatch(result.reason, /\n/);
    assert.match(result.reason, /pull\/21\/head/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the reviewed head is fetched from the pull request ref", () => {
  const root = mkdtempSync(join(tmpdir(), "eden-ci-proof-"));
  const remote = mkdtempSync(join(tmpdir(), "eden-ci-proof-remote-"));
  try {
    git(remote, "init", "-q", "--bare");
    git(root, "init", "-q", "-b", "main");
    git(root, "config", "user.name", "CI proof fixture");
    git(root, "config", "user.email", "fixture@example.invalid");
    writeFileSync(join(root, "README.md"), "fixture\n");
    const base = commit(root, "base");
    git(root, "switch", "-qc", "topic");
    writeFileSync(join(root, "feature.txt"), "reviewed\n");
    const head = commit(root, "feature");
    git(root, "push", "-q", remote, `${head}:refs/pull/11/head`);
    git(root, "switch", "-q", "main");
    git(root, "remote", "add", "origin", remote);
    assert.equal(resolveHead(11, root), head);
    assert.throws(() => resolveHead(12, root), /refs\/pull\/12\/head/);
    const merged = git(root, "merge-tree", "--write-tree", base, head);
    const squash = git(root, "commit-tree", merged, "-p", base, "-m", "Feature (#11)");
    assert.equal(
      proof({
        commit: squash,
        message: "Feature (#11)",
        head: resolveHead(11, root),
        previous: base,
        root,
      }).verified,
      true,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(remote, { recursive: true, force: true });
  }
});
