"""Use the default profile in your existing Kurama configuration."""

import asyncio

from kurama import prompt


async def main() -> None:
    reply = await prompt(
        "Summarize this project's architecture without changing files."
    )
    print(reply.text)


if __name__ == "__main__":
    asyncio.run(main())
