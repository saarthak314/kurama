import { Agent } from "@kurama/sdk";

await using agent = await Agent.open();
for await (const event of agent.stream("Inspect the workspace and suggest a small improvement.")) {
  if (event.type === "text") process.stdout.write(event.text);
  if (event.type === "approval") {
    console.error("Denied:", event.request.summary);
    await agent.approve(event.request.operation_id, "deny");
  }
  if (event.type === "done") {
    console.log(`\n${event.status}`);
    if (event.error) console.error(event.error.message);
  }
}
// Breaking the for-await loop also cancels/drains its execution before returning.
