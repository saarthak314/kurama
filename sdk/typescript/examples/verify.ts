import { verify } from "@kurama/sdk";

// approve: true explicitly trusts the selected recipe; Rust policy constraints still apply.
const report = await verify(process.argv[2] ?? "quick", { approve: true });
console.log(report.status, report.command, report.cwd, report.exit_code);
// A previous pass describes the last run, not the current working tree.
