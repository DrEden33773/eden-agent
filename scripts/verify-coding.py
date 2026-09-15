#!/usr/bin/env python3
"""Installed coding acceptance with controlled HTTP and independent SDK authors."""
import copy
import hashlib
import http.server
import json
import os
import pathlib
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
from install import ROOT, install, library, package, target

PROVIDER = "eden.coding-provider.v1"
CONTEXT = "eden.coding-context.v2"
TOOL = "eden.coding-tool.v1"
STORE = "eden.session-store.v2"


def run(args, cwd, check=True, env=None):
    result = subprocess.run([str(a) for a in args], cwd=cwd, capture_output=True, text=True, encoding="utf-8", timeout=240, env=env)
    if check and result.returncode:
        raise AssertionError(f"{args}: {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def records(path):
    return [record for line in path.read_text(encoding="utf-8").splitlines() for record in (lambda value: value.get("transaction", [value]))(json.loads(line))]


def answer(text):
    return [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}]


def call(identity, name, **arguments):
    return [{"type": "function_call", "call_id": identity, "name": name, "arguments": json.dumps(arguments)}]


class Server:
    def __init__(self, respond, gated=False, partial=False):
        self.requests = []
        self.errors = []
        self.release = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.release.set()
                self.send_response(200)
                self.end_headers()

            def do_POST(self):
                try:
                    assert self.headers["Authorization"] == "Bearer controlled-verifier-key"
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    assert body["model"] == "controlled-model" and body["stream"] is True and body["store"] is False
                    owner.requests.append(body)
                    output = respond(body, len(owner.requests) - 1)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.write(b'data: {"type":"response.output_text.delta","delta":"provider gate"}\n\n')
                    self.wfile.flush()
                    if partial:
                        return
                    if gated and len(owner.requests) == 1:
                        assert owner.release.wait(30), "provider gate was not released"
                    event = {"type": "response.completed", "response": {"status": "completed", "output": output, "usage": {"input_tokens": 10, "output_tokens": 5}}}
                    self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except BaseException as error:
                    owner.errors.append(repr(error))

        self.http = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.http.daemon_threads = True
        self.address = f"127.0.0.1:{self.http.server_port}"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.release.set()
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


def configure(destination, composition, server, name):
    selected = copy.deepcopy(composition)
    for item in selected["packages"]:
        if item["descriptor"]["package"] == "model-access":
            item["config"] = {"endpoint": f"http://{server.address}/v1/responses", "model": "controlled-model", "api_key_env": "EDEN_VERIFY_KEY"}
    path = destination / f"{name}.json"
    write(path, selected)
    return path


def events(result, completed=True):
    data = [json.loads(line) for line in result.stdout.splitlines()]
    accepted = next(i for i, item in enumerate(data) if item["kind"] == "accepted")
    assert data[-1]["kind"] == "settled" and accepted < len(data) - 1
    terminal = data[-1]["payload"]
    assert (terminal["outcome"]["status"] == "completed") is completed, terminal
    assert terminal["cleanup_errors"] == [], terminal
    assert [e["sequence"] for e in data] == list(range(data[0]["sequence"], data[-1]["sequence"] + 1))
    assert (result.returncode == 0) is completed
    return data


def build_author(destination, scratch):
    sdk = scratch / "sdk"
    (sdk / "crates").mkdir(parents=True)
    for name in ["eden-protocol", "eden-plugin-sdk"]:
        shutil.copytree(ROOT / "crates" / name, sdk / "crates" / name)
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    manifest = re.sub(r'^members = .*$', 'members = ["crates/*"]', manifest, flags=re.MULTILINE)
    manifest = "\n".join(line for line in manifest.splitlines() if not line.startswith(("exclude =", "eden-kernel =", "eden-agent ="))) + "\n"
    (sdk / "Cargo.toml").write_text(manifest, encoding="utf-8")
    shutil.copy2(ROOT / "rust-toolchain.toml", scratch / "rust-toolchain.toml")
    author = scratch / "authors" / "coding-replacements"
    shutil.copytree(ROOT / "tests/contract-authors/coding-replacements", author, ignore=shutil.ignore_patterns("target"))
    cargo = author / "Cargo.toml"
    cargo.write_text(cargo.read_text(encoding="utf-8").replace("../../../crates/eden-plugin-sdk", "../../sdk/crates/eden-plugin-sdk"), encoding="utf-8")
    run(["cargo", "build", "--locked", "--target-dir", scratch / "author-target"], author)
    lib = library("author_coding_replacements")
    folder = destination / "plugins/coding-replacements/0.1.0"
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copy2(scratch / "author-target/debug" / lib, folder / lib)
    return package("coding-replacements", [PROVIDER, CONTEXT, TOOL, STORE, "eden.record-interpreter.v1", "eden.state-migrator.v1"], f"plugins/coding-replacements/0.1.0/{lib}", target())


