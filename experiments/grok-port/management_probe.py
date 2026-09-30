"""Controlled failures and final-consumer observations for the native frontend probe."""

import http.client
import http.server
import json
from pathlib import Path


class ModelRelay(http.server.ThreadingHTTPServer):
    def __init__(self, address):
        super().__init__(("127.0.0.1", 0), Handler)
        self.address = address
        self.fail_defaults = False
        self.opened: list[Path] = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def forward(self):
        relay = self.server
        assert isinstance(relay, ModelRelay)
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        request = json.loads(body) if body else {}
        connection = None
        try:
            if (
                relay.fail_defaults
                and self.path == "/models/catalog"
                and request.get("request", {}).get("action") == "set_default"
            ):
                status = 200
                data = json.dumps(
                    {
                        "ok": False,
                        "error": {
                            "code": "InjectedSaveFailure",
                            "source": "fixture",
                            "message": "Default storage is temporarily unavailable",
                        },
                    }
                ).encode()
            else:
                connection = http.client.HTTPConnection(relay.address, timeout=120)
                connection.request(
                    self.command,
                    self.path,
                    body=body,
                    headers={"X-Eden-Token": self.headers["X-Eden-Token"]},
                )
                response = connection.getresponse()
                status = response.status
                data = response.read()
                if self.path == "/manage/open":
                    result = json.loads(data)
                    if result.get("ok") and "endpoint" in result.get("result", {}):
                        relay.opened.append(Path(result["result"]["endpoint"]))
            self.send_response(status)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            if connection is not None:
                connection.close()
