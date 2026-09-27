// Run against the built renderer with agent-browser's isolated real Chromium session.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { promisify } from "node:util";

const execute = promisify(execFile);
const browserSession = `presentation-configuration-${process.pid}`;
const dist = new URL("../dist/", import.meta.url);
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
let sequence = 1;
let activeRun = null;
let binding = { instance: "external", generation: 1, revision: 1, profile: 1 };
let fields = [
  { path: "/count", label: "Count", control: "integer", value: 1 },
  { path: "/items", label: "Items", control: "list", value: ["first", "second"] },
  { path: "/extra", label: "Extra", control: "json", value: { unknown: true } },
  { path: "/secret", label: "Secret", control: "secret", configured: true },
].map((field) => ({
  options: [],
  source: "instance",
  writable: true,
  configured: false,
  ...field,
}));
const actions = [];
const applied = new Map();
let dropApplyResponse = false;
let frozenBinding = null;
const server = createServer(async (request, response) => {
  const url = new URL(request.url, "http://localhost");
  const reply = (result) => {
    response.setHeader("Content-Type", "application/json");
    response.end(JSON.stringify({ ok: true, result }));
  };
  if (url.pathname === "/snapshot") {
    setTimeout(
      () =>
        reply({
          presentation: {
            version: 1,
            session_id: "configuration-fixture",
            sequence,
            activity: [],
            views: [
              {
                owner: "host",
                run_id: 0,
                scope: "management",
                revision: sequence,
                active: true,
                handled_actions: [],
                id: "settings",
                slot: "panel",
                title: "Settings",
                fallback: "Configuration",
                platforms: [],
                nodes: [
                  {
                    kind: "configuration_form",
                    id: "config",
                    binding: frozenBinding ?? binding,
                    fields,
                  },
                ],
              },
            ],
          },
          state: { active_run: activeRun, closed: false },
        }),
      80,
    );
    return;
  }
  if (request.method === "POST") {
    let bytes = "";
    for await (const chunk of request) bytes += chunk;
    const body = JSON.parse(bytes);
    if (url.pathname === "/attach") reply({ attachment: 1 });
    else if (url.pathname === "/action") {
      actions.push(body);
      if (applied.has(body.request_id)) {
        reply(applied.get(body.request_id));
        return;
      }
      if (body.action === "config:apply" || body.action === "config:cancel_apply") {
        for (const edit of body.values.edits)
          if (edit.operation === "set")
            fields = fields.map((field) =>
              field.path === edit.path ? { ...field, value: edit.value } : field,
            );
        binding = { ...binding, revision: binding.revision + 1 };
        sequence++;
        const result = { status: "applied", revision: binding.revision };
        applied.set(body.request_id, result);
        if (dropApplyResponse) {
          dropApplyResponse = false;
          response.writeHead(503).end("Interrupted action response");
          return;
        }
        reply(result);
      } else reply({ errors: [] });
    } else reply({});
    return;
  }
  try {
    const path = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    response.setHeader(
      "Content-Type",
      path.endsWith(".js") ? "text/javascript" : path.endsWith(".css") ? "text/css" : "text/html",
    );
    response.end(await readFile(new URL(path, dist)));
  } catch {
    response.writeHead(404).end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const address = server.address();
const click = (name) => browser("find", "role", "button", "click", "--name", name, "--exact");
try {
  await browser("open", `http://127.0.0.1:${address.port}/?token=fixture`);
  await waitFor('document.body.innerText.includes("Private input pending D2")');
  await browser("find", "label", "Count", "fill", "42");
  sequence++;
  activeRun = 99;
  await waitFor(`document.body.innerText.includes("revision ${sequence}")`);
  assert.equal(await evaluate('document.querySelector(".configuration input").value'), "42");
  await click("validate");
  assert.deepEqual(actions.at(-1).values.edits, [{ operation: "set", path: "/count", value: 42 }]);
  await click("Move item 2 up");
  await click("Add item");
  await browser("find", "label", "Item 3", "fill", "third");
  await click("Delete item 2");
  await click("preview");
  assert.deepEqual(actions.at(-1).values.edits.find((edit) => edit.path === "/items").value, [
    "second",
    "third",
  ]);
  binding = { ...binding, generation: 2 };
  sequence++;
  await waitFor('document.body.innerText.includes("Configuration changed")');
  assert.equal(
    await evaluate(
      'Array.from(document.querySelectorAll("button")).find(button => button.textContent === "apply").disabled',
    ),
    true,
  );
  await click("Discard draft and use current values");
  await browser("find", "label", "Count", "fill", "7");
  await click("apply");
  await waitFor('document.body.innerText.includes("configuration revision 2")');
  await browser("find", "label", "Count", "fill", "8");
  await click("apply");
  await waitFor('document.body.innerText.includes("configuration revision 3")');
  assert.equal(actions.at(-1).values.binding.revision, 2);
  assert.equal(actions.at(-1).values.edits[0].value, 8);
  await browser("find", "label", "Count", "fill", "99");
  await click("Discard draft and use current values");
  assert.equal(await evaluate('document.querySelector(".configuration input").value'), "8");
  await browser("find", "label", "Count", "fill", "9");
  await click("cancel apply");
  await waitFor('document.body.innerText.includes("configuration revision 4")');
  assert.deepEqual(actions.at(-1).values.edits, [{ operation: "set", path: "/count", value: 9 }]);
  fields.push({
    path: "/numbers",
    label: "Numbers",
    control: "list",
    item_kind: "integer",
    value: [],
    options: [],
    source: "instance",
    writable: true,
    configured: false,
  });
  fields.push({
    path: "/objects",
    label: "Objects",
    control: "list",
    item_kind: "object",
    value: [],
    options: [],
    source: "instance",
    writable: true,
    configured: false,
  });
  sequence++;
  await waitFor('document.body.innerText.includes("Numbers")');
  await evaluate(
    'Array.from(document.querySelectorAll("fieldset")).find(field => field.querySelector("legend").textContent.includes("Numbers")).querySelector("button").click()',
  );
  await browser("fill", ".configuration fieldset:nth-of-type(5) input", "3");
  await evaluate(
    'Array.from(document.querySelectorAll("fieldset")).find(field => field.querySelector("legend").textContent.includes("Objects")).querySelector("button").click()',
  );
  await browser("fill", ".configuration fieldset:nth-of-type(6) input", '{"enabled":true}');
  await click("preview");
  assert.deepEqual(actions.at(-1).values.edits.find((edit) => edit.path === "/numbers").value, [3]);
  assert.deepEqual(actions.at(-1).values.edits.find((edit) => edit.path === "/objects").value, [
    { enabled: true },
  ]);
  await click("Discard draft and use current values");
  dropApplyResponse = true;
  await browser("find", "label", "Count", "fill", "10");
  await click("apply");
  await waitFor(
    'document.body.innerText.includes("Retry last request") && document.body.innerText.includes("Configuration changed")',
  );
  const submitted = actions.at(-1);
  await click("Retry last request");
  await waitFor(
    'document.body.innerText.includes("Retry resolved") && !document.body.innerText.includes("Configuration changed")',
  );
  assert.equal(actions.at(-1).request_id, submitted.request_id);
  assert.equal(await evaluate('document.querySelector(".configuration input").value'), "10");
  await browser("find", "label", "Count", "fill", "11");
  await click("apply");
  await waitFor('document.body.innerText.includes("configuration revision 6")');
  frozenBinding = { ...binding };
  dropApplyResponse = true;
  await browser("find", "label", "Count", "fill", "12");
  await click("apply");
  await waitFor('document.body.innerText.includes("Retry last request")');
  await browser("find", "label", "Count", "fill", "13");
  await click("Retry last request");
  await waitFor('document.body.innerText.includes("Retry resolved")');
  frozenBinding = null;
  sequence++;
  await waitFor('document.body.innerText.includes("Configuration changed")');
  assert.ok(await evaluate('document.querySelector(".configuration").innerText.includes("13")'));
  await click("Discard draft and use current values");
  assert.equal(await evaluate('document.querySelector(".configuration input").value'), "12");
  assert.equal(await evaluate('document.querySelectorAll("input[type=password]").length'), 0);
  assert.ok(
    actions.every((action) => action.values.edits.every((edit) => edit.path !== "/secret")),
  );
  console.log(
    "Configuration browser contract passed: typed patches, list edits, run/status retention, conflict, repeated apply, secret presence.",
  );
} finally {
  await browser("close");
  await new Promise((resolve) => server.close(resolve));
}
