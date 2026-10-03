import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, rm, writeFile, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { Agent, prompt, verify, ApprovalRequired, KuramaError } from "../dist/index.js";

const fixtures = JSON.parse(await readFile(new URL("../../../protocol/sdk.fixtures.json", import.meta.url), "utf8"));
const frames = fixtures.frames;
const limits = { timeout: 10_000 };

async function setup(t, scenario = "normal") {
  const root = await mkdtemp(join(tmpdir(), "kurama-ts-"));
  const binary = join(root, "kurama-test");
  const agents = [];
  await writeFile(binary, `#!/usr/bin/env node\nimport ${JSON.stringify(new URL("./fixture-process.mjs", import.meta.url).href)};\n`, { mode: 0o700 });
  const env = {
    PATH: `${dirname(process.execPath)}:${process.env.PATH ?? ""}`,
    SDK_SCENARIO: scenario,
    SDK_PID: join(root, "pid"),
    SDK_STATE: join(root, "state"),
    SDK_DESCENDANT: join(root, "descendant"),
    SDK_FLOODED: join(root, "flooded"),
  };
  t.after(async () => {
    await Promise.all(agents.map(agent => agent.close()));
    await rm(root, { recursive: true, force: true });
  });
  return {
    root, binary, env, options: { workspace: root, binary, env },
    async open(options = {}) {
      const agent = await Agent.open({ workspace: root, binary, env, ...options });
      agents.push(agent);
      return agent;
    },
  };
}

async function waitForExit(pid) {
  for (let n = 0; n < 200; n++) {
    try { process.kill(pid, 0); }
    catch (error) { if (error.code === "ESRCH") return; throw error; }
    await delay(10);
  }
  assert.fail(`Owned subprocess ${pid} did not exit`);
}

async function assertProcessClosed(harness) {
  const pid = Number(await readFile(harness.env.SDK_PID, "utf8"));
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
}

async function collect(iterable) {
  const events = [];
  for await (const event of iterable) events.push(event);
  return events;
}

test("shared fixture text survives fragmented UTF-8 and consecutive frames", limits, async t => {
  const harness = await setup(t, "fragment");
  const agent = await harness.open();
  assert.equal(agent.sessionId, frames.initialize_response.result.session_id);
  assert.deepEqual(await agent.prompt("hello"), {
    text: frames.text_event.event.text, sessionId: agent.sessionId, status: "completed",
  });
  assert.deepEqual(await collect(agent.stream("hello")), [
    frames.text_event.event, frames.usage_event.event, frames.done_event.event,
  ]);
});

test("manual approval exposes the shared typed request and preserves all lifecycle events", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  const observed = [];
  for await (const event of agent.stream("approval")) {
    observed.push(event);
    if (event.type === "approval") {
      assert.deepEqual(event.request, frames.approval_event.event.request);
      await agent.approve(event.request.operation_id);
    }
  }
  assert.deepEqual(observed, ["approval_event", "tool_started_event", "tool_output_event", "tool_completed_event", "usage_event", "text_event", "done_event"].map(name => frames[name].event));
});

test("prompt missing approval cancels and drains before reuse", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  await assert.rejects(agent.prompt("approval"), error => {
    assert.ok(error instanceof ApprovalRequired);
    assert.deepEqual(error.request, frames.approval_event.event.request);
    return true;
  });
  assert.equal((await agent.prompt("hello")).text, frames.text_event.event.text);
  assert.equal(await agent.cancel(), false);
});

test("async callbacks approve or deny without exposing protocol requests", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open({ approve: async request => {
    assert.equal(request.operation.type, "bash");
    return "approve_once";
  } });
  assert.equal((await agent.prompt("approval")).text, frames.text_event.event.text);
  const denied = await harness.open({ approve: () => "deny" });
  assert.equal((await denied.prompt("approval")).status, "cancelled");
});

test("callback exceptions propagate unchanged and leave the agent reusable", limits, async t => {
  const harness = await setup(t);
  const cause = new Error("user callback failure");
  const agent = await harness.open({ approve: () => { throw cause; } });
  await assert.rejects(agent.prompt("approval"), error => {
    assert.equal(error, cause);
    return true;
  });
  assert.equal((await agent.prompt("hello")).status, "completed");
});

