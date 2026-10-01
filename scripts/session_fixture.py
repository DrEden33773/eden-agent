"""Isolated native hosts, provider and real pager for installed Session acceptance."""

import http.server
import importlib.util
import json
import os
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "session_streaming", ROOT / "scripts/verify-tui-streaming.py"
)
assert SPEC and SPEC.loader
streaming = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(streaming)


class Terminal(streaming.Terminal):
    """Answer the terminal version query before native picker keys are sent."""

    def paint(self, data):
        if "\x1b[>0q" in self.pending + data and not getattr(self, "xt_answered", False):
            os.write(self.master, b"\x1bP>|EdenProbe 1.0\x1b\\")
            self.xt_answered = True
        super().paint(data)


probe = SimpleNamespace(Terminal=Terminal, streaming=streaming)


class BusinessSink(http.server.ThreadingHTTPServer):
    """Record unintended direct Grok HTTP calls without reaching an external service."""

    def __init__(self):
        calls = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format, *args):
                pass

            def do_GET(self):
                calls.append((self.command, self.path))
                self.send_error(503, "Business services are outside the native frontend")

            do_POST = do_GET

        super().__init__(("127.0.0.1", 0), Handler)
        self.calls = calls


class Fixture:
    """Own only processes and history files created in this temporary project."""

    def __init__(self, installation, output):
        self.installation = installation.resolve()
        self.output = output
        self.temporary = tempfile.TemporaryDirectory(prefix="eden-session-recovery-")
        self.root = Path(self.temporary.name)
        self.state = self.root / "host-state"
        self.business = BusinessSink()
        self.business_thread = threading.Thread(target=self.business.serve_forever, daemon=True)
        self.business_thread.start()
        self.provider = streaming.Provider()
        self.provider.mode = "short"
        self.provider.allow.set()
        self.thread = threading.Thread(target=self.provider.serve_forever, daemon=True)
        self.thread.start()
        self.terminal = None
        self.children = []
        self.host = None

    def __enter__(self):
        try:
            config = json.loads((self.installation / "composition.json").read_text())
            for package in config["packages"]:
                package["library"] = str((self.installation / package["library"]).resolve())
                if package["descriptor"]["package"] == "model-access":
                    package["config"] = {
                        "catalog": {
                            "offline": True,
                            "models": [
                                {
                                    "provider": "fixture",
                                    "model": "workflow",
                                    "api": "openai-completions",
                                    "base_url": f"http://127.0.0.1:{self.provider.server_port}/v1",
                                    "limits": {"context_window": 65536, "max_output_tokens": 4096},
                                    "capabilities": {
                                        "tools": True,
                                        "images": False,
                                        "reasoning": True,
                                    },
                                    "source": {
                                        "kind": "author",
                                        "location": "Session recovery fixture",
                                    },
                                }
                            ],
                        },
                        "credentials": {"path": str(self.root / "global/credentials.json")},
                    }
            self.composition = self.root / "composition.json"
            self.composition.write_text(json.dumps(config))
            self.endpoint_file = self.root / "endpoint.json"
            self.history = self.root / "preparation.jsonl"
            self.host = subprocess.Popen(
                [
                    *self.arguments(),
                    "--session",
                    str(self.history),
                    "live",
                    "--endpoint",
                    str(self.endpoint_file),
                ],
                cwd=self.root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={**os.environ, "EDEN_TUI_STATE_DIR": str(self.state)},
            )
            self.endpoint = streaming.live.ready(self.host, self.endpoint_file)
            challenge = self.mutate(
                "/auth/start", {"request": {"action": "start", "provider": "fixture"}}
            )
            reply = self.call(
                "/auth/input",
                {
                    "operation_id": challenge["operation_id"],
                    "api_key": True,
                    "input": "isolated-fixture-key",
                },
            )
            self.call("/terminal", reply)
            self.mutate(
                "/models/catalog",
                {
                    "request": {
                        "action": "set_default",
                        "selection": {
                            "provider": "fixture",
                            "model": "workflow",
                            "thinking": "off",
                        },
                    }
                },
            )
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def arguments(self, composition=None):
        return [
            str(self.installation / "bin/eden"),
            "--composition",
            str(composition or self.composition),
            "--global-dir",
            str(self.root / "global"),
            "--no-trust-project",
            "--offline-startup",
            "--cwd",
            str(self.root),
        ]

    def call(self, route, body=None):
        return streaming.live.call(self.endpoint, route, body)

    def mutate(self, route, body):
        reply = self.call(route, {**body, "request_id": f"fixture-{time.monotonic_ns()}"})
        result = self.call("/terminal", {"run_id": reply["run_id"]})
        assert result["outcome"]["status"] == "completed", result
        return result["outcome"]["value"]

    def start(self, *, extra=(), composition=None, trace=None, env=None):
        environment = {
            "EDEN_TUI_STATE_DIR": str(self.state),
            "GROK_XAI_API_BASE_URL": f"http://127.0.0.1:{self.business.server_port}",
            **(env or {}),
        }
        if trace:
            environment["EDEN_FRONTEND_TRACE"] = str(trace)
        self.terminal = Terminal(
            self.installation / "bin/eden",
            self.endpoint_file,
            width=110,
            height=40,
            command=[*self.arguments(composition), *extra],
            env=environment,
        )
        self.terminal.wait("workflow", seconds=30)
        return self.terminal

    def capture(self, name):
        assert self.terminal
        (self.output / f"{name}.txt").write_text(self.terminal.display)
        (self.output / f"{name}.ansi").write_bytes(self.terminal.output)

    def click(self, label):
        assert self.terminal
        self.terminal.wait(label, seconds=15)
        deadline = time.monotonic() + 15
        while "Working…" in self.terminal.display or "Loading…" in self.terminal.display:
            assert time.monotonic() < deadline, self.terminal.display
            self.terminal.read(0.05)
        lines = self.terminal.display.splitlines()
        row = next(index for index, line in enumerate(lines) if label in line)
        column = lines[row].index(label) + 2
        self.terminal.send(f"\x1b[<0;{column};{row + 1}M\x1b[<0;{column};{row + 1}m".encode())

    def __exit__(self, exc_type, exc, traceback):
        terminal_error = None
        if self.terminal and not self.terminal.closed:
            if exc:
                self.capture("failure")
            try:
                self.terminal.send(b"\x1b")
                self.terminal.send(b"\x15")
                exited(self.terminal)
            except BaseException as error:
                terminal_error = error
                if not self.terminal.closed:
                    try:
                        self.terminal.close(screen=False)
                    except BaseException:
                        pass
        for path in [self.endpoint_file, *self.state.glob("hosts/*.json")]:
            if path.name.endswith("startup.json"):
                continue
            if path.exists():
                try:
                    metadata = json.loads(path.read_text())
                    if "address" in metadata:
                        streaming.live.call(
                            metadata, "/shutdown", {"session_id": metadata["session_id"]}
                        )
                except (OSError, RuntimeError):
                    pass
        if self.host:
            try:
                self.host.wait(timeout=15)
            finally:
                if self.host.poll() is None:
                    self.host.kill()
                    self.host.wait()
        self.business.shutdown()
        self.business.server_close()
        self.business_thread.join()
        self.provider.allow.set()
        self.provider.shutdown()
        self.provider.server_close()
        self.thread.join()
        self.temporary.cleanup()
        if terminal_error and exc is None:
            raise terminal_error


