import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const { Agent, ApprovalRequired } = await import(pathToFileURL(process.env.KURAMA_SDK_MODULE).href);
const workspace = process.env.SDK_WORKSPACE;
const options = {
  workspace,
  profile: "fixture",
  mode: "supervised",
  binary: process.env.KURAMA_BIN,
  stateDir: process.env.SDK_STATE,
  env: { SDK_CANARY_SECRET: "sdk-secret-must-not-appear-in-protocol" },
};
const agent = await Agent.open(options);
const sessionId = agent.sessionId;
const replies = [];
let approvalCount = 0;
let sawChild = false;
try {
  const simple = await agent.prompt("SDK_SIMPLE");
  assert.equal(simple.text, "SDK_SIMPLE_OK");
  assert.equal(simple.sessionId, sessionId);
  replies.push(simple.text);

  let streamed = "";
  for await (const event of agent.stream("SDK_STREAM")) {
    if (event.type === "text") streamed += event.text;
  }
  assert.equal(streamed, "stream 世界\nfinished");
  replies.push(streamed);

  let written = "";
  for await (const event of agent.stream("SDK_WRITE")) {
    if (event.type === "approval") {
      approvalCount++;
      await agent.approve(event.request.operation_id);
    }
    if (event.type === "text") written += event.text;
  }
  assert.equal(approvalCount, 1);
  assert.equal(written, "SDK_WRITE_DONE");
  assert.equal(await readFile(join(workspace, "sdk-result.txt"), "utf8"), "written\n");
  replies.push(written);

  await assert.rejects(agent.prompt("SDK_NEEDS_APPROVAL"), (error) => {
    assert.ok(error instanceof ApprovalRequired);
    assert.equal(error.code, "approval_required");
    assert.ok(error.request.operation_id);
    return true;
  });

  let cancelled = false;
  let terminal;
  for await (const event of agent.stream("SDK_CANCEL")) {
    if (event.type === "text" && event.text.includes("SDK_CANCEL_BEGIN") && !cancelled) {
      await agent.cancel();
      cancelled = true;
    }
    if (event.type === "done") terminal = event.status;
  }
  assert.ok(cancelled);
  assert.equal(terminal, "cancelled");
  const after = await agent.prompt("SDK_AFTER_CANCEL");
  assert.equal(after.text, "SDK_AFTER_CANCEL_OK");
  replies.push(after.text);

  let delegated = "";
  for await (const event of agent.stream("SDK_AGENTS", { explicitDelegation: true })) {
    if (event.type === "agent_updated" && event.snapshot.state === "completed") sawChild = true;
    if (event.type === "text") delegated += event.text;
  }
  assert.ok(sawChild);
  assert.equal(delegated, "SDK_AGENTS_DONE");
  replies.push(delegated);
} finally {
  await agent.close();
}

const resumed = await Agent.open({
  ...options,
  sessionId,
  onApproval: async () => "approve_once",
});
try {
  assert.equal(resumed.sessionId, sessionId);
  const reply = await resumed.prompt("SDK_RESUME");
  assert.equal(reply.text, "SDK_RESUME_OK");
  replies.push(reply.text);
  const initial = await resumed.verificationStatus();
  assert.deepEqual(initial.map((report) => [report.name, report.status]), [["fail", "not_run"], ["quick", "not_run"]]);
  const passed = await resumed.verify("quick");
  assert.equal(passed.status, "passed");
  assert.equal(passed.exit_code, 0);
  assert.ok(passed.operation_id);
  assert.equal(await readFile(join(workspace, "verified.txt"), "utf8"), "verified\n");
  const failed = await resumed.verify("fail");
  assert.equal(failed.status, "failed");
  assert.equal(failed.exit_code, 7);
} finally {
  await resumed.close();
}

const inspected = await Agent.open({ ...options, sessionId });
let verification;
try {
  verification = (await inspected.verificationStatus()).map((report) => [report.name, report.status]);
  assert.deepEqual(verification, [["fail", "failed"], ["quick", "passed"]]);
} finally {
  await inspected.close();
}
console.log(JSON.stringify({ language: "typescript", session_id: sessionId, replies, manual_approvals: approvalCount, saw_child: sawChild, verification }));
