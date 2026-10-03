from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from unittest.mock import patch

from kurama import Agent, ApprovalRequired, KuramaError, prompt, verify

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

    def options(self, scenario: str = "normal", **options):
        return {
            "workspace": self.root,
            "binary": self.binary,
            "env": {
                "KURAMA_TEST_SCENARIO": scenario,
                "KURAMA_TEST_PID": str(self.pid_file),
            },
            **options,
        }

    def agent(self, scenario: str = "normal", **options) -> Agent:
        return Agent(**self.options(scenario, **options))

    @contextmanager
    def error(self, code: str):
        with self.assertRaises(KuramaError) as raised:
            yield raised
        self.assertEqual(raised.exception.code, code)

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

    async def test_one_shot_prompt_returns_reply_and_closes_resumed_session(
        self,
    ) -> None:
        reply = await prompt("hello", **self.options(session_id="ses_resumed"))
        self.assertEqual(reply.text, "Hello, 世界\n")
        self.assertEqual(reply.session_id, "ses_resumed")
        self.assertEqual(reply.status, "completed")
        self.assert_process_exited()

    async def test_one_shot_verify_needs_only_explicit_trust(self) -> None:
        with patch.dict(
            os.environ,
            {
                "KURAMA_BIN": str(self.binary),
                "KURAMA_TEST_SCENARIO": "verify_approval",
                "KURAMA_TEST_PID": str(self.pid_file),
            },
        ):
            report = await verify("quick", approve=True)
        self.assertEqual(report.name, "quick")
        self.assertEqual(report.status, "passed")
        self.assertEqual(report.exit_code, 0)
        self.assert_process_exited()

    async def test_one_shot_errors_keep_partial_reply_and_close_process(self) -> None:
        with self.error("runtime_error") as raised:
            await prompt("failed", **self.options())
        self.assertEqual(raised.exception.reply.text, "partial")
        self.assertEqual(raised.exception.reply.status, "failed")
        self.assertEqual(raised.exception.reply.session_id, "ses_fixture")
        self.assert_process_exited()
        with self.error("unknown_recipe"):
            await verify("missing", **self.options())
        self.assert_process_exited()

    async def test_one_shot_approval_is_never_implicit(self) -> None:
        for call, argument, scenario in (
            (prompt, "approval", "normal"),
            (verify, "quick", "verify_approval"),
        ):
            with self.subTest(call=call.__name__):
                with self.assertRaises(ApprovalRequired) as raised:
                    await call(argument, **self.options(scenario))
                self.assertEqual(raised.exception.request.operation_id, "op_fixture")
                self.assert_process_exited()

    async def test_true_approves_once_and_does_not_leak_into_later_calls(self) -> None:
        reply = await prompt("approval", approve=True, **self.options("require_once"))
        self.assertEqual(reply.text, "approved")
        self.assert_process_exited()
        with self.assertRaises(ApprovalRequired):
            await prompt("approval", **self.options())
        self.assert_process_exited()

    async def test_one_shot_callbacks_can_approve_or_deny(self) -> None:
        async def approve(request):
            await asyncio.sleep(0)
            self.assertEqual(request.operation_id, "op_fixture")
            return "approve_once"

        reply = await prompt("approval", approve=approve, **self.options())
        self.assertEqual(reply.text, "approved")
        self.assert_process_exited()
        report = await verify(
            "quick", approve=lambda request: "deny", **self.options("verify_callback")
        )
        self.assertEqual(report.status, "denied")
        self.assertIsNone(report.exit_code)
        self.assert_process_exited()

    async def test_one_shot_callback_errors_keep_original_identity(self) -> None:
        original = ValueError("application policy failed")

        def broken(request):
            raise original

        async def broken_async(request):
            await asyncio.sleep(0)
            raise original

        for call, argument, scenario, callback in (
            (prompt, "approval", "normal", broken),
            (verify, "quick", "verify_callback", broken_async),
        ):
            with self.subTest(call=call.__name__):
                with self.assertRaises(ValueError) as raised:
                    await call(argument, approve=callback, **self.options(scenario))
                self.assertIs(raised.exception, original)
                self.assert_process_exited()

    async def test_one_shot_cancellation_finishes_callback_and_process_cleanup(
        self,
    ) -> None:
        for call, argument, scenario in (
            (prompt, "approval", "normal"),
            (verify, "quick", "verify_callback"),
        ):
            with self.subTest(call=call.__name__):
                entered, finished = asyncio.Event(), asyncio.Event()

                async def approval(request):
                    entered.set()
                    try:
                        await asyncio.Event().wait()
                    finally:
                        finished.set()
                    return "deny"

                task = asyncio.create_task(
                    call(argument, approve=approval, **self.options(scenario))
                )
                try:
                    await asyncio.wait_for(entered.wait(), 2)
                    task.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await asyncio.wait_for(task, 2)
                    self.assertTrue(finished.is_set())
                    self.assert_process_exited()
                finally:
                    task.cancel()
                    await asyncio.gather(task, return_exceptions=True)

    async def test_one_shot_recovery_approval_and_startup_error_cleanup(self) -> None:
        for call, argument in ((prompt, "hello"), (verify, "quick")):
            with self.subTest(call=call.__name__):
                result = await call(
                    argument,
                    approve=True,
                    **self.options("recovery_once", session_id="ses_resumed"),
                )
                self.assertEqual(
                    result.status, "completed" if call is prompt else "passed"
                )
                self.assert_process_exited()
                with self.assertRaises(ApprovalRequired):
                    await call(
                        argument, **self.options("recovery", session_id="ses_resumed")
                    )
                self.assert_process_exited()
                with self.error("incompatible_protocol"):
                    await call(argument, **self.options("incompatible"))
                self.assert_process_exited()

    async def test_aggregated_reply_bound_counts_utf8_bytes_and_keeps_agent_reusable(
        self,
    ) -> None:
        async with self.agent() as agent:
            reply = await agent.prompt("reply_limit")
            self.assertEqual(reply.text, "é" * (8 * 1024 * 1024))
            self.assertEqual(reply.status, "completed")
            with self.error("reply_too_large"):
                await agent.prompt("reply_overflow")
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")
        self.assert_process_exited()

    async def test_streaming_is_not_limited_by_reply_aggregation(self) -> None:
        async with self.agent() as agent:
            byte_count = 0
            async with agent.stream("reply_overflow") as events:
                async for event in events:
                    if event.type == "text":
                        byte_count += len(event.text.encode("utf-8"))
                    if byte_count > 16 * 1024 * 1024:
                        break
            self.assertEqual(byte_count, 16 * 1024 * 1024 + 2)
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")
        self.assert_process_exited()

    async def test_default_approval_cancels_and_drains_before_reuse(self) -> None:
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

        async with self.agent(approve=approve) as agent:
            self.assertEqual((await agent.prompt("approval")).text, "approved")
        self.assertEqual(requests[0].operation_id, "op_fixture")
        async with self.agent(approve=lambda request: "deny") as agent:
            self.assertEqual((await agent.prompt("approval")).text, "denied")

    async def test_callback_exception_is_preserved_and_next_turn_is_clean(self) -> None:
        original = ValueError("application policy failed")

        def broken(request):
            raise original

        async with self.agent(approve=broken) as agent:
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
            approve=lambda request: "approve_once",
        ) as agent:
            self.assertEqual(agent.session_id, "ses_resumed")
            event = await anext(agent.events())
            self.assertEqual(event.type, "status")
            self.assertEqual(event.message, "recovered")
            reply = await agent.prompt("next")
            self.assertEqual(reply.session_id, "ses_resumed")
            self.assertEqual(reply.text, "Hello, 世界\n")

    async def test_recovery_without_approval_fails_safely(self) -> None:
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
            async with self.agent("recovery", approve=broken):
                self.fail("Callback should fail")
        self.assertIs(raised.exception, original)
        self.assert_process_exited()

    async def test_recovery_approval_can_outlast_the_handshake_deadline(self) -> None:
        async def approve(request):
            await asyncio.sleep(0.75)
            return "approve_once"

        with patch("kurama.client._STARTUP_TIMEOUT", 0.5):
            async with self.agent("recovery", approve=approve) as agent:
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
            with self.error("busy"):
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
            with self.error("runtime_error") as raised:
                await agent.prompt("failed")
            self.assertEqual(raised.exception.reply.text, "partial")
            self.assertEqual(raised.exception.reply.status, "failed")
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_request_rejection_does_not_poison_next_turn(self) -> None:
        async with self.agent() as agent:
            with self.error("invalid_params") as raised:
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
            with self.error("unknown_recipe") as raised:
                await agent.verify("missing")
            self.assertEqual(raised.exception.code, "unknown_recipe")
            self.assertEqual((await agent.prompt("next")).text, "Hello, 世界\n")

    async def test_protocol_mismatch_and_old_binary_fail_actionably(self) -> None:
        with self.error("incompatible_protocol") as raised:
            async with self.agent("incompatible"):
                self.fail("Incompatible server must not open")
        self.assertEqual(raised.exception.code, "incompatible_protocol")
        self.assert_process_exited()
        with self.error("process_error") as raised:
            async with self.agent("old_binary"):
                self.fail("Old binary must not open")
        self.assertIn("--stdio", str(raised.exception))
        self.assertNotIn("secret-value", str(raised.exception))
        self.assert_process_exited()

    async def test_invalid_framing_fails_stream_and_closes_process(self) -> None:
        for text, code in (
            ("truncated", "invalid_protocol"),
            ("oversized", "frame_too_large"),
        ):
            with self.subTest(text=text):
                async with self.agent() as agent:
                    with self.error(code):
                        await agent.prompt(text)
                self.assert_process_exited()

    async def test_exit_resolves_execution_and_request_waiters(self) -> None:
        async with self.agent() as agent:
            with self.error("process_error"):
                await asyncio.wait_for(agent.prompt("exit"), 2)
        async with self.agent("status_exit") as agent:
            execution = asyncio.create_task(agent.prompt("wait"))
            await self.waiting(agent)
            with self.error("process_error"):
                await asyncio.wait_for(agent.verification_status(), 2)
            with self.error("process_error"):
                await asyncio.wait_for(execution, 2)
        self.assert_process_exited()

    async def test_close_unblocks_pending_control_request(self) -> None:
        agent = await self.agent("status_wait").open()
        pending = asyncio.create_task(agent.verification_status())
        self.assertEqual((await anext(agent.events())).message, "status requested")
        await agent.close()
        with self.error("closed"):
            await asyncio.wait_for(pending, 2)
        self.assert_process_exited()

    async def test_slow_consumer_overflow_is_explicit_and_cleanup_is_bounded(
        self,
    ) -> None:
        agent = await self.agent().open()
        events = agent.stream("overflow")
        try:
            # Opening does not consume events; the process may fill the buffer
            # before acceptance returns, which must fail rather than deadlock.
            with self.error("backpressure"):
                async with events:
                    await asyncio.sleep(0.1)
                    async for _ in events:
                        pass
        finally:
            await asyncio.wait_for(agent.close(), 2)
        self.assert_process_exited()

    async def test_shutdown_deadline_kills_stuck_owned_process(self) -> None:
        agent = await self.agent("hang_shutdown").open()
        with patch("kurama.client._SHUTDOWN_TIMEOUT", 0.2):
            await asyncio.wait_for(agent.close(), 2)
        self.assert_process_exited()

    async def test_handshake_deadline_closes_unresponsive_process(self) -> None:
        with (
            patch("kurama.client._STARTUP_TIMEOUT", 0.5),
            patch("kurama.client._SHUTDOWN_TIMEOUT", 0.2),
        ):
            with self.error("process_error"):
                await prompt("hello", **self.options("hang_handshake"))
        self.assert_process_exited()

    async def test_byte_backpressure_closes_process_before_any_event_is_dropped(
        self,
    ) -> None:
        async with self.agent() as agent:
            with self.error("backpressure"):
                async with agent.stream("overflow_bytes") as events:
                    async with asyncio.timeout(5):
                        while True:
                            try:
                                os.kill(int(self.pid_file.read_text()), 0)
                            except ProcessLookupError:
                                break
                            await asyncio.sleep(0.01)
                    await anext(events)
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

        agent = await self.agent(approve=approval).open()
        task = asyncio.create_task(agent.prompt("approval"))
        await asyncio.wait_for(entered.wait(), 2)
        await asyncio.wait_for(agent.close(), 2)
        with self.error("closed"):
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

        with self.error("process_error"):
            async with self.agent("recovery_exit", approve=approval):
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