test("cancel drains only its execution and busy checks do not consume the next turn", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  const stream = agent.stream("hold");
  assert.equal((await stream.next()).value.text, "started");
  await assert.rejects(agent.prompt("overlap"), { name: "KuramaError", code: "busy" });
  await assert.rejects(agent.verify("quick"), { name: "KuramaError", code: "busy" });
  assert.equal((await agent.verificationStatus())[0].status, "not_run");
  assert.equal(await agent.cancel(), true);
  assert.deepEqual(await collect(stream), [{ type: "text", text: " drained" }, frames.cancelled_event.event]);
  assert.equal(await agent.cancel(), false);
  assert.equal((await agent.prompt("next")).text, frames.text_event.event.text);
});

test("breaking native iteration cancels and drains before the next prompt", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  for await (const event of agent.stream("hold")) {
    assert.equal(event.text, "started");
    break;
  }
  assert.equal((await agent.prompt("next")).text, frames.text_event.event.text);
});

test("resume spans two owned processes rather than replaying an in-memory conversation", limits, async t => {
  const harness = await setup(t);
  const first = await harness.open();
  await first.prompt("remember");
  const sessionId = first.sessionId;
  const pid = Number(await readFile(harness.env.SDK_PID, "utf8"));
  await first.close();
  await waitForExit(pid);
  const resumed = await harness.open({ sessionId });
  assert.equal((await resumed.prompt("recall")).text, "remembered across a process boundary");
});

test("recovery approval fails safely by default; explicit approval resumes", limits, async t => {
  const harness = await setup(t, "recovery");
  await assert.rejects(harness.open(), ApprovalRequired);
  await waitForExit(Number(await readFile(harness.env.SDK_PID, "utf8")));
  const agent = await harness.open({ approve: true });
  const recovered = await agent.events()[Symbol.asyncIterator]().next();
  assert.deepEqual(recovered.value, frames.approval_event.event);
  assert.equal((await agent.prompt("hello")).status, "completed");
});

test("verification reports distinguish failed checks from protocol errors", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open({ approve: true });
  assert.deepEqual(await agent.verificationStatus(), frames.verification_status_response.result.recipes);
  assert.deepEqual(await agent.verify("quick"), frames.verification_event.event.report);
  assert.equal((await agent.verificationStatus())[0].status, "passed");
  const failed = await agent.verify("failed");
  assert.equal(failed.status, "failed");
  assert.equal(failed.exit_code, 1);
  await assert.rejects(agent.verify("missing"), error => error instanceof KuramaError && error.code === "unknown_recipe");
  assert.equal((await agent.prompt("next")).status, "completed");
});

test("failed terminal stays visible to stream consumers and prompt retains partial reply", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  assert.deepEqual(await collect(agent.stream("failed")), [frames.text_event.event, frames.failed_event.event]);
  await assert.rejects(agent.prompt("failed"), error => {
    assert.ok(error instanceof KuramaError);
    assert.equal(error.code, frames.failed_event.event.error.code);
    assert.equal(error.reply.text, frames.text_event.event.text);
    return true;
  });
  assert.equal((await agent.prompt("next")).status, "completed");
});

test("unsolicited events are not attributed to a subsequent prompt", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  await agent.prompt("unsolicited");
  const idle = await agent.events()[Symbol.asyncIterator]().next();
  assert.deepEqual(idle.value, { type: "status", message: "idle update" });
  assert.equal((await agent.prompt("next")).text, frames.text_event.event.text);
});

for (const scenario of ["legacy", "version", "silent"]) {
  test(`incompatible ${scenario} binary fails actionably without stderr leakage`, { timeout: 20_000 }, async t => {
    const harness = await setup(t, scenario);
    await assert.rejects(harness.open(), error => {
      assert.ok(error instanceof KuramaError);
      assert.equal(error.code, "incompatible_protocol");
      assert.match(error.message, /binary|protocol/);
      assert.ok(!error.message.includes("secret-from-stderr"));
      return true;
    });
    await waitForExit(Number(await readFile(harness.env.SDK_PID, "utf8")));
  });
}