def main():
    os.environ["EDEN_VERIFY_KEY"] = "controlled-verifier-key"
    run(["cargo", "build", "--workspace", "--locked"], ROOT)
    run(["cargo", "build", "-p", "eden-agent", "--example", "coding_probe", "--locked"], ROOT)
    artifacts = ROOT / "artifacts"
    artifacts.mkdir(exist_ok=True)
    destination = install(artifacts / "coding-install")
    suffix = ".exe" if sys.platform == "win32" else ""
    host = destination / "bin" / ("eden" + suffix)
    probe = destination / "bin" / ("coding_probe" + suffix)
    shutil.copy2(ROOT / "target/debug/examples" / probe.name, probe)
    fixed_host = host.read_bytes()
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results = {}
    with tempfile.TemporaryDirectory(prefix="eden-coding-acceptance-") as temp:
        scratch = pathlib.Path(temp)
        author = build_author(destination, scratch)
        caller = scratch / "unrelated caller 工作目录"
        caller.mkdir()
        project = scratch / "coding project"
        project.mkdir()
        source = project / "arithmetic.py"
        original = "# TODO TODO\ndef add(a, b):\n    return a - b\n"
        source.write_text(original, encoding="utf-8")
        history = scratch / "coding.jsonl"
        test_source = f'''import json, pathlib, unittest
from arithmetic import add
class ArithmeticTests(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)
        self.assertEqual(add(-2, 3), 1)
        history = [record for x in pathlib.Path({str(history)!r}).read_text(encoding="utf-8").splitlines() for record in json.loads(x).get("transaction", [json.loads(x)])]
        self.assertTrue(any(x["kind"] == "tool_intent" and x["payload"].get("call_id") == "tests" for x in history))
        pathlib.Path("tests-ran.txt").write_text("passed after durable intent", encoding="utf-8")
'''
        python = shlex.quote(pathlib.Path(sys.executable).as_posix())
        commands = [
            call("read", "read", path="arithmetic.py"),
            call("absent", "edit", path="arithmetic.py", old_text="not present", new_text="bad"),
            call("ambiguous", "edit", path="arithmetic.py", old_text="TODO", new_text="bad"),
            call("repair", "edit", path="arithmetic.py", old_text="return a - b", new_text="return a + b"),
            call("write-tests", "write", path="test_arithmetic.py", content=test_source),
            call("tests", "bash", command=f"{python} -m unittest -v && {python} -c 'print(\"x\" * 100000)'"),
            call("nonzero", "bash", command="exit 7"),
        ]

        def coding(body, index):
            if index:
                result = json.loads([item for item in body["input"] if item["type"] == "function_call_output"][-1]["output"])
                previous = commands[index - 1][0]["call_id"]
                persisted = records(history)
                assert any(r["kind"] == "tool_result" and r["payload"]["call_id"] == previous for r in persisted)
                if index == 1:
                    assert result["error"] is None and "return a - b" in result["text"]
                if index in [2, 3]:
                    assert result["error"] is not None and source.read_text(encoding="utf-8") == original
                if index == 4:
                    assert result["error"] is None and source.read_text(encoding="utf-8") == original.replace("return a - b", "return a + b")
                if index == 5:
                    assert result["error"] is None and (project / "test_arithmetic.py").read_text(encoding="utf-8") == test_source
                if index == 6:
                    assert result["exit_code"] == 0 and result["truncated"] is True
                    assert (project / "tests-ran.txt").read_text(encoding="utf-8") == "passed after durable intent"
                if index == 7:
                    assert result["exit_code"] == 7
            return commands[index] if index < len(commands) else answer("Repaired addition; two cases passed. Long output was truncated and exit 7 was observed.")

        server = Server(coding)
        config = configure(destination, composition, server, "coding-task")
        try:
            data = events(run([host, "--composition", config, "--cwd", project, "--session", history, "--json", "Repair addition and verify with tests"], caller))
            assert len(server.requests) == 8
            assert "return a + b" in source.read_text(encoding="utf-8")
            results["coding_task"] = {"requests": len(server.requests), "terminal": data[-1]["payload"], "tools": [c[0]["name"] for c in commands], "intent_observed_by_test_process": True, "exact_edit_failure_preserved_source": True, "long_output_truncated": True, "nonzero_exit": 7}
        finally:
            server.close()
        before = records(history)
        resume = Server(lambda body, _: answer("Reopened history includes the repaired addition and test results."))
        # Endpoint config is not part of the durable role identity.
        config = configure(destination, composition, resume, "coding-reopen")
        try:
            events(run([host, "--composition", config, "--cwd", project, "--session", history, "--json", "Review the previous coding result"], caller))
            assert any(i["type"] == "function_call_output" and i["call_id"] == "tests" for i in resume.requests[0]["input"])
            after = records(history)
            assert after[:len(before)] == before and after[-1]["run_id"] > before[-1]["run_id"]
            assert len({r["session_id"] for r in after}) == 1
            results["reopen"] = {"preserved_records": len(before), "new_record_count": len(after), "test_result_in_model_input": True}
        finally:
            resume.close()

        partial = Server(lambda *_: [], partial=True)
        try:
            config = configure(destination, composition, partial, "partial")
            failed = events(run([host, "--composition", config, "--cwd", project, "--session", scratch / "partial.jsonl", "--json", "partial stream"], caller, check=False), completed=False)
            assert any(e["kind"] == "model_text_delta" for e in failed)
            results["partial_stream"] = failed[-1]["payload"]
        finally:
            partial.close()

        # Explicit env-file is a CLI startup input; no implicit project/parent discovery.
        env_server = Server(lambda *_: answer("explicit env-file request"))
        try:
            env_project = scratch / "env parent"
            env_caller = env_project / "child"
            env_caller.mkdir(parents=True)
            env_file = env_project / ".env"
            env_file.write_text(f'OPENAI_MODEL="file-model"\nOPENAI_BASE_URL=http://{env_server.address}/v1\nOPENAI_API_KEY=wrong-file-key\n', encoding="utf-8")
            env_config = destination / "env-only.json"
            write(env_config, composition)
            isolated = dict(os.environ)
            for key in ["OPENAI_MODEL", "OPENAI_BASE_URL", "OPENAI_API_KEY", "EDEN_RESPONSES_PROFILE", "EDEN_API_KEY_ENV", "OPENAI_MAX_OUTPUT_TOKENS", "OPENAI_REASONING_EFFORT"]:
                isolated.pop(key, None)
            missing = run([host, "--composition", env_config, "--cwd", env_caller, "--no-session", "--json", "no implicit env"], env_caller, check=False, env=isolated)
            assert missing.returncode != 0 and not env_server.requests
            inherited = dict(isolated, OPENAI_MODEL="controlled-model", OPENAI_API_KEY="controlled-verifier-key")
            explicit = run([host, "--env-file", "../.env", "--composition", env_config, "--cwd", project, "--no-session", "--json", "explicit env"], env_caller, env=inherited)
            events(explicit)
            assert len(env_server.requests) == 1
            # Case aliases must retain source order on case-insensitive Windows environments.
            case_file = env_project / "case.env"
            case_file.write_text(f'openai_model=wrong-case-model\nOPENAI_MODEL=controlled-model\nOPENAI_BASE_URL=http://{env_server.address}/v1\nOPENAI_API_KEY=controlled-verifier-key\n', encoding="utf-8")
            events(run([host, "--env-file", "../case.env", "--composition", env_config, "--cwd", project, "--no-session", "--json", "case precedence"], env_caller, env=isolated))
            assert len(env_server.requests) == 2
            # Original explicit package settings also remain stronger than environment values.
            preferred = configure(destination, composition, env_server, "package-precedence")
            overridden = dict(inherited, OPENAI_MODEL="wrong-process-model", OPENAI_BASE_URL="http://127.0.0.1:1")
            events(run([host, "--env-file", "../.env", "--composition", preferred, "--cwd", project, "--no-session", "--json", "package wins"], env_caller, env=overridden))
            assert len(env_server.requests) == 3
            results["explicit_env_file"] = {"parent_discovery": False, "relative_to_caller": True, "environment_over_file": True, "package_over_environment": True, "case_alias_assignment_order": True}
        finally:
            env_server.close()

        for mode in ["queue", "steering_only", "cancel"]:
            server = Server(lambda *_: answer("probe response"), gated=True)
            try:
                config = configure(destination, composition, server, mode)
                result = run([probe, config, project, scratch / f"{mode}.jsonl", mode, server.address], caller)
                results[mode] = json.loads(result.stdout)
                if mode == "queue":
                    assert len(server.requests) == 3
                    text = json.dumps(server.requests[-1]["input"])
                    assert all(label in text for label in ["steer-one", "steer-two", "follow-one", "follow-two"])
                elif mode == "steering_only":
                    assert len(server.requests) == 3
                    text = json.dumps(server.requests[-1]["input"])
                    assert all(label in text for label in ["steer-one", "steer-two"])
                else:
                    assert len(server.requests) == 1
            finally:
                server.close()

        for role, label in [(PROVIDER, "provider"), (CONTEXT, "context"), (TOOL, "tool"), (STORE, "store")]:
            selected = copy.deepcopy(composition)
            selected["packages"].append(author)
            selected["roles"][role] = "coding-replacements"
            replacement_project = scratch / f"replacement-{label}"
            replacement_project.mkdir()
            replacement_history = scratch / f"replacement-{label}.jsonl"

            def replacement(body, index):
                if label == "context":
                    assert "INDEPENDENT_CONTEXT_MARKER" in json.dumps(body["input"])
                    assert [t["name"] for t in body["tools"]] == ["write"]
                if index == 0:
                    return call(f"{label}-write", "write", path="model-created.txt", content="changed by controlled provider\n")
                result = json.loads(next(item["output"] for item in body["input"] if item["type"] == "function_call_output"))
                if label == "tool":
                    assert result["text"] == "INDEPENDENT_TOOL_RESULT"
                return answer("Final model input contained " + result["text"])

            server = Server(replacement)
            try:
                config = configure(destination, selected, server, f"replacement-{label}")
                data = events(run([host, "--composition", config, "--cwd", replacement_project, "--session", replacement_history, "--json", "write a file"], caller))
                if label == "provider":
                    assert not server.requests
                    assert (replacement_project / "author-created.txt").read_text(encoding="utf-8") == "independent provider wrote this\n"
                elif label == "tool":
                    assert (replacement_project / "author-tool-effect.txt").read_text(encoding="utf-8") == "write:tool-write"
                    assert "INDEPENDENT_TOOL_RESULT" in json.dumps(data[-1])
                else:
                    assert (replacement_project / "model-created.txt").read_text(encoding="utf-8") == "changed by controlled provider\n"
                recovery = None
                if label == "store":
                    prior = records(replacement_history)
                    resumed = events(run([host, "--composition", config, "--cwd", replacement_project, "--session", replacement_history, "--json", "Review the preserved tool result"], caller))
                    recovered = records(replacement_history)
                    assert recovered[:len(prior)] == prior
                    assert recovered[-1]["run_id"] > prior[-1]["run_id"]
                    assert len({r["session_id"] for r in recovered}) == 1
                    assert any(i["type"] == "function_call_output" and i["call_id"] == "store-write" for i in server.requests[-1]["input"])
                    recovery = {"preserved_records":len(prior), "new_run_id":recovered[-1]["run_id"], "terminal":resumed[-1]["payload"]}
                # Read without any business/store plugin: remove composition availability entirely.
                offline_config = config.with_suffix(".disabled")
                config.rename(offline_config)
                plugins = destination / "plugins"
                offline_plugins = destination / "plugins-disabled"
                plugins.rename(offline_plugins)
                try:
                    exported = run([host, "--history", replacement_history], caller)
                finally:
                    offline_plugins.rename(plugins)
                assert exported.stdout.strip()
                public = records(replacement_history)
                assert any(r["kind"] == "tool_result" for r in public) and public[-1]["kind"] == "terminal"
                results[f"independent_{label}"] = {"terminal": data[-1]["payload"], "public_records": len(public), "offline_history_read": True, "all_native_plugins_unavailable_during_read": True, "selected_role": role}
                if recovery:
                    results[f"independent_{label}"]["recovery"] = recovery
            finally:
                server.close()

        for fail_kind in ["tool_intent", "tool_result"]:
            selected = copy.deepcopy(composition)
            failing_author = copy.deepcopy(author)
            failing_author["config"] = {"fail_kind":fail_kind}
            selected["packages"].append(failing_author)
            selected["roles"][STORE] = "coding-replacements"
            failure_project = scratch / f"fail-{fail_kind}"
            failure_project.mkdir()
            failure_history = scratch / f"fail-{fail_kind}.jsonl"
            call_id = f"failure-{fail_kind}"
            server = Server(lambda *_: call(call_id, "write", path="effect.txt", content="real effect before result persistence\n"))
            try:
                config = configure(destination, selected, server, f"fail-{fail_kind}")
                data = events(run([host, "--composition", config, "--cwd", failure_project, "--session", failure_history, "--json", "perform the write"], caller, check=False), completed=False)
                terminal = data[-1]["payload"]
                assert terminal["outcome"]["value"]["code"] == "PersistenceFailure", terminal
                assert len(server.requests) == 1
                durable = records(failure_history)
                assert not any(r["kind"] == fail_kind for r in durable)
                if fail_kind == "tool_intent":
                    assert not (failure_project / "effect.txt").exists()
                else:
                    assert (failure_project / "effect.txt").read_text(encoding="utf-8") == "real effect before result persistence\n"
                    assert call_id in terminal["outcome"]["value"]["message"], terminal
                    assert "may already have occurred" in terminal["outcome"]["value"]["message"], terminal
                    assert any(r["kind"] == "tool_intent" and r["payload"]["call_id"] == call_id for r in durable)
                results[f"persistence_failure_{fail_kind}"] = {"terminal":terminal, "effect_occurred":(failure_project / "effect.txt").exists(), "public_records":len(durable)}
            finally:
                server.close()

        selected = copy.deepcopy(composition)
        selected["packages"].append(author)
        selected["roles"][STORE] = "coding-replacements"
        config = destination / "open-abandon.json"
        write(config, selected)
        results["abandoned_session_open"] = json.loads(run([probe, config, project, scratch / "open-abandon.jsonl", "open-abandon"], caller).stdout)

        # An interrupted durable intention is context, never executable work on reopen.
        interrupted = scratch / "interrupted.jsonl"
        seed = copy.deepcopy(before[:1])
        seed.append({"schema_version": 2, "parent_id": 1, "branch": "main", "session_id": seed[0]["session_id"], "sequence": 2, "run_id": 1, "kind": "tool_intent", "payload": {"type": "tool_call", "call_id": "interrupted-write", "name": "write", "arguments": json.dumps({"path": "must-not-exist.txt", "content": "replayed"})}})
        interrupted.write_text(json.dumps({"schema_version":2,"transaction":seed}) + "\n", encoding="utf-8")
        server = Server(lambda *_: answer("Historical effects remain unknown."))
        try:
            config = configure(destination, composition, server, "interrupted")
            events(run([host, "--composition", config, "--cwd", project, "--session", interrupted, "--json", "Review interrupted history"], caller))
            assert not (project / "must-not-exist.txt").exists()
            assert "was not replayed" in json.dumps(server.requests[0]["input"])
            results["interrupted_intent"] = "reopened as uncertainty without replay"
        finally:
            server.close()
        memory = scratch / "memory-only"
        memory.mkdir()
        server = Server(lambda *_: answer("memory only"))
        try:
            config = configure(destination, composition, server, "memory")
            events(run([host, "--composition", config, "--cwd", memory, "--no-session", "--json", "do not write"], memory))
            assert list(memory.iterdir()) == []
            results["memory_only"] = "no local files created"
        finally:
            server.close()
        assert host.read_bytes() == fixed_host
    evidence = {"platform": platform.platform(), "target": target(), "commit": run(["git", "rev-parse", "HEAD"], ROOT).stdout.strip(), "source_dirty": bool(run(["git", "status", "--porcelain"], ROOT).stdout.strip()), "host_unchanged": True, "host_sha256": hashlib.sha256(fixed_host).hexdigest(), "independent_sdk_only_author_build": True, "provider_evidence": "controlled Responses HTTP; real authenticated provider call remains unaccepted", "scenarios": results}
    write(artifacts / "coding-verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
