"""Install kurama-sdk and a Kurama binary supporting --stdio, then run this file."""

import asyncio

from kurama import Agent, ApprovalRequest, ApprovalResponse


def review(request: ApprovalRequest) -> ApprovalResponse:
    # Application policy belongs here. No raw protocol or process plumbing needed.
    # Deny by default; adapt this callback to your application's approval UI.
    print(f"Approval requested: {request.summary}")
    return "deny"


async def main() -> None:
    # Omit profile to use the default in your existing Kurama configuration.
    async with Agent(on_approval=review) as agent:
        reply = await agent.prompt(
            "Summarize this project's architecture without changing files."
        )
        print(reply.text)
        session_id = agent.session_id

    # Resume the same persisted session using the same workspace and profile.
    async with Agent(session_id=session_id, on_approval=review) as resumed:
        print((await resumed.prompt("What should I read first?")).text)
        for report in await resumed.verification_status():
            print(report.name, report.command, report.status)
        # To explicitly execute a configured .kurama/verification.toml recipe:
        # report = await resumed.verify("quick")


if __name__ == "__main__":
    asyncio.run(main())
