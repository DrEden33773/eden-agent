"""The SSE fixture must not confuse a socket write with frontend consumption."""

import importlib
import json
import socket
import unittest
import urllib.request

fixture = importlib.import_module("verify-router-models")


class RouterVerificationTests(unittest.TestCase):
    def test_sent_progress_does_not_complete_before_observer_consumes_it(self):
        router = fixture.Router()
        connection = socket.create_connection(router.http.server_address, timeout=5)
        try:
            connection.sendall(b"GET /models/sse HTTP/1.0\r\nHost: localhost\r\n\r\n")
            self.assertTrue(router.sse_entered.wait(5))
            request = urllib.request.Request(
                router.base + "/models/load",
                data=json.dumps({"model": "event-load"}).encode(),
                headers={"Content-Type": "application/json"},
            )
            with urllib.request.urlopen(request, timeout=5) as response:
                self.assertEqual(response.status, 200)
            self.assertTrue(router.load_progress_sent.wait(5))

            def state():
                with urllib.request.urlopen(router.base + "/models", timeout=5) as response:
                    models = json.load(response)["data"]
                return next(
                    model["status"]["value"] for model in models if model["id"] == "event-load"
                )

            # The SSE bytes are deliberately still unread while independent polls finish.
            for _ in range(4):
                self.assertEqual(state(), "loading")
            received = b""
            while b'"value": 0.4' not in received:
                received += connection.recv(4096)
            router.load_progress_observed.set()
            self.assertEqual(state(), "loaded")
        finally:
            connection.close()
            router.close()
