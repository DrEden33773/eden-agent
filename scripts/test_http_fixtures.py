"""A provider failure must remain observable even when close races its handler."""

import http.client
import json
import pathlib
import runpy
import threading
import unittest
from typing import Any
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]


class HttpFixtureTests(unittest.TestCase):
    def test_router_sse_accepts_aborted_peer_but_keeps_other_handler_errors(self):
        namespace = runpy.run_path(str(ROOT / "scripts" / "verify-router-models.py"))
        for path in ("/models/sse", "/props"):
            with self.subTest(path=path):
                router = namespace["Router"]()
                router.withhold_headers = True
                failure = ConnectionAbortedError(10053, "peer cancelled")

                class AbortedSocket:
                    def recv(self, _count: int, error: BaseException = failure) -> bytes:
                        raise error

                socket = AbortedSocket()
                handler = router.http.RequestHandlerClass.__new__(router.http.RequestHandlerClass)
                handler.path = path
                handler.headers = {}
                handler.connection = socket

                def fail_reply(_value: object, error: BaseException = failure) -> None:
                    raise error

                handler.reply = fail_reply
                try:
                    with patch.object(
                        namespace["select"], "select", return_value=([socket], [], [])
                    ):
                        handler.do_GET()
                    self.assertEqual(router.streams, 0)
                    if path == "/models/sse":
                        self.assertEqual(router.errors, [])
                        self.assertTrue(router.stream_closed.is_set())
                    else:
                        self.assertEqual(router.errors, [repr(failure)])
                finally:
                    router.errors.clear()
                    router.close()

    def test_close_joins_active_handlers_before_asserting_errors(self):
        for script, key in (
            ("verify-coding.py", "controlled-verifier-key"),
            ("verify-context.py", "context-verifier-key"),
        ):
            with self.subTest(script=script):
                self.assert_close_joins_handler(script, key)

    def assert_close_joins_handler(self, script: str, key: str) -> None:
        entered, release, closed = threading.Event(), threading.Event(), threading.Event()
        close_errors: list[str] = []

        def respond(_body: dict[str, Any], _index: int) -> Any:
            entered.set()
            if not release.wait(5):
                raise AssertionError("test did not release handler")
            raise AssertionError("late handler failure")

        server = runpy.run_path(str(ROOT / "scripts" / script))["Server"](respond)

        def request() -> None:
            connection = http.client.HTTPConnection(server.address, timeout=5)
            try:
                connection.request(
                    "POST",
                    "/",
                    json.dumps({"model": "controlled-model", "stream": True, "store": False}),
                    {"Authorization": f"Bearer {key}"},
                )
                connection.getresponse().read()
            except http.client.RemoteDisconnected:
                pass  # Handler deliberately fails before writing response headers.
            finally:
                connection.close()

        def close() -> None:
            try:
                server.close()
            except AssertionError as error:
                close_errors.append(str(error))
            finally:
                closed.set()

        client = threading.Thread(target=request)
        closer = threading.Thread(target=close)
        client.start()
        try:
            self.assertTrue(entered.wait(5), "request never reached the controlled handler")
            closer.start()
            server.thread.join(2)
            self.assertFalse(server.thread.is_alive(), "accept loop did not stop")
            returned_before_handler = closed.wait(0.05)
        finally:
            release.set()
            client.join(5)
            if closer.ident is not None:
                closer.join(5)
            else:
                server.close()
        self.assertFalse(
            returned_before_handler,
            "close reported success before its active handler completed",
        )
        self.assertFalse(client.is_alive())
        self.assertFalse(closer.is_alive())
        self.assertEqual(1, len(close_errors), close_errors)
        self.assertIn("late handler failure", close_errors[0])


if __name__ == "__main__":
    unittest.main()