for (const prompt of ["truncated", "oversized", "invalid-utf8", "unknown-event", "wrong-session", ...fixtures.invalid_lines.map((_, n) => `invalid:${n}`)]) {
  test(`rejects ${prompt} framing without leaving process waiters alive`, limits, async t => {
    const harness = await setup(t);
    const agent = await harness.open();
    await assert.rejects(agent.prompt(prompt), { name: "KuramaError", code: "invalid_protocol" });
    await agent.close();
    await waitForExit(Number(await readFile(harness.env.SDK_PID, "utf8")));
  });
}

test("subprocess exit rejects both an execution and an unrelated response waiter", limits, async t => {
  const harness = await setup(t, "hang-status");
  const agent = await harness.open();
  const statusRejected = assert.rejects(agent.verificationStatus(), { name: "KuramaError", code: "process_error" });
  await assert.rejects(agent.prompt("exit"), { name: "KuramaError", code: "process_error" });
  await statusRejected;
  await agent.close();
});

test("slow consumers fail explicitly with bounded backpressure", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  const stream = agent.stream("flood");
  assert.equal((await stream.next()).value.text, "started");
  await waitForExit(Number(await readFile(harness.env.SDK_PID, "utf8")));
  await assert.rejects(stream.next(), { name: "KuramaError", code: "backpressure" });
  await agent.close();
});

test("closing releases a never-resolving approval callback and is idempotent", limits, async t => {
  const harness = await setup(t);
  let entered;
  const callbackEntered = new Promise(resolve => { entered = resolve; });
  const agent = await harness.open({ approve: () => { entered(); return new Promise(() => {}); } });
  const rejected = assert.rejects(agent.prompt("approval"), { name: "KuramaError", code: "closed" });
  await callbackEntered;
  const closing = agent.close();
  assert.equal(agent.close(), closing);
  await closing;
  await rejected;
  await assert.rejects(agent.prompt("closed"), { name: "KuramaError", code: "closed" });
});

test("bounded close kills its stuck owned process group, including descendants", { timeout: 20_000 }, async t => {
  const harness = await setup(t, "stuck");
  const agent = await harness.open();
  const stream = agent.stream("tree");
  await stream.next();
  const parent = Number(await readFile(harness.env.SDK_PID, "utf8"));
  const child = Number(await readFile(harness.env.SDK_DESCENDANT, "utf8"));
  await agent.close();
  await waitForExit(parent);
  await waitForExit(child);
  await assert.rejects(stream.next(), { name: "KuramaError", code: "closed" });
});

test("binary precedence is explicit, then KURAMA_BIN, then PATH", limits, async t => {
  const harness = await setup(t);
  const explicit = await harness.open({ env: { ...harness.env, KURAMA_BIN: "/not/a/binary" } });
  assert.equal((await explicit.prompt("hello")).status, "completed");
  const fromEnv = await harness.open({ binary: undefined, env: { ...harness.env, KURAMA_BIN: harness.binary } });
  assert.equal((await fromEnv.prompt("hello")).status, "completed");
  await symlink(harness.binary, join(harness.root, "kurama"));
  const fromPath = await harness.open({ binary: undefined, env: { ...harness.env, KURAMA_BIN: undefined, PATH: `${harness.root}:${harness.env.PATH}` } });
  assert.equal((await fromPath.prompt("hello")).status, "completed");
});

test("oversized requests fail locally without poisoning the next execution", limits, async t => {
  const harness = await setup(t);
  const agent = await harness.open();
  await assert.rejects(agent.prompt("x".repeat(1_048_576)), error => error.code === "frame_too_large");
  assert.equal((await agent.prompt("hello")).status, "completed");
});

test("one-shot prompt returns a structured reply after closing its process", limits, async t => {
  const harness = await setup(t);
  assert.deepEqual(await prompt("hello", harness.options), {
    text: frames.text_event.event.text,
    sessionId: frames.initialize_response.result.session_id,
    status: "completed",
  });
  await assertProcessClosed(harness);
});

test("one-shot sessions resume across independently owned processes", limits, async t => {
  const harness = await setup(t);
  const first = await prompt("remember", harness.options);
  await assertProcessClosed(harness);
  const resumed = await prompt("recall", { ...harness.options, sessionId: first.sessionId });
  assert.deepEqual(resumed, {
    text: "remembered across a process boundary", sessionId: first.sessionId, status: "completed",
  });
  await assertProcessClosed(harness);
});

