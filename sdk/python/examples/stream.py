"""Manual approvals and deterministic early-exit cleanup."""

import asyncio

from kurama import Agent


async def main() -> None:
    async with Agent() as agent:
        # Native async-for does not call aclose when a loop breaks. The stream
        # context manager cancels/drains on break or errors; agent exit also
        # closes the process. Alternatively call await events.aclose() explicitly.
        async with agent.stream(
            "Review the current changes; do not edit files."
        ) as events:
            async for event in events:
                if event.type == "text":
                    print(event.text, end="", flush=True)
                elif event.type == "approval":
                    answer = await asyncio.to_thread(
                        input, f"\n{event.request.summary}\nApprove once? [y/N] "
                    )
                    await agent.approve(
                        event.request.operation_id,
                        "approve_once" if answer.lower() == "y" else "deny",
                    )
                elif event.type == "done":
                    print(f"\nTurn {event.status}")
                    if event.error is not None:
                        print(event.error.code, event.error.message)


if __name__ == "__main__":
    asyncio.run(main())