def exited(terminal):
    """Use the reported Ctrl+D twice path and require normal terminal restoration."""
    terminal.send(b"\x04")
    terminal.send(b"\x04")
    deadline = time.monotonic() + 20
    while terminal.process.poll() is None and time.monotonic() < deadline:
        terminal.read(0.1)
    assert terminal.process.poll() == 0, terminal.display
    terminal.close(screen=False)


def completed_load(terminal, path, trace, offset=0, *, success=True, seconds=60):
    expected = f"history:{path.resolve()}::view-"
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        terminal.read(0.1)
        rows = trace_rows(trace)[offset:]
        requests = [
            row
            for row in rows
            if row["direction"] == "in"
            and row["method"] == "session/load"
            and isinstance(row["session"], str)
            and row["session"].startswith(expected)
        ]
        if requests:
            replies = [
                row for row in rows if row["direction"] == "out" and row["id"] == requests[-1]["id"]
            ]
            if replies and "Loading session" not in terminal.display:
                assert (replies[-1]["error"] is None) == success, replies[-1]
                return {"request": requests[-1], "reply": replies[-1]}
    raise AssertionError(f"No completed load for {expected}: {terminal.display}")


def select(terminal, path, trace, *, success=True):
    """Select the actual native picker row by its locator; no RPC loads."""
    offset = len(trace.read_text().splitlines())
    terminal.send(b"/")
    terminal.send(path.stem.encode())
    terminal.wait(path.stem[:15], seconds=30)
    terminal.send(b"\r")
    return completed_load(terminal, path, trace, offset, success=success)


def process_gone(pid):
    """On Linux distinguish a completed zombie from a still-running host process."""
    status = Path(f"/proc/{pid}/stat")
    return not status.exists() or status.read_text().split(")", 1)[1].strip().startswith("Z")


def wait_until(predicate, terminal=None, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        if terminal:
            terminal.read(0.03)
        else:
            threading.Event().wait(0.03)
    raise AssertionError("Causal observation did not complete before its deadline")


def records(path):
    return [
        record
        for line in path.read_text().splitlines()
        for record in json.loads(line).get("transaction", [json.loads(line)])
    ]


def writer_held(path):
    import fcntl

    with Path(str(path) + ".lock").open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(lock, fcntl.LOCK_UN)
        return False


def owner(fixture, path):
    info = fixture.call("/manage/info", {"path": str(path)})
    endpoint = Path(info["owner"])
    return endpoint, json.loads(endpoint.read_text())


def committed(endpoint, marker):
    return probe.streaming.live.wait_for(
        endpoint,
        lambda frame: frame["state"]["active_run"] is None
        and not frame["state"].get("shell_runs")
        and any(
            record["kind"] == "user_shell" and marker in record["payload"].get("command", "")
            for record in frame["history"]
        ),
    )


def trace_rows(path):
    """Observe complete JSONL records while the producer may be appending the next one."""
    return [json.loads(line) for line in path.read_text().rsplit("\n", 1)[0].splitlines()]
