import { createInterface } from "node:readline/promises";
import { Agent } from "@kurama/sdk";

const terminal = createInterface({ input: process.stdin, output: process.stdout });
const agent = await Agent.open({
  onApproval: async request => {
    const answer = await terminal.question(`${request.summary}\nApprove once? [y/N] `);
    return answer.toLowerCase() === "y" ? "approve_once" : "deny";
  },
});
try {
  // Reads configured .kurama/verification.toml recipes; does not execute them.
  for (const report of await agent.verificationStatus()) {
    console.log(report.name, report.status, report.command);
  }
  const name = process.argv[2];
  if (name) {
    // Runs exactly this named recipe without contacting a model.
    const report = await agent.verify(name);
    console.log(report.status, report.command, report.cwd, report.exit_code);
    // A previous pass describes the last run, not the current working tree.
  }
} finally {
  terminal.close();
  await agent.close();
}
