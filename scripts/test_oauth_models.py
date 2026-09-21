"""Closed-input probes must distinguish a reaped login from a surviving pipe reader."""

import errno
import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

import oauth_models


class PrivateInputCleanupTests(unittest.TestCase):
    def test_finished_login_probe_does_not_leave_buffered_bytes_for_close(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            login = oauth_models.Login(
                [sys.executable, "-c", "pass"], pathlib.Path(temporary), dict(os.environ)
            )
            try:
                self.assertEqual(login.finish(True), "")
            finally:
                login.close()

    def test_closed_pipe_passes(self) -> None:
        reader, writer = os.pipe()
        os.close(reader)
        try:
            oauth_models.assert_pipe_reader_closed(writer)
        finally:
            os.close(writer)

    def test_live_pipe_reader_fails(self) -> None:
        reader, writer = os.pipe()
        try:
            with self.assertRaisesRegex(AssertionError, "reader alive"):
                oauth_models.assert_pipe_reader_closed(writer)
        finally:
            os.close(reader)
            os.close(writer)

    def test_unexpected_io_error_propagates(self) -> None:
        reader, writer = os.pipe()
        try:
            with mock.patch.object(
                oauth_models.os, "write", side_effect=OSError(errno.EIO, "test I/O failure")
            ):
                with self.assertRaises(OSError) as raised:
                    oauth_models.assert_pipe_reader_closed(writer)
            self.assertEqual(raised.exception.errno, errno.EIO)
        finally:
            os.close(reader)
            os.close(writer)


if __name__ == "__main__":
    unittest.main()
