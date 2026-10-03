import { prompt } from "@kurama/sdk";

// Uses your installed binary and existing profile; approval requests cancel by default.
const reply = await prompt("Explain the main entry point without modifying files.");
console.log(reply.text);
// Pass sessionId to a later prompt() or Agent.open() to resume.
console.log("Session:", reply.sessionId);
