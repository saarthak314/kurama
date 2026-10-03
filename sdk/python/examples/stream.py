"""Manual approvals and deterministic early-exit cleanup."""

import asyncio

from kurama import Agent


async def main() -> None:
    async with Agent() as agent:
        # The stream context cancels and drains if iteration exits early.
        async with agent.stream(
            "Review the current changes; do not edit files."
        ) as events:
            async for event in events:
                if event.type == "text":
                    print(event.text, end="", flush=True)
                elif event.type == "approval":
                    await agent.approve(event.request.operation_id, "deny")
                elif event.type == "done":
                    print(f"\nTurn {event.status}")
                    if event.error is not None:
                        print(event.error.code, event.error.message)


if __name__ == "__main__":
    asyncio.run(main())
