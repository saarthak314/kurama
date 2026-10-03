"""Deterministic test binary. It speaks real pipes, never contacts a model."""

from __future__ import annotations

import copy
import json
import os
import sys
import time
from pathlib import Path

FIXTURE = Path(__file__).resolve().parents[3] / "protocol" / "sdk.fixtures.json"
FRAMES = json.loads(FIXTURE.read_text())["frames"]
SCENARIO = os.environ.get("KURAMA_TEST_SCENARIO", "normal")
SESSION = "ses_fixture"
ACTIVE: str | None = None
ACTIVE_TEXT = ""
INITIALIZE: str | None = None
LAST_ID = 0
REPORT = copy.deepcopy(FRAMES["verification_event"]["event"]["report"])


def emit(frame: dict) -> None:
    data = (
        json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n"
    ).encode()
    if SCENARIO == "fragmented":
        for byte in data:
            os.write(1, bytes([byte]))
    else:
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()


def response(identifier: str, result: dict) -> None:
    emit({"type": "response", "id": identifier, "result": result})


def event(
    payload: dict, identifier: str | None = None, *, unsolicited: bool = False
) -> None:
    emit(
        {
            "type": "event",
            "request_id": None if unsolicited else identifier or ACTIVE,
            "session_id": SESSION,
            "event": payload,
        }
    )


def fixture_event(name: str, identifier: str | None = None) -> None:
    event(copy.deepcopy(FRAMES[name]["event"]), identifier)


def done(status: str = "completed") -> None:
    global ACTIVE
    event({"type": "done", "status": status})
    ACTIVE = None


def main() -> None:
    global ACTIVE, ACTIVE_TEXT, SESSION, INITIALIZE, LAST_ID, REPORT
    pid_file = os.environ.get("KURAMA_TEST_PID")
    if pid_file:
        Path(pid_file).write_text(str(os.getpid()))
    if "--stdio" not in sys.argv:
        sys.exit(2)
    if SCENARIO == "old_binary":
        sys.stderr.write("unknown option --stdio; secret-value-must-not-leak\n")
        sys.exit(2)
    hello = copy.deepcopy(FRAMES["hello"])
    if SCENARIO == "incompatible":
        hello["protocol_version"] = 99
    emit(hello)
    for line in sys.stdin.buffer:
        request = json.loads(line)
        identifier, method, params = request["id"], request["method"], request["params"]
        if str(int(identifier)) != identifier or int(identifier) <= LAST_ID:
            sys.exit(11)
        LAST_ID = int(identifier)
        if method == "initialize":
            SESSION = params.get("session_id", SESSION)
            result = {
                "session_id": SESSION,
                "workspace": str(Path(params["workspace"]).resolve()),
                "profile": params.get("profile", "fixture"),
                "mode": params["mode"],
            }
            if params["mode"] == "yolo" and "--yolo" not in sys.argv:
                sys.exit(12)
            if SCENARIO in {"recovery", "recovery_exit"}:
                INITIALIZE = identifier
                fixture_event("approval_event", identifier)
                if SCENARIO == "recovery_exit":
                    time.sleep(0.05)
                    sys.exit(19)
                globals()["INIT_RESULT"] = result
            else:
                response(identifier, result)
        elif method == "prompt":
            if ACTIVE is not None:
                emit(
                    {
                        "type": "response",
                        "id": identifier,
                        "error": {"code": "busy", "message": "Busy"},
                    }
                )
                continue
            ACTIVE, ACTIVE_TEXT = identifier, params["text"]
            if ACTIVE_TEXT == "rejected":
                ACTIVE = None
                emit(
                    {
                        "type": "response",
                        "id": identifier,
                        "error": {"code": "invalid_params", "message": "Rejected"},
                    }
                )
                continue
            if ACTIVE_TEXT != "pending_acceptance":
                response(identifier, {"accepted": True})
            if ACTIVE_TEXT in {"approval", "fixtures"}:
                fixture_event("approval_event")
            elif ACTIVE_TEXT in {"wait", "pending_acceptance"}:
                event({"type": "status", "message": "waiting"}, unsolicited=True)
                if ACTIVE_TEXT == "wait":
                    event({"type": "status", "message": "waiting"})
            elif ACTIVE_TEXT == "failed":
                event({"type": "text", "text": "partial"})
                fixture_event("failed_event")
                ACTIVE = None
            elif ACTIVE_TEXT == "truncated":
                os.write(1, b'{"type":"event"')
                sys.exit(17)
            elif ACTIVE_TEXT == "oversized":
                os.write(1, b"x" * 1_048_577 + b"\n")
            elif ACTIVE_TEXT == "exit":
                sys.exit(23)
            elif ACTIVE_TEXT == "overflow":
                for _ in range(1000):
                    event({"type": "text", "text": "x"})
                done()
            elif ACTIVE_TEXT == "duplicate_terminal":
                saved = ACTIVE
                done()
                event({"type": "done", "status": "completed"}, saved)
            else:
                fixture_event("text_event")
                done()
        elif method == "approve":
            if params["operation_id"] != "op_fixture":
                emit(
                    {
                        "type": "response",
                        "id": identifier,
                        "error": {
                            "code": "unknown_operation",
                            "message": "Unknown approval",
                        },
                    }
                )
                continue
            response(identifier, {"accepted": True})
            if INITIALIZE is not None:
                event({"type": "status", "message": "recovered"}, INITIALIZE)
                response(INITIALIZE, globals()["INIT_RESULT"])
                INITIALIZE = None
            elif ACTIVE_TEXT == "fixtures":
                for name in FRAMES:
                    if name.endswith("_event") and name not in {
                        "approval_event",
                        "done_event",
                        "cancelled_event",
                        "failed_event",
                    }:
                        fixture_event(name)
                done()
            elif params["response"] == "deny":
                event({"type": "text", "text": "denied"})
                done()
            else:
                event({"type": "text", "text": "approved"})
                done()
        elif method == "cancel":
            active = ACTIVE is not None
            response(identifier, {"cancelled": active})
            if active:
                if ACTIVE_TEXT == "pending_acceptance":
                    response(ACTIVE, {"accepted": True})
                event({"type": "text", "text": "old turn trailing text"})
                done("cancelled")
        elif method == "verification_status":
            if SCENARIO == "status_exit":
                sys.exit(24)
            if SCENARIO == "status_wait":
                event(
                    {"type": "status", "message": "status requested"}, unsolicited=True
                )
                continue
            response(identifier, {"recipes": [REPORT]})
        elif method == "verify":
            if params["name"] == "missing":
                emit(
                    {
                        "type": "response",
                        "id": identifier,
                        "error": {"code": "unknown_recipe", "message": "No recipe"},
                    }
                )
                continue
            ACTIVE = identifier
            response(identifier, {"accepted": True})
            REPORT = copy.deepcopy(FRAMES["verification_event"]["event"]["report"])
            if params["name"] == "fails":
                REPORT.update(name="fails", status="failed", exit_code=1)
            event({"type": "verification", "report": REPORT})
            done()
        elif method == "shutdown":
            if SCENARIO == "hang_shutdown":
                while True:
                    time.sleep(60)
            response(identifier, {"closed": True})
            return
        else:
            sys.exit(13)


if __name__ == "__main__":
    main()
