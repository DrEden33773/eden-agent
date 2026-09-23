// Run against the built renderer with agent-browser's isolated real Chromium session.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { promisify } from "node:util";

const execute = promisify(execFile);
const browserSession = `presentation-recovery-${process.pid}`;
const dist = new URL("../dist/", import.meta.url);
let disconnected = false;
let readOnly = false;
let active = true;
let sequence = 10;
let slot = "panel";
let version = 1;
let unknownNode = false;
let cancellations = 0;
let contributionVisible = true;
let nextAttachment = 0;
const attachments = new Set();
const snapshots = [];
const actions = [];
let executions = 0;
const results = new Map();

async function browser(...args) {
  const { stdout } = await execute(
    "agent-browser",
    ["--session", browserSession, "--json", ...args],
    {
      timeout: 15000,
    },
  );
  const output = JSON.parse(stdout);
  assert.equal(output.success, true, stdout);
  return output.data;
}
async function evaluate(expression) {
  return (await browser("eval", expression)).result;
}
async function waitFor(expression) {
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    if (await evaluate(expression)) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.fail(
    `Browser condition timed out: ${expression}\n${await evaluate("document.body.innerText")}`,
  );
}
function frame() {
  return {
    presentation: {
      version,
      session_id: "explicit-fixture-session",
      sequence,
      activity: [],
      views: contributionVisible
        ? [
            {
              owner: "fixture",
              run_id: 1,
              revision: sequence,
              active: active && !readOnly,
              handled_actions: [],
              id: "review",
              slot,
              title: "Recovery fixture",
              fallback: "Review",
              platforms: [],
              nodes: [
                ...(unknownNode
                  ? [
                      {
                        kind: "future-widget",
                        id: "future",
                        children: [
                          {
                            kind: "text",
                            id: "known-child",
                            text: "Known child survives unknown node",
                          },
                        ],
                      },
                    ]
                  : []),
                {
                  kind: "form",
                  id: "form",
                  action: "submit",
                  fields: [
                    {
                      id: "reason",
                      label: "Reason",
                      kind: "text",
                      required: false,
                      initial: "base",
                      options: [],
                    },
                  ],
                },
              ],
            },
          ]
        : [],
    },
    state: { active_run: active ? 1 : null, closed: false, read_only: readOnly },
  };
}
const server = createServer(async (request, response) => {
  const url = new URL(request.url, "http://localhost");
  const reply = (result) => {
    response.setHeader("Content-Type", "application/json");
    response.end(JSON.stringify({ ok: true, result }));
  };
  if (url.pathname === "/snapshot") {
    snapshots.push({
      after: url.searchParams.get("after"),
      attachment: url.searchParams.get("attachment"),
    });
    if (disconnected) return response.destroy();
    if (!attachments.has(Number(url.searchParams.get("attachment")))) {
      response.setHeader("Content-Type", "application/json");
      response.end(
        JSON.stringify({
          ok: false,
          error: { code: "InvalidInput", message: "unknown attachment" },
        }),
      );
      return;
    }
    setTimeout(() => reply(frame()), 100);
    return;
  }
  if (request.method === "POST") {
    let bytes = "";
    for await (const chunk of request) bytes += chunk;
    const body = JSON.parse(bytes);
    if (url.pathname === "/attach") {
      attachments.add(++nextAttachment);
      reply({ attachment: nextAttachment });
    } else if (url.pathname === "/detach") {
      attachments.delete(body.attachment);
      reply({});
    } else if (url.pathname === "/cancel") {
      assert.equal(body.run_id, 1);
      cancellations++;
      active = false;
      sequence++;
      reply({ cancelled: true });
    } else if (url.pathname === "/action") {
      actions.push(body);
      if (!results.has(body.request_id)) {
        executions++;
        results.set(body.request_id, { accepted: true });
        disconnected = true;
        response.writeHead(503).end("Interrupted action response");
      } else reply(results.get(body.request_id));
    } else reply({});
    return;
  }
  try {
    const path = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    const contents = await readFile(new URL(path, dist));
    response.setHeader(
      "Content-Type",
      path.endsWith(".js") ? "text/javascript" : path.endsWith(".css") ? "text/css" : "text/html",
    );
    response.end(contents);
  } catch {
    response.writeHead(404).end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const address = server.address();
const explicitUrl = `http://127.0.0.1:${address.port}/?token=fixture`;
try {
  await browser("open", explicitUrl);
  await waitFor('document.body.innerText.includes("Connected to the shared session")');
  await browser("find", "label", "Reason", "fill", "edited during recovery");
  await browser("find", "label", "Message", "fill", "composer draft");
  await browser("find", "role", "button", "click", "--name", "Submit", "--exact");
  await waitFor(
    'document.body.innerText.includes("Connection interrupted") && document.body.innerText.includes("Retry last request")',
  );
  assert.equal(
    await evaluate(
      'Array.from(document.querySelectorAll("button")).find(b => b.textContent === "Submit").disabled',
    ),
    true,
  );
  const pendingBefore = await evaluate('sessionStorage.getItem("eden-live-attempts")');
  assert.equal(actions.length, 1);
  assert.equal(actions[0].values.reason, "edited during recovery");
  const oldAttachment = nextAttachment;
  attachments.clear();
  sequence = 11;
  disconnected = false;
  await waitFor(
    'document.body.innerText.includes("Connected to the shared session") && document.body.innerText.includes("revision 11")',
  );
  assert.ok(nextAttachment > oldAttachment, "expired attachment was replaced");
  assert.ok(
    snapshots.some(
      (request) => Number(request.attachment) > oldAttachment && request.after === "10",
    ),
    "reattach retains last observed watermark",
  );
  assert.equal(await evaluate("location.href"), explicitUrl);
  assert.equal(await evaluate('sessionStorage.getItem("eden-live-attempts")'), pendingBefore);
  assert.deepEqual(
    await evaluate('Array.from(document.querySelectorAll("input")).map(input => input.value)'),
    ["edited during recovery", "composer draft"],
  );
  await browser("find", "role", "button", "click", "--name", "Retry last request", "--exact");
  await waitFor('document.body.innerText.includes("Retry resolved")');
  assert.equal(actions.length, 2);
  assert.equal(actions[0].request_id, actions[1].request_id);
  assert.equal(executions, 1);
  active = false;
  sequence = 12;
  await waitFor('document.body.innerText.includes("Run settled; actions unavailable.")');
  assert.equal(
    await evaluate(
      'Array.from(document.querySelectorAll("button")).find(b => b.textContent === "Submit").disabled',
    ),
    true,
  );
  readOnly = true;
  await browser("open", explicitUrl);
  await waitFor('document.body.innerText.includes("Read-only saved history")');
  disconnected = true;
  await waitFor('document.body.innerText.includes("Connection interrupted")');
  disconnected = false;
  await waitFor(
    'document.body.innerText.includes("Read-only saved history") && !document.body.innerText.includes("Connection interrupted")',
  );
  readOnly = false;
  for (const exclusiveSlot of ["header", "footer", "overlay", "composer"]) {
    slot = exclusiveSlot;
    active = true;
    sequence++;
    await waitFor(`document.body.innerText.includes("${exclusiveSlot} · revision ${sequence}")`);
    await browser("find", "label", "Reason", "fill", `draft in ${exclusiveSlot}`);
    const focusedId = await evaluate("document.activeElement.id");
    sequence++;
    await waitFor(`document.body.innerText.includes("revision ${sequence}")`);
    assert.equal(
      await evaluate("document.activeElement.id"),
      focusedId,
      `${exclusiveSlot}: update retains focus`,
    );
    assert.equal(
      await evaluate("document.activeElement.value"),
      `draft in ${exclusiveSlot}`,
      `${exclusiveSlot}: update retains edited value`,
    );
    await browser("find", "role", "button", "click", "--name", "Cancel run", "--exact");
    await waitFor('document.body.innerText.includes("Run settled; actions unavailable.")');
    assert.equal(
      await evaluate('document.querySelector("article input").value'),
      `draft in ${exclusiveSlot}`,
    );
    assert.equal(await evaluate('document.querySelector("article input").disabled'), true);
    assert.equal(
      await evaluate('document.activeElement === document.querySelector("footer input")'),
      true,
      `${exclusiveSlot}: cancellation returns focus to composer`,
    );
    assert.equal(await evaluate('document.querySelector("footer input").value'), "composer draft");
    active = true;
    sequence++;
    await waitFor(`document.body.innerText.includes("revision ${sequence}")`);
    await browser("find", "label", "Reason", "fill", `closing ${exclusiveSlot}`);
    contributionVisible = false;
    sequence++;
    await waitFor('document.querySelector("article") === null');
    assert.equal(
      await evaluate('document.activeElement === document.querySelector("footer input")'),
      true,
      `${exclusiveSlot}: closed contribution returns focus to composer`,
    );
    assert.equal(await evaluate('document.querySelector("footer input").value'), "composer draft");
    contributionVisible = true;
  }
  assert.equal(cancellations, 4);
  unknownNode = true;
  sequence++;
  await waitFor(
    'document.body.innerText.includes("Review — Unsupported presentation node.") && document.body.innerText.includes("Known child survives unknown node")',
  );
  version = 2;
  sequence++;
  await waitFor('document.body.innerText.includes("Review — Unsupported presentation version.")');
  assert.equal(await evaluate('document.querySelector("article button") === null'), true);
  version = 1;
  slot = "future-slot";
  sequence++;
  await waitFor('document.body.innerText.includes("Review — Unsupported presentation slot.")');
  assert.equal(await evaluate('document.querySelector("article button") === null'), true);
  console.log(
    "PASS: real Chromium recovery, expired attachment, retained target/watermark/drafts/attempt ID, settled controls, read-only errors, four exclusive slots focus/draft/cancel/close/composer restoration, unknown node/version/slot fallback",
  );
} finally {
  await browser("close");
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}