test("one-shot approval defaults cancel safely and close both helpers", limits, async t => {
  const harness = await setup(t);
  await assert.rejects(prompt("approval", harness.options), error => {
    assert.ok(error instanceof ApprovalRequired);
    assert.deepEqual(error.request, frames.approval_event.event.request);
    return true;
  });
  await assertProcessClosed(harness);
  await assert.rejects(verify("quick", { ...harness.options, approve: false }), ApprovalRequired);
  await assertProcessClosed(harness);
});

test("one-shot explicit approval executes a recipe without approving subsequent calls", limits, async t => {
  const harness = await setup(t);
  const report = await verify("quick", { ...harness.options, approve: true });
  assert.deepEqual(report, frames.verification_event.event.report);
  await assertProcessClosed(harness);
  assert.equal((await prompt("approval", { ...harness.options, approve: true })).status, "completed");
  await assertProcessClosed(harness);
  await assert.rejects(verify("quick", harness.options), ApprovalRequired);
  await assertProcessClosed(harness);
});

test("one-shot callbacks approve prompts and can deny verification", limits, async t => {
  const harness = await setup(t);
  const reply = await prompt("approval", { ...harness.options, approve: async request => {
    assert.deepEqual(request, frames.approval_event.event.request);
    return "approve_once";
  } });
  assert.equal(reply.text, frames.text_event.event.text);
  await assertProcessClosed(harness);
  const report = await verify("quick", { ...harness.options, approve: () => "deny" });
  assert.equal(report.status, "denied");
  assert.equal(report.exit_code, null);
  await assertProcessClosed(harness);
});

test("one-shot cancellation returns a cancelled reply and closes", limits, async t => {
  const harness = await setup(t);
  const reply = await prompt("approval", { ...harness.options, approve: () => "deny" });
  assert.equal(reply.status, "cancelled");
  assert.equal(reply.sessionId, frames.initialize_response.result.session_id);
  await assertProcessClosed(harness);
});

test("one-shot callback exceptions and thrown values survive cleanup unchanged", limits, async t => {
  const harness = await setup(t);
  const failure = new Error("caller failure");
  await assert.rejects(prompt("approval", { ...harness.options, approve: () => { throw failure; } }), error => {
    assert.equal(error, failure);
    return true;
  });
  await assertProcessClosed(harness);
  await assert.rejects(verify("quick", { ...harness.options, approve: async () => { throw undefined; } }), error => {
    assert.equal(error, undefined);
    return true;
  });
  await assertProcessClosed(harness);
});

test("one-shot failures preserve partial replies and close rejected executions", limits, async t => {
  const harness = await setup(t);
  await assert.rejects(prompt("failed", harness.options), error => {
    assert.ok(error instanceof KuramaError);
    assert.equal(error.code, frames.failed_event.event.error.code);
    assert.deepEqual(error.reply, {
      text: frames.text_event.event.text, sessionId: frames.initialize_response.result.session_id, status: "failed",
    });
    return true;
  });
  await assertProcessClosed(harness);
  await assert.rejects(verify("missing", harness.options), { name: "KuramaError", code: "unknown_recipe" });
  await assertProcessClosed(harness);
});

test("one-shot transport failures close without masking protocol or process errors", limits, async t => {
  const harness = await setup(t);
  await assert.rejects(prompt("invalid-utf8", harness.options), { name: "KuramaError", code: "invalid_protocol" });
  await assertProcessClosed(harness);
  await assert.rejects(prompt("exit", harness.options), { name: "KuramaError", code: "process_error" });
  await assertProcessClosed(harness);
});

test("one-shot recovery uses the approval policy during initialization and closes on failure", limits, async t => {
  const harness = await setup(t, "recovery");
  await assert.rejects(prompt("hello", harness.options), ApprovalRequired);
  await assertProcessClosed(harness);
  const report = await verify("quick", { ...harness.options, approve: true });
  assert.equal(report.status, "passed");
  await assertProcessClosed(harness);
  const failure = new Error("recovery callback failure");
  await assert.rejects(prompt("hello", { ...harness.options, approve: () => { throw failure; } }), error => {
    assert.equal(error, failure);
    return true;
  });
  await assertProcessClosed(harness);
});
