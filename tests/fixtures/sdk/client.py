import asyncio
import json
import os
from pathlib import Path

from kurama import Agent, ApprovalRequired


async def main():
    workspace = Path(os.environ["SDK_WORKSPACE"])
    options = {
        "workspace": str(workspace),
        "profile": "fixture",
        "mode": "supervised",
        "binary": os.environ["KURAMA_BIN"],
        "state_dir": os.environ["SDK_STATE"],
        "env": {"SDK_CANARY_SECRET": "sdk-secret-must-not-appear-in-protocol"},
    }
    replies = []
    approval_count = 0
    saw_child = False
    async with Agent(**options) as agent:
        session_id = agent.session_id
        simple = await agent.prompt("SDK_SIMPLE")
        assert simple.text == "SDK_SIMPLE_OK"
        assert simple.session_id == session_id
        replies.append(simple.text)

        streamed = ""
        async for event in agent.stream("SDK_STREAM"):
            if event.type == "text":
                streamed += event.text
        assert streamed == "stream 世界\nfinished"
        replies.append(streamed)

        written = ""
        async for event in agent.stream("SDK_WRITE"):
            if event.type == "approval":
                approval_count += 1
                await agent.approve(event.request.operation_id)
            if event.type == "text":
                written += event.text
        assert approval_count == 1
        assert written == "SDK_WRITE_DONE"
        assert (workspace / "sdk-result.txt").read_text() == "written\n"
        replies.append(written)

        try:
            await agent.prompt("SDK_NEEDS_APPROVAL")
        except ApprovalRequired as error:
            assert error.code == "approval_required"
            assert error.request.operation_id
        else:
            raise AssertionError("prompt silently passed an approval boundary")

        cancelled = False
        terminal = None
        async for event in agent.stream("SDK_CANCEL"):
            if (
                event.type == "text"
                and "SDK_CANCEL_BEGIN" in event.text
                and not cancelled
            ):
                await agent.cancel()
                cancelled = True
            if event.type == "done":
                terminal = event.status
        assert cancelled and terminal == "cancelled"
        after = await agent.prompt("SDK_AFTER_CANCEL")
        assert after.text == "SDK_AFTER_CANCEL_OK"
        replies.append(after.text)

        delegated = ""
        async for event in agent.stream("SDK_AGENTS", explicit_delegation=True):
            if event.type == "agent_updated" and event.snapshot.state == "completed":
                saw_child = True
            if event.type == "text":
                delegated += event.text
        assert saw_child and delegated == "SDK_AGENTS_DONE"
        replies.append(delegated)

    async def approve(_request):
        return "approve_once"

    async with Agent(**options, session_id=session_id, on_approval=approve) as resumed:
        assert resumed.session_id == session_id
        reply = await resumed.prompt("SDK_RESUME")
        assert reply.text == "SDK_RESUME_OK"
        replies.append(reply.text)
        initial = await resumed.verification_status()
        assert [(report.name, report.status) for report in initial] == [
            ("fail", "not_run"),
            ("quick", "not_run"),
        ]
        passed = await resumed.verify("quick")
        assert passed.status == "passed" and passed.exit_code == 0
        assert passed.operation_id
        assert (workspace / "verified.txt").read_text() == "verified\n"
        failed = await resumed.verify("fail")
        assert failed.status == "failed" and failed.exit_code == 7

    async with Agent(**options, session_id=session_id) as inspected:
        verification = [
            (report.name, report.status)
            for report in await inspected.verification_status()
        ]
        assert verification == [("fail", "failed"), ("quick", "passed")]
    print(
        json.dumps(
            {
                "language": "python",
                "session_id": session_id,
                "replies": replies,
                "manual_approvals": approval_count,
                "saw_child": saw_child,
                "verification": verification,
            }
        )
    )


if __name__ == "__main__":
    asyncio.run(main())
