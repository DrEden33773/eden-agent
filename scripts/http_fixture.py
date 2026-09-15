"""Wakeable HTTP fixtures: stop accepting, join handlers, then inspect their errors."""

import http.server
import selectors
import socket
import socketserver
import threading


class FixtureHTTPServer(http.server.ThreadingHTTPServer):
    # server_close must join request threads before the owner's error assertion.
    daemon_threads = False
    block_on_close = True
    timeout = 0.0

    def __init__(
        self,
        server_address: tuple[str, int],
        handler: type[socketserver.BaseRequestHandler],
    ) -> None:
        super().__init__(server_address, handler)
        self._wake_read, self._wake_write = socket.socketpair()
        self._stopping = threading.Event()
        self._stopped = threading.Event()
        self._failure: BaseException | None = None

    def serve_forever(self, poll_interval: float = 0.5) -> None:
        # A control socket wakes select immediately. No teardown polling interval.
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(self, selectors.EVENT_READ)
                selector.register(self._wake_read, selectors.EVENT_READ)
                while not self._stopping.is_set():
                    for key, _ in selector.select():
                        if self._stopping.is_set():
                            break
                        if key.fileobj is self:
                            self.handle_request()
        except BaseException as error:
            self._failure = error
        finally:
            self._stopped.set()

    def shutdown(self) -> None:
        if self._stopped.is_set():
            return
        self._stopping.set()
        self._wake_write.send(b"x")
        self._stopped.wait()

    def server_close(self) -> None:
        super().server_close()
        self._wake_read.close()
        self._wake_write.close()
        if self._failure is not None:
            raise RuntimeError("HTTP fixture accept loop failed") from self._failure
