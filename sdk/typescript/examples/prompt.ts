import { Agent, ApprovalRequired } from "@kurama/sdk";

// Uses the installed Kurama binary and your existing profile/credential configuration.
const agent = await Agent.open({ workspace: process.cwd() });
try {
  const reply = await agent.prompt("Explain the main entry point without modifying files.");
  console.log(reply.text);
  // Persist this ID if you want Agent.open({ sessionId }) in a later process.
  console.log("Session:", reply.sessionId);
} catch (error) {
  if (error instanceof ApprovalRequired) {
    console.error("Cancelled an operation requiring approval:", error.request.summary);
    // The agent remains usable. Use the streaming example for interactive approval.
  } else throw error;
} finally {
  await agent.close();
}
