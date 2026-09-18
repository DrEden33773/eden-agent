// Content-addressed proof that a squashed main commit carries exactly the tree
// that pull-request CI validated.
//
// Pull-request CI runs on GitHub's synthesized merge of the reviewed head into
// its base. This compares the landed commit's tree with a merge of the same head
// into the same base, which is the landed commit's only parent. The comparison
// is meaningful only because branch rules reject a merge whose verified base has
// moved, so the parent really is the base the review was computed against.
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const COMMIT = /^[a-f0-9]{40}$/;

/// Pull request number GitHub appends to a squashed commit subject.
export function pullRequestFromSubject(message) {
  const [subject = ""] = message.trim().split("\n", 1);
  const match = /\(#([1-9][0-9]*)\)\s*$/.exec(subject);
  return match ? Number(match[1]) : null;
}

function git(args, root) {
  const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
  if (result.error) throw result.error;
  return { status: result.status, stdout: result.stdout.trim(), stderr: result.stderr.trim() };
}

function reject(reason, extra = {}) {
  return { verified: false, reason, ...extra };
}

/// Resolve a pull request's reviewed head through the pull request ref.
///
/// The ref outlives the head branch, including the branch deletion that follows
/// the merge.
export function resolveHead(number, root = process.cwd()) {
  const ref = `refs/remotes/pull/${number}/head`;
  const fetched = git(
    ["fetch", "--no-tags", "--no-write-fetch-head", "origin", `+refs/pull/${number}/head:${ref}`],
    root,
  );
  if (fetched.status !== 0) throw new Error(`cannot fetch ${ref}: ${fetched.stderr}`);
  return git(["rev-parse", "--verify", `${ref}^{commit}`], root).stdout;
}

/// Compare a landed commit's tree with the merge of the reviewed head.
///
/// `previous` is the tip the push started from. A squash merge is one commit,
/// so that tip must be the landed commit's only parent; a push that carried
/// more than one commit cannot be proven this way.
export function proof({ commit, message, head, previous, root = process.cwd() }) {
  if (!COMMIT.test(commit ?? "")) return reject("no exact main commit to prove");
  if (pullRequestFromSubject(message ?? "") === null)
    return reject("commit subject carries no pull request number");
  if (!COMMIT.test(head ?? "")) return reject("no reviewed head to compare against");
  const parents = git(["rev-list", "--parents", "-n", "1", commit], root).stdout.split(/\s+/);
  if (parents.length !== 2)
    return reject(`${commit} has ${parents.length - 1} parents, not a squash commit`);
  const [base] = parents.slice(1);
  if (previous !== base)
    return reject(
      `push started from ${previous || "an unknown commit"}, not from the landed parent ${base}`,
      { base, head },
    );
  const landed = git(["rev-parse", `${commit}^{tree}`], root).stdout;
  const merged = git(["merge-tree", "--write-tree", base, head], root);
  const [tree = ""] = merged.stdout.split("\n");
  if (merged.status !== 0 || !COMMIT.test(tree))
    return reject(`reviewed head does not merge cleanly into ${base}`, {
      base,
      head,
      landed_tree: landed,
    });
  return {
    verified: tree === landed,
    reason:
      tree === landed ? "tree equals the verified merge" : "tree differs from the verified merge",
    previous,
    base,
    head,
    landed_tree: landed,
    merge_tree: tree,
  };
}

/// Prove the landed main commit named by the workflow environment.
export function proveLanded({ commit, message, previous, root = process.cwd() }) {
  const number = pullRequestFromSubject(message ?? "");
  if (number === null || !COMMIT.test(commit ?? ""))
    return proof({ commit, message, head: "", previous, root });
  let head;
  try {
    head = resolveHead(number, root);
  } catch (error) {
    return reject(error.message, { pull_request: number });
  }
  return { pull_request: number, ...proof({ commit, message, head, previous, root }) };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const commit = process.env.PROOF_COMMIT ?? "";
  const message = git(["show", "-s", "--format=%B", commit], process.cwd()).stdout;
  const receipt = {
    comparison:
      "landed tree equals the merge of the head currently published at refs/pull/<n>/head",
    not_a_verification: "tree equivalence is not a re-run of the checks that passed on the merge",
    ...proveLanded({ commit, message, previous: process.env.PROOF_PREVIOUS }),
  };
  mkdirSync("artifacts/ci", { recursive: true });
  writeFileSync("artifacts/ci/tree-proof.json", `${JSON.stringify(receipt, null, 2)}\n`);
  if (process.env.GITHUB_OUTPUT) {
    // A failure reason can embed multi-line git output, which the runner rejects
    // as an output value. The receipt keeps the original text.
    const reason = receipt.reason.replaceAll(/\s+/g, " ").trim();
    appendFileSync(process.env.GITHUB_OUTPUT, `verified=${receipt.verified}\nreason=${reason}\n`);
  }
  console.log(JSON.stringify(receipt));
}
