#!/usr/bin/env python3
"""Experimental ACP projection of one explicit Eden host; no Grok execution services."""

import json
import os
import sys
import threading
import urllib.error
import urllib.request
import uuid
from pathlib import Path


def text_content(text):
    return {"type": "text", "text": text}


class Host:
    """Pin the host identity and keep endpoint credentials off the ACP stream."""

    def __init__(self, endpoint):
        self.endpoint = json.loads(Path(endpoint).read_text())

    def call(self, route, body=None):
        if body is not None:
            body = {**body, "session_id": self.endpoint["session_id"]}
        request = urllib.request.Request(
            f"http://{self.endpoint['address']}{route}",
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "X-Eden-Token": self.endpoint["token"],
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            result = json.load(error)
        if not result["ok"]:
            raise RuntimeError(result["error"])
        result = result["result"]
        if route.startswith("/tui/snapshot"):
            if result["state"]["session_id"] != self.endpoint["session_id"]:
                raise RuntimeError("Endpoint now serves another Session")
        return result


class Projection:
    """Translate ordered durable commits and live deltas into Grok's actual tracker."""

    def __init__(self, emit):
        self.emit = emit
        self.head = 0
        self.event = 0
        self.tools = {}
        self.streamed = {}

    def record(self, record, replay=False):
        sequence = record["sequence"]
        if sequence <= self.head:
            return
        self.head = sequence
        value = record["payload"]
        run = record["run_id"]
        kind = value.get("type")
        if kind == "message":
            role = value["role"]
            text = "\n".join(b.get("text", "") for b in value["content"])
            if role == "user" and not replay:
                return  # The pager owns the locally submitted prompt row.
            channel = "user_message_chunk" if role == "user" else "agent_message_chunk"
            prefix = self.streamed.pop((run, channel), "")
            if not replay and prefix and text.startswith(prefix):
                text = text[len(prefix) :]
            if text:
                self.emit({"sessionUpdate": channel, "content": text_content(text)}, replay)
        elif kind == "tool_call":
            call_id = f"{run}:{value['call_id']}"
            arguments = json.loads(value["arguments"])
            name = value["name"].split(".")[-1]
            tool_kind = {
                "read": "read",
                "write": "edit",
                "edit": "edit",
                "bash": "execute",
                "powershell": "execute",
            }.get(name, "other")
            title = arguments.get("command", arguments.get("path", value["name"]))
            raw = {**arguments, "tool_name": name}
            if "path" in arguments:
                raw["file_path"] = arguments["path"]
            self.tools[call_id] = (name, raw)
            self.emit(
                {
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": title,
                    "kind": tool_kind,
                    "status": "in_progress",
                    "rawInput": raw,
                    "content": [],
                },
                replay,
            )
        elif kind == "tool_result":
            call_id = f"{run}:{value['call_id']}"
            result = value["result"]
            name, arguments = self.tools.get(call_id, ("unknown", {}))
            failed = bool(result.get("error")) or result.get("exit_code") not in (None, 0)
            text = result.get("text", "")
            if result.get("error"):
                text += "\n" + str(result["error"])
            for artifact in result.get("artifacts", []):
                text += f"\nFull output: {artifact['path']} ({artifact['bytes']} bytes)"
            content = [{"type": "content", "content": text_content(text)}]
            details = result.get("details") or {}
            diffs = [details.get("diff", details)]
            if "edits" in details:
                diffs = [e.get("diff", {}) for e in details["edits"]]
            for diff in diffs:
                if isinstance(diff.get("before"), str) and isinstance(diff.get("after"), str):
                    content.append(
                        {
                            "type": "diff",
                            "path": arguments.get("path", "unknown"),
                            "oldText": diff["before"],
                            "newText": diff["after"],
                        }
                    )
            raw_output = result
            if name in {"bash", "powershell"}:
                raw_output = {
                    "type": "Bash",
                    "output": list(text.encode()),
                    "exit_code": result.get("exit_code") or (1 if failed else 0),
                    "command": arguments.get("command", ""),
                    "truncated": result.get("truncated", False),
                    "signal": None,
                    "timed_out": False,
                    "description": None,
                    "current_dir": "",
                    "output_file": "",
                    "total_bytes": len(text.encode()),
                }
            self.emit(
                {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": "failed" if failed else "completed",
                    "content": content,
                    "rawOutput": raw_output,
                },
                replay,
            )
        elif record["kind"] == "model_attempt":
            text = value.get("text", "")
            prefix = self.streamed.pop((run, "agent_message_chunk"), "")
            if not replay and prefix and text.startswith(prefix):
                text = text[len(prefix) :]
            if text:
                self.emit(
                    {"sessionUpdate": "agent_message_chunk", "content": text_content(text)}, replay
                )
        elif record["kind"] == "terminal":
            outcome = value.get("outcome", {})
            status = outcome.get("status")
            if status in {"failed", "cancelled"} or value.get("cleanup_errors"):
                self.emit(
                    {
                        "sessionUpdate": "agent_message_chunk",
                        "content": text_content(
                            f"\n\nRun {status}. "
                            + (str(outcome) if status == "failed" else "")
                            + (
                                f" Cleanup: {value['cleanup_errors']}"
                                if value.get("cleanup_errors")
                                else ""
                            )
                        ),
                    },
                    replay,
                )
            self.streamed = {key: value for key, value in self.streamed.items() if key[0] != run}

    def apply(self, snapshot, replay=False):
        history = snapshot.get("history", [])
        records = {r["sequence"]: r for r in history}
        if replay:
            for record in history:
                self.record(record, True)
            self.event = max((e["sequence"] for e in snapshot["events"]), default=0)
            return
        for event in snapshot["events"]:
            if event["sequence"] <= self.event:
                continue
            self.event = event["sequence"]
            kind = event["kind"]
            if kind == "committed":
                record = records.get(event["payload"]["sequence"])
                if record:
                    self.record(record)
            elif kind in {"model_text_delta", "model_reasoning_delta"}:
                channel = (
                    "agent_message_chunk" if kind == "model_text_delta" else "agent_thought_chunk"
                )
                delta = event["payload"].get("delta", "")
                key = (event["run_id"], channel)
                self.streamed[key] = self.streamed.get(key, "") + delta
                self.emit({"sessionUpdate": channel, "content": text_content(delta)}, False)
        for record in history:
            self.record(record)


class Adapter:
    """One lease and one projection; prompt waiting cannot block cancellation input."""

    def __init__(self, host):
        self.host = host
        self.session = f"eden-{host.endpoint['session_id']}"
        self.output_lock = threading.Lock()
        self.state_lock = threading.RLock()
        self.stop = threading.Event()
        self.projection = Projection(self.update)
        self.lease = None
        self.snapshot = None
        self.started = False

    def send(self, value):
        with self.output_lock:
            print(json.dumps({"jsonrpc": "2.0", **value}), flush=True)

    def reply(self, request, value):
        if "id" in request:
            self.send({"id": request["id"], "result": value})

    def update(self, update, replay=False):
        self.send(
            {
                "method": "session/update",
                "params": {
                    "sessionId": self.session,
                    "update": update,
                    "_meta": {"isReplay": replay},
                },
            }
        )

    def model_state(self):
        catalog = self.host.call("/models/list", {})
        current = self.host.call("/models/current", {})
        target = current.get("effective_target") or {}
        available = []
        for entry in catalog.get("models", []):
            model = entry["target"]
            available.append(
                {
                    "modelId": model["provider"] + "/" + model["model"],
                    "name": entry["name"],
                    "description": entry["status"],
                    "_meta": {
                        "totalContextTokens": model["limits"].get("context_window"),
                        "acceptsImages": model["capabilities"].get("images", False),
                    },
                }
            )
        identity = target.get("provider", "unknown") + "/" + target.get("model", "unknown")
        if not available:
            available = [{"modelId": identity, "name": identity}]
        return {"currentModelId": identity, "availableModels": available}

    def poll_once(self):
        with self.state_lock:
            previous = self.snapshot
            assert previous is not None
            route = (
                f"/tui/snapshot?attachment={self.lease}&after={previous['presentation']['sequence']}"
                f"&events_after={self.projection.event}&history_head={self.projection.head}&incremental=1"
            )
        snapshot = self.host.call(route)
        with self.state_lock:
            self.projection.apply(snapshot)
            self.snapshot = snapshot

    def follow(self):
        while not self.stop.is_set():
            try:
                self.poll_once()
            except (OSError, RuntimeError, ValueError) as error:
                self.update(
                    {
                        "sessionUpdate": "agent_message_chunk",
                        "content": text_content(f"\nHost disconnected: {error}\n"),
                    }
                )
                return

    def dispatch(self, request):
        try:
            method = request["method"]
            params = request.get("params", {})
            if method == "initialize":
                self.reply(
                    request,
                    {
                        "protocolVersion": 1,
                        "agentCapabilities": {"loadSession": True},
                        "agentInfo": {"name": "eden", "title": "Eden", "version": "g1-experiment"},
                        "authMethods": [{"id": "eden-host", "name": "Eden host attachment"}],
                        "_meta": {
                            "grokShell": False,
                            "cancelRewind": False,
                            "modelState": self.model_state(),
                            "availableCommands": [
                                {
                                    "name": "eden-status",
                                    "description": "Inspect the actual Eden host state",
                                }
                            ],
                        },
                    },
                )
            elif method == "authenticate":
                self.host.call("/tui/snapshot")
                self.reply(request, {})
            elif method in {"session/new", "session/load"}:
                if self.started:
                    raise RuntimeError(
                        "This experiment attaches one Eden Session; launch another endpoint for a new Session"
                    )
                if method == "session/load" and params["sessionId"] != self.session:
                    raise RuntimeError("Session identity mismatch")
                self.lease = self.host.call("/attach", {"frontend": "tui"})["attachment"]
                with self.state_lock:
                    self.snapshot = self.host.call("/tui/snapshot")
                    self.projection.apply(self.snapshot, replay=True)
                self.started = True
                response: dict[str, object] = {"models": self.model_state()}
                if method == "session/new":
                    response["sessionId"] = self.session
                self.reply(request, response)
                threading.Thread(target=self.follow, daemon=True).start()
            elif method == "session/prompt":
                text = "\n".join(b.get("text", "") for b in params["prompt"])
                if any(b["type"] != "text" for b in params["prompt"]):
                    raise RuntimeError("Image/resource input is not connected in this experiment")
                if text == "/eden-status":
                    current = self.host.call("/tui/snapshot")
                    self.update(
                        {
                            "sessionUpdate": "agent_message_chunk",
                            "content": text_content(json.dumps(current["state"], indent=2)),
                        }
                    )
                    self.reply(request, {"stopReason": "end_turn"})
                    return
                if text.startswith("/"):
                    raise RuntimeError("This Eden command is not connected in the Grok port yet")
                run = self.host.call("/prompt", {"request_id": str(uuid.uuid4()), "text": text})[
                    "run_id"
                ]
                terminal = self.host.call("/terminal", {"run_id": run})
                # A response may settle only after its durable output reached the pager channel.
                with self.state_lock:
                    self.snapshot = self.host.call("/tui/snapshot")
                    self.projection.apply(self.snapshot)
                status = terminal["outcome"]["status"]
                self.reply(
                    request, {"stopReason": "cancelled" if status == "cancelled" else "end_turn"}
                )
            elif method == "session/cancel":
                snapshot = self.host.call("/tui/snapshot")
                run = snapshot["state"].get("active_run")
                if run is not None:
                    self.host.call("/cancel", {"run_id": run})
                self.reply(request, {})
            elif method == "session/set_model":
                provider, model = params["modelId"].split("/", 1)
                run = self.host.call(
                    "/models/select",
                    {
                        "request_id": str(uuid.uuid4()),
                        "selection": {"provider": provider, "model": model},
                    },
                )["run_id"]
                terminal = self.host.call("/terminal", {"run_id": run})
                if terminal["outcome"]["status"] != "completed":
                    raise RuntimeError(terminal["outcome"])
                self.reply(request, {})
            else:
                raise RuntimeError(f"Not connected to Eden: {method}")
        except (OSError, RuntimeError, ValueError, KeyError) as error:
            if "id" in request:
                self.send({"id": request["id"], "error": {"code": -32603, "message": str(error)}})

    def run(self):
        try:
            for line in sys.stdin:
                request = json.loads(line)
                threading.Thread(target=self.dispatch, args=(request,), daemon=True).start()
        finally:
            self.stop.set()
            if self.lease is not None:
                self.host.call("/detach", {"attachment": self.lease})


if __name__ == "__main__":
    Adapter(Host(os.environ["EDEN_GROK_ENDPOINT"])).run()
