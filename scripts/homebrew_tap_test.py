"""Completion-gate tests use fake time and network; no real waits or requests."""
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import homebrew_tap as tap


class Clock:
    def __init__(self):
        self.now = 0

    def time(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class TapTests(unittest.TestCase):
    def wait(self, responses, timeout=40):
        clock = Clock()
        messages = []
        remaining = iter(responses)

        def fetch(request_timeout):
            self.assertGreater(request_timeout, 0)
            self.assertLessEqual(request_timeout, 20)
            result = next(remaining)
            if isinstance(result, Exception):
                raise result
            return result

        tap.wait_for_formula(b'version "1.2.3"\n', fetch, timeout=timeout,
                             interval=20, clock=clock.time, sleep=clock.sleep,
                             report=messages.append)
        return clock.now, messages

    def test_identical_is_immediate(self):
        elapsed, messages = self.wait([b'version "1.2.3"\n'])
        self.assertEqual(elapsed, 0)
        self.assertIn("matches verified bytes", messages[-1])

    def test_same_version_different_bytes_times_out(self):
        with self.assertRaisesRegex(TimeoutError, "NOT complete.*formula differs"):
            self.wait([b'version "1.2.3"\r\n'] * 2)

    def test_eventual_merge(self):
        elapsed, _ = self.wait([b"old", b'version "1.2.3"\n'])
        self.assertEqual(elapsed, 20)

    def test_transient_error_recovers(self):
        elapsed, messages = self.wait([tap.TapReadError("temporary read failure"),
                                     b'version "1.2.3"\n'])
        self.assertEqual(elapsed, 20)
        self.assertIn("temporary read failure", messages[0])

    def test_persistent_errors_timeout_with_last_reason(self):
        with self.assertRaisesRegex(TimeoutError, "NOT complete.*network unavailable"):
            self.wait([tap.TapReadError("network unavailable")] * 2)

    def test_short_deadline_does_not_oversleep(self):
        clock = Clock()
        with self.assertRaises(TimeoutError):
            tap.wait_for_formula(b"want", lambda _: b"old", timeout=7, interval=20,
                                 clock=clock.time, sleep=clock.sleep, report=lambda _: None)
        self.assertEqual(clock.now, 7)

    def test_invalid_wait_configuration(self):
        with self.assertRaises(ValueError):
            tap.wait_for_formula(b"want", lambda _: b"", timeout=0)

    @patch("homebrew_tap.subprocess.run")
    def test_api_uses_default_branch_and_preserves_bytes(self, run):
        formula = b'version "1.2.3"\r\n'
        run.return_value = subprocess.CompletedProcess([], 0, json.dumps({
            "type": "file", "encoding": "base64",
            "content": base64.b64encode(formula).decode() + "\n",
        }).encode())
        self.assertEqual(tap.read_formula("example/tap"), formula)
        args, call_options = run.call_args
        self.assertEqual(args[0][-1], "repos/example/tap/contents/Formula/knit.rb")
        self.assertNotIn("ref=", " ".join(args[0]))
        self.assertEqual(call_options["timeout"], 20)

    @patch("homebrew_tap.subprocess.run")
    def test_network_timeout_is_retryable(self, run):
        run.side_effect = subprocess.TimeoutExpired("gh", 20)
        with self.assertRaisesRegex(tap.TapReadError, "TimeoutExpired"):
            tap.read_formula("example/tap")

    @patch("homebrew_tap.subprocess.run")
    def test_failed_read_does_not_echo_stderr(self, run):
        run.return_value = subprocess.CompletedProcess([], 1, b"", b"sensitive diagnostic")
        with self.assertRaisesRegex(tap.TapReadError, "gh exit 1") as error:
            tap.read_formula("example/tap")
        self.assertNotIn("sensitive", str(error.exception))

    @patch("homebrew_tap.subprocess.run")
    def test_malformed_response_is_retryable(self, run):
        run.return_value = subprocess.CompletedProcess([], 0, b"{}")
        with self.assertRaises(tap.TapReadError):
            tap.read_formula("example/tap")

    def preflight(self, mode, token, actual):
        with tempfile.TemporaryDirectory() as directory:
            formula = Path(directory) / "knit.rb"
            formula.write_bytes(b"expected")
            with patch.dict(os.environ, {"TAP_TOKEN": token}), patch.object(
                sys, "argv", ["homebrew_tap.py", "preflight", "--repo", "example/tap",
                              "--formula", str(formula), "--mode", mode]
            ), patch.object(tap, "read_formula", return_value=actual) as read:
                result = tap.main()
                return result, read.call_count

    def test_explicit_manual_needs_no_credential(self):
        self.assertEqual(self.preflight("manual", "", b"old"), (0, 0))

    def test_automatic_recovery_needs_no_credential_when_identical(self):
        self.assertEqual(self.preflight("automatic", "", b"expected"), (0, 1))

    def test_automatic_missing_credential_is_not_manual_fallback(self):
        with self.assertRaisesRegex(ValueError, "requires HOMEBREW_TAP_TOKEN"):
            self.preflight("automatic", "", b"old")

    def test_automatic_credential_permits_publication(self):
        self.assertEqual(self.preflight("automatic", "synthetic", b"old"), (0, 0))


if __name__ == "__main__":
    unittest.main()
