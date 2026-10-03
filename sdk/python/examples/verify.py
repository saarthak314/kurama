"""Run a named recipe from .kurama/verification.toml."""

import asyncio

from kurama import verify


async def main() -> None:
    # approve=True is opt-in trust in the selected recipe, not a policy bypass.
    report = await verify("quick", approve=True)
    print(report.name, report.status, report.exit_code)


if __name__ == "__main__":
    asyncio.run(main())
