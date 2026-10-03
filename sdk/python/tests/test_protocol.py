from __future__ import annotations

import json
import unittest
from pathlib import Path

from kurama import ProtocolError
from kurama._protocol import (
    MAX_FRAME_BYTES,
    decode_event,
    decode_frame,
    decode_report,
    request_id,
)

FIXTURES = json.loads(
    (Path(__file__).resolve().parents[3] / "protocol" / "sdk.fixtures.json").read_text()
)


class ProtocolTests(unittest.TestCase):
    def test_shared_terminal_fixtures_preserve_status_and_error(self) -> None:
        frames = FIXTURES["frames"]
        self.assertEqual(
            decode_event(frames["done_event"]["event"]).status, "completed"
        )
        self.assertEqual(
            decode_event(frames["cancelled_event"]["event"]).status, "cancelled"
        )
        failed = decode_event(frames["failed_event"]["event"])
        self.assertEqual(failed.status, "failed")
        self.assertEqual(failed.error.code, "runtime_error")
        self.assertEqual(failed.error.message, "Provider connection closed")

    def test_shared_report_distinguishes_not_run_from_success(self) -> None:
        frames = FIXTURES["frames"]
        report = decode_report(
            frames["verification_status_response"]["result"]["recipes"][0]
        )
        self.assertEqual(report.status, "not_run")
        self.assertIsNone(report.exit_code)
        self.assertIsNone(report.operation_id)
        success = decode_event(frames["verification_event"]["event"]).report
        self.assertEqual(success.status, "passed")
        self.assertEqual(success.exit_code, 0)
        self.assertEqual(success.operation_id, "op_fixture")

    def test_shared_invalid_lines_are_rejected_without_echoing_input(self) -> None:
        for fixture in FIXTURES["invalid_lines"]:
            with self.subTest(fixture=fixture["name"]):
                with self.assertRaises(ProtocolError):
                    frame = decode_frame(fixture["line"].encode())
                    request_id(frame.get("id"))
        with self.assertRaises(ProtocolError) as raised:
            decode_frame(b'{"secret":"never echo this" bad}\n')
        self.assertNotIn("never echo", str(raised.exception))

    def test_frame_boundary_counts_utf8_bytes_and_excludes_lf(self) -> None:
        prefix, suffix = b'{"text":"', b'"}\n'
        frame = (
            prefix + b"x" * (MAX_FRAME_BYTES - len(prefix) - len(suffix) + 1) + suffix
        )
        self.assertEqual(len(decode_frame(frame)["text"]), MAX_FRAME_BYTES - 11)
        with self.assertRaises(ProtocolError):
            decode_frame(frame[:-3] + b"x" + frame[-3:])
        with self.assertRaises(ProtocolError):
            decode_frame(frame[:-1])
        with self.assertRaises(ProtocolError):
            decode_frame(b'{"text":"\xff"}\n')

    def test_unknown_events_fail_instead_of_disappearing(self) -> None:
        with self.assertRaises(ProtocolError):
            decode_event({"type": "new_event_from_incompatible_server"})

    def test_nonfinite_numbers_and_invalid_correlation_ids_fail(self) -> None:
        with self.assertRaises(ProtocolError):
            decode_frame(b'{"number":NaN}\n')
        for identifier in ("0", "01", "-1", "１", 1, None):
            with self.subTest(identifier=identifier):
                with self.assertRaises(ProtocolError):
                    request_id(identifier)
        self.assertEqual(request_id("9007199254740993"), "9007199254740993")


if __name__ == "__main__":
    unittest.main()
