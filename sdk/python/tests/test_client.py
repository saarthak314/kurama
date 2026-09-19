from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from kurama import (
    Agent,
    ApprovalRequired,
    BufferOverflowError,
    BusyError,
    ClosedError,
    ProcessError,
    ProtocolError,
    ServerError,
    TurnFailed,
)

FIXTURES = Path(__file__).resolve().parents[3] / "protocol" / "sdk.fixtures.json"
FRAMES = json.loads(FIXTURES.read_text())["frames"]


@unittest.skipUnless(os.name == "posix", "Released SDK platforms are Unix")
class ClientTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "kurama"
        server = Path(__file__).with_name("fake_kurama.py")
        self.binary.write_text(
            f"#!{sys.executable}\nimport runpy\nrunpy.run_path({str(server)!r}, run_name='__main__')\n"
        )
        self.binary.chmod(0o755)
        self.pid_file = self.root / "pid"

    def agent(self, scenario: str = "normal", **options) -> Agent:
        return Agent(
            workspace=self.root,
            binary=self.binary,
            shutdown_timeout=0.2,
            request_timeout=2,
            startup_timeout=10,
            env={
                "KURAMA_TEST_SCENARIO": scenario,
                "KURAMA_TEST_PID": str(self.pid_file),
            },
            **options,
        )

    def assert_process_exited(self) -> None:
        pid = int(self.pid_file.read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    async def waiting(self, agent: Agent) -> None:
        async with asyncio.timeout(2):
            async for event in agent.events():
                if event.type == "status" and event.message == "waiting":
                    return

    async def test_fragmented_unicode_frames_and_repeat_prompts(self) -> None:
        async with self.agent("fragmented", profile="fixture") as agent:
            first = await agent.prompt("hello")
            second = await agent.prompt("again", explicit_delegation=True)
            self.assertEqual(first.text, FRAMES["text_event"]["event"]["text"])
            self.assertEqual(second.text, first.text)
            self.assertEqual(first.status, "completed")
            self.assertEqual(first.session_id, "ses_fixture")
        self.assert_process_exited()

    async def test_missing_callback_cancels_and_drains_before_reuse(self) -> None:
        async with self.agent() as agent:
            with self.assertRaises(ApprovalRequired) as raised:
                await agent.prompt("approval")
            self.assertEqual(raised.exception.code, "approval_required")
            self.assertEqual(raised.exception.request.operation_id, "op_fixture")
            self.assertEqual(
                (await agent.prompt("next")).text, FRAMES["text_event"]["event"]["text"]
            )
            self.assertFalse(await agent.cancel())

    async def test_manual_approval_and_shared_fixture_interpretation(self) -> None:
        async with self.agent() as agent:
            seen = {}
            async with agent.stream("fixtures") as events:
                async for event in events:
                    seen[event.type] = event
                    if event.type == "approval":
                        self.assertEqual(event.request.operation["type"], "bash")
                        self.assertEqual(
                            event.request.arguments["command"], "cargo test"
                        )
                        await agent.approve(event.request.operation_id)
            self.assertEqual(seen["text"].text, "Hello, 世界\n")
            self.assertEqual(seen["tool_started"].context, "cargo test")
            self.assertEqual(seen["tool_output"].chunk, "tests passed\n")
            self.assertEqual(seen["tool_completed"].result.metadata["exit_code"], 0)
            self.assertEqual(seen["usage"].usage.input_tokens, 12)
            self.assertEqual(seen["verification"].report.status, "passed")
            self.assertEqual(seen["done"].status, "completed")

    async def test_sync_and_async_callbacks_are_user_facing_approvals(self) -> None:
        requests = []

        async def approve(request):
            requests.append(request)
            await asyncio.sleep(0)
            return "approve_once"

        async with self.agent(on_approval=approve) as agent:
            self.assertEqual((await agent.prompt("approval")).text, "approved")
        self.assertEqual(requests[0].operation_id, "op_fixture")
        async with self.agent(on_approval=lambda request: "deny") as agent:
            self.assertEqual((await agent.prompt("approval")).text, "denied")

    async def test_callback_exception_is_preserved_and_next_turn_is_clean(self) -> None:
        original = ValueError("application policy failed")

        def broken(request):
            raise original

        async with self.agent(on_approval=broken) as agent:
            with self.assertRaises(ValueError) as raised:
                await agent.prompt("approval")
            self.assertIs(raised.exception, original)
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_recovery_uses_callback_and_keeps_events_out_of_next_turn(
        self,
    ) -> None:
        async with self.agent(
            "recovery",
            session_id="ses_resumed",
            on_approval=lambda request: "approve_once",
        ) as agent:
            self.assertEqual(agent.session_id, "ses_resumed")
            event = await anext(agent.events())
            self.assertEqual(event.type, "status")
            self.assertEqual(event.message, "recovered")
            reply = await agent.prompt("next")
            self.assertEqual(reply.session_id, "ses_resumed")
            self.assertEqual(reply.text, "Hello, 世界\n")

    async def test_recovery_without_callback_fails_safely(self) -> None:
        with self.assertRaises(ApprovalRequired):
            async with self.agent("recovery", session_id="ses_resumed"):
                self.fail("Recovery cannot silently approve")
        self.assert_process_exited()

    async def test_callback_timeout_error_is_not_reclassified_as_startup_failure(
        self,
    ) -> None:
        original = TimeoutError("application timeout")

        def broken(request):
            raise original

        with self.assertRaises(TimeoutError) as raised:
            async with self.agent("recovery", on_approval=broken):
                self.fail("Callback should fail")
        self.assertIs(raised.exception, original)
        self.assert_process_exited()

    async def test_recovery_approval_can_outlast_the_handshake_deadline(self) -> None:
        async def approve(request):
            await asyncio.sleep(0.75)
            return "approve_once"

        async with Agent(
            workspace=self.root,
            binary=self.binary,
            startup_timeout=0.5,
            shutdown_timeout=0.2,
            on_approval=approve,
            env={
                "KURAMA_TEST_SCENARIO": "recovery",
                "KURAMA_TEST_PID": str(self.pid_file),
            },
        ) as agent:
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")
        self.assert_process_exited()

    async def test_resume_preserves_session_identity(self) -> None:
        async with self.agent() as first:
            session = (await first.prompt("first")).session_id
        async with self.agent(session_id=session) as resumed:
            self.assertEqual((await resumed.prompt("second")).session_id, session)

    async def test_cancelled_stream_has_one_terminal_and_next_turn_is_isolated(
        self,
    ) -> None:
        async with self.agent() as agent:
            async with agent.stream("wait") as events:
                self.assertEqual((await anext(events)).type, "status")
                self.assertTrue(await agent.cancel())
                remaining = [event async for event in events]
                terminals = [event for event in remaining if event.type == "done"]
                self.assertEqual([event.status for event in terminals], ["cancelled"])
            self.assertFalse(await agent.cancel())
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_task_cancellation_drains_accepted_and_not_yet_accepted_turns(
        self,
    ) -> None:
        for text in ("wait", "pending_acceptance"):
            with self.subTest(text=text):
                async with self.agent() as agent:
                    task = asyncio.create_task(agent.prompt(text))
                    await self.waiting(agent)
                    task.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await task
                    self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_stream_context_break_cancels_before_reuse(self) -> None:
        async with self.agent() as agent:
            async with agent.stream("wait") as events:
                async for event in events:
                    self.assertEqual(event.type, "status")
                    break
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_explicit_stream_close_and_busy_rejection(self) -> None:
        async with self.agent() as agent:
            events = agent.stream("wait")
            await anext(events)
            with self.assertRaises(BusyError):
                await agent.prompt("overlap")
            await events.aclose()
            await events.aclose()
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_failed_terminal_is_typed_in_stream_and_error_in_prompt(self) -> None:
        async with self.agent() as agent:
            events = [event async for event in agent.stream("failed")]
            self.assertEqual(events[-1].type, "done")
            self.assertEqual(events[-1].status, "failed")
            self.assertEqual(events[-1].error.code, "runtime_error")
            with self.assertRaises(TurnFailed) as raised:
                await agent.prompt("failed")
            self.assertEqual(raised.exception.reply.text, "partial")
            self.assertEqual(raised.exception.reply.status, "failed")
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_request_rejection_does_not_poison_next_turn(self) -> None:
        async with self.agent() as agent:
            with self.assertRaises(ServerError) as raised:
                await agent.prompt("rejected")
            self.assertEqual(raised.exception.code, "invalid_params")
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_verification_reports_failure_without_protocol_failure(self) -> None:
        async with self.agent() as agent:
            failed = await agent.verify("fails")
            self.assertEqual(failed.status, "failed")
            self.assertEqual(failed.exit_code, 1)
            self.assertEqual((await agent.verification_status())[0], failed)
            self.assertEqual((await agent.verify("quick")).status, "passed")
            with self.assertRaises(ServerError) as raised:
                await agent.verify("missing")
            self.assertEqual(raised.exception.code, "unknown_recipe")
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_protocol_mismatch_and_old_binary_fail_actionably(self) -> None:
        with self.assertRaises(ProtocolError) as raised:
            async with self.agent("incompatible"):
                self.fail("Incompatible server must not open")
        self.assertEqual(raised.exception.code, "incompatible_protocol")
        self.assert_process_exited()
        with self.assertRaises(ProcessError) as raised:
            async with self.agent("old_binary"):
                self.fail("Old binary must not open")
        self.assertIn("--stdio", str(raised.exception))
        self.assertNotIn("secret-value", str(raised.exception))
        self.assert_process_exited()

    async def test_invalid_framing_fails_stream_and_closes_process(self) -> None:
        for text in ("truncated", "oversized"):
            with self.subTest(text=text):
                async with self.agent() as agent:
                    with self.assertRaises(ProtocolError):
                        await agent.prompt(text)
                self.assert_process_exited()

    async def test_exit_resolves_execution_and_request_waiters(self) -> None:
        async with self.agent() as agent:
            with self.assertRaises(ProcessError):
                await asyncio.wait_for(agent.prompt("exit"), 2)
        async with self.agent("status_exit") as agent:
            execution = asyncio.create_task(agent.prompt("wait"))
            await self.waiting(agent)
            with self.assertRaises(ProcessError):
                await asyncio.wait_for(agent.verification_status(), 2)
            with self.assertRaises(ProcessError):
                await asyncio.wait_for(execution, 2)
        self.assert_process_exited()

    async def test_close_unblocks_pending_control_request(self) -> None:
        agent = await self.agent("status_wait").open()
        pending = asyncio.create_task(agent.verification_status())
        self.assertEqual((await anext(agent.events())).message, "status requested")
        await agent.close()
        with self.assertRaises(ClosedError):
            await asyncio.wait_for(pending, 2)
        self.assert_process_exited()

    async def test_slow_consumer_overflow_is_explicit_and_cleanup_is_bounded(
        self,
    ) -> None:
        agent = await self.agent(max_buffered_events=2).open()
        events = agent.stream("overflow")
        try:
            # Opening does not consume events; the process may fill the buffer
            # before acceptance returns, which must fail rather than deadlock.
            with self.assertRaises(BufferOverflowError):
                async with events:
                    await asyncio.sleep(0.1)
                    async for _ in events:
                        pass
        finally:
            await asyncio.wait_for(agent.close(), 2)
        self.assert_process_exited()

    async def test_shutdown_deadline_kills_stuck_owned_process(self) -> None:
        agent = await self.agent("hang_shutdown").open()
        await asyncio.wait_for(agent.close(), 2)
        self.assert_process_exited()

    async def test_close_interrupts_async_approval_callback(self) -> None:
        entered = asyncio.Event()
        finished = asyncio.Event()

        async def approval(request):
            entered.set()
            try:
                await asyncio.Event().wait()
            finally:
                finished.set()
            return "deny"

        agent = await self.agent(on_approval=approval).open()
        task = asyncio.create_task(agent.prompt("approval"))
        await asyncio.wait_for(entered.wait(), 2)
        await asyncio.wait_for(agent.close(), 2)
        with self.assertRaises(ClosedError):
            await asyncio.wait_for(task, 2)
        self.assertTrue(finished.is_set())
        self.assert_process_exited()

    async def test_process_exit_interrupts_recovery_callback(self) -> None:
        finished = asyncio.Event()

        async def approval(request):
            try:
                await asyncio.Event().wait()
            finally:
                finished.set()
            return "deny"

        with self.assertRaises(ProcessError):
            async with self.agent("recovery_exit", on_approval=approval):
                self.fail("Exited process cannot finish recovery")
        self.assertTrue(finished.is_set())
        self.assert_process_exited()

    async def test_binary_resolution_explicit_environment_then_path(self) -> None:
        environment = {"KURAMA_BIN": str(self.root / "not-installed")}
        with patch.dict(os.environ, environment):
            async with self.agent() as agent:
                self.assertEqual((await agent.prompt("explicit")).status, "completed")
        async with Agent(
            workspace=self.root, env={"KURAMA_BIN": str(self.binary)}
        ) as agent:
            self.assertEqual((await agent.prompt("environment")).status, "completed")
        with patch.dict(
            os.environ,
            {"PATH": str(self.root) + os.pathsep + os.environ.get("PATH", "")},
        ):
            with patch.dict(os.environ):
                os.environ.pop("KURAMA_BIN", None)
                async with Agent(workspace=self.root) as agent:
                    self.assertEqual((await agent.prompt("path")).status, "completed")

    async def test_yolo_launch_is_explicitly_opted_in(self) -> None:
        async with self.agent(mode="yolo") as agent:
            self.assertEqual((await agent.prompt("hello")).status, "completed")


if __name__ == "__main__":
    unittest.main()
