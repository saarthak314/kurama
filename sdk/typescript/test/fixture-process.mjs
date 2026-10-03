import { readFile, writeFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { spawn } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";

const fixtures = JSON.parse(await readFile(new URL("../../../protocol/sdk.fixtures.json", import.meta.url), "utf8"));
const frames = fixtures.frames;
const scenario = process.env.SDK_SCENARIO ?? "normal";
const statePath = process.env.SDK_STATE;
let active;
let recovery;
let session = frames.initialize_response.result.session_id;
let previousId = 0n;
let lastReport = frames.verification_status_response.result.recipes[0];

async function bytes(buffer) {
  if (!process.stdout.write(buffer)) await once(process.stdout, "drain");
}
async function send(frame, fragmented = false) {
  const buffer = Buffer.from(JSON.stringify(frame) + "\n");
  if (fragmented) {
    for (let i = 0; i < buffer.length; i += 3) {
      await bytes(buffer.subarray(i, i + 3));
      await delay(1);
    }
  } else await bytes(buffer);
}
async function response(id, result) { await send({ type: "response", id, result }); }
async function event(payload, id = active?.id ?? null, fragmented = false) {
  await send({ type: "event", request_id: id, session_id: session, event: payload }, fragmented);
}
async function complete(payload = frames.done_event.event) {
  await event(payload);
  active = undefined;
}
async function finishApproval(responseValue) {
  if (recovery) {
    const id = recovery;
    recovery = undefined;
    await response(id, { ...frames.initialize_response.result, session_id: session });
    return;
  }
  if (responseValue === "deny") {
    await complete(frames.cancelled_event.event);
    return;
  }
  for (const name of ["tool_started_event", "tool_output_event", "tool_completed_event", "usage_event"]) {
    await event(frames[name].event);
  }
  await event(frames.text_event.event);
  await complete();
}

await writeFile(process.env.SDK_PID, String(process.pid));
if (scenario === "legacy") {
  process.stderr.write("old binary: unsupported --stdio; secret-from-stderr\n");
  process.exit(2);
}
if (scenario === "silent") {
  setInterval(() => {}, 1000);
} else {
  await send(scenario === "version" ? { ...frames.hello, protocol_version: 999 } : frames.hello, scenario === "fragment");
}
for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
  const request = JSON.parse(line);
  if (!/^[1-9][0-9]*$/.test(request.id) || BigInt(request.id) <= previousId) process.exit(20);
  previousId = BigInt(request.id);
  const { id, method, params } = request;
  if (method === "initialize") {
    if (params.protocol_version !== 1 || (params.mode === "yolo" && !process.argv.includes("--yolo"))) process.exit(21);
    if (params.session_id && params.session_id !== session) {
      await send({ type: "response", id, error: { code: "unknown_session", message: "Unknown session" } });
      continue;
    }
    if (!params.session_id) await writeFile(statePath, "");
    if (scenario === "recovery") {
      recovery = id;
      await event(frames.approval_event.event, id);
    } else await response(id, { ...frames.initialize_response.result, workspace: params.workspace, mode: params.mode ?? "supervised" });
  } else if (method === "prompt") {
    if (active) {
      await send({ type: "response", id, error: { code: "busy", message: "Busy" } });
      continue;
    }
    active = { id, text: params.text };
    await response(id, frames.prompt_response.result);
    if (params.text === "hold" || params.text === "tree") {
      if (params.text === "tree") {
        const descendant = spawn(process.execPath, ["-e", "process.on('SIGTERM',()=>{});setInterval(()=>{},1000)"], { stdio: "inherit" });
        await writeFile(process.env.SDK_DESCENDANT, String(descendant.pid));
      }
      await event({ type: "text", text: "started" });
    } else if (params.text === "approval") await event(frames.approval_event.event);
    else if (params.text === "remember") {
      await writeFile(statePath, "remembered across a process boundary");
      await complete();
    } else if (params.text === "recall") {
      await event({ type: "text", text: await readFile(statePath, "utf8") });
      await complete();
    } else if (params.text === "exit") process.exit(17);
    else if (params.text === "truncated") {
      await bytes(Buffer.from('{"type":"event"'));
      process.stdout.end();
    } else if (params.text === "oversized") await bytes(Buffer.alloc(1_048_577, 120));
    else if (params.text === "invalid-utf8") await bytes(Buffer.from([0xff, 10]));
    else if (params.text.startsWith("invalid:")) {
      await bytes(Buffer.from(fixtures.invalid_lines[Number(params.text.split(":")[1])].line));
    } else if (params.text === "unknown-event") await event({ type: "unknown_future_event" });
    else if (params.text === "wrong-session") await send({ ...frames.text_event, request_id: id, session_id: "ses_other" });
    else if (params.text === "failed") {
      await event(frames.text_event.event);
      await complete(frames.failed_event.event);
    } else if (params.text === "flood") {
      await event({ type: "text", text: "started" });
      await delay(25);
      for (let n = 0; n < 160; n++) await event({ type: "text", text: String(n) });
      await writeFile(process.env.SDK_FLOODED, "done");
      await complete();
    } else if (params.text === "unsolicited") {
      await complete();
      await event({ type: "status", message: "idle update" }, null);
    } else {
      await event(frames.text_event.event, id, scenario === "fragment");
      await event(frames.usage_event.event);
      await complete();
    }
  } else if (method === "approve") {
    if ((!active && !recovery) || params.operation_id !== frames.approval_event.event.request.operation_id) {
      await send({ type: "response", id, error: { code: "unknown_operation", message: "No pending approval" } });
    } else {
      await response(id, frames.approve_response.result);
      await finishApproval(params.response);
    }
  } else if (method === "cancel") {
    const wasActive = Boolean(active || recovery);
    await response(id, { cancelled: wasActive });
    if (active) {
      await event({ type: "text", text: " drained" });
      await complete(frames.cancelled_event.event);
    }
    if (recovery) {
      await send({ type: "response", id: recovery, error: { code: "cancelled", message: "Recovery cancelled" } });
      recovery = undefined;
    }
  } else if (method === "verification_status") {
    if (scenario !== "hang-status") await response(id, { recipes: [lastReport] });
  } else if (method === "verify") {
    if (params.name !== "quick" && params.name !== "failed") {
      await send({ type: "response", id, error: { code: "unknown_recipe", message: "Unknown verification recipe" } });
    } else {
      active = { id };
      await response(id, { accepted: true });
      lastReport = { ...frames.verification_event.event.report, status: params.name === "failed" ? "failed" : "passed", exit_code: params.name === "failed" ? 1 : 0 };
      await event({ type: "verification", report: { ...lastReport, status: "running", finished_at_ms: null, exit_code: null } });
      await event({ type: "verification", report: lastReport });
      await complete();
    }
  } else if (method === "shutdown") {
    if (scenario === "stuck") {
      process.on("SIGTERM", () => {});
      continue;
    }
    await response(id, frames.shutdown_response.result);
    process.stdout.end(() => process.exit(0));
    break;
  } else process.exit(22);
}
