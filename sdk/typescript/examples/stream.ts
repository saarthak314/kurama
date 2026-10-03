import { createInterface } from "node:readline/promises";
import { stdin, stdout } from "node:process";
import { Agent } from "@kurama/sdk";

const agent = await Agent.open();
const terminal = createInterface({ input: stdin, output: stdout });
const cancel = () => { void agent.cancel().catch(error => console.error(error)); };
process.on("SIGINT", cancel);
try {
  for await (const event of agent.stream("Inspect the workspace and suggest a small improvement.")) {
    if (event.type === "text") stdout.write(event.text);
    if (event.type === "approval") {
      const answer = await terminal.question(`\n${event.request.summary}\nApprove once? [y/N] `);
      await agent.approve(event.request.operation_id, answer.toLowerCase() === "y" ? "approve_once" : "deny");
    }
    if (event.type === "done") {
      console.log(`\n${event.status}`);
      if (event.error) console.error(event.error.message);
    }
  }
  // Breaking the for-await loop also cancels/drains its execution before returning.
} finally {
  process.off("SIGINT", cancel);
  terminal.close();
  await agent.close();
}
