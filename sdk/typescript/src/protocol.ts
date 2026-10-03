import { IncompatibleProtocolError, ProtocolError } from "./errors.js";
import type { AgentEvent, ApprovalResponse, ErrorDetail, VerificationReport } from "./types.js";

export const MAX_FRAME_BYTES = 1_048_576;
export type RecordValue = Record<string, unknown>;
export interface Hello {
  type: "hello";
  protocol_version: number;
  server_version: string;
  max_frame_bytes: number;
  capabilities: string[];
}
export type Frame = Hello
  | { type: "response"; id: string | null; result?: RecordValue; error?: ErrorDetail }
  | { type: "event"; request_id: string | null; session_id: string; event: AgentEvent };

export function object(value: unknown): value is RecordValue {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function string(value: unknown): value is string { return typeof value === "string"; }
function boolean(value: unknown): value is boolean { return typeof value === "boolean"; }
function integer(value: unknown): value is number { return typeof value === "number" && Number.isInteger(value); }
function unsigned(value: unknown): value is number { return integer(value) && value >= 0; }
function strings(value: unknown): value is string[] { return Array.isArray(value) && value.every(string); }
function nullable(value: unknown, check: (v: unknown) => boolean): boolean { return value === null || check(value); }
function detail(value: unknown): value is ErrorDetail {
  return object(value) && string(value.code) && string(value.message);
}
function blobs(value: unknown): boolean {
  return Array.isArray(value) && value.every(v => object(v) && string(v.sha256) && unsigned(v.bytes));
}
function operation(value: unknown): boolean {
  if (!object(value)) return false;
  switch (value.type) {
    case "read": return strings(value.paths) && boolean(value.external);
    case "write": return strings(value.paths) && boolean(value.external) && boolean(value.destructive);
    case "bash": return string(value.command) && string(value.cwd) && unsigned(value.timeout_ms)
      && ["read_only", "mutating", "unknown"].includes(String(value.class));
    case "web_search": return string(value.query) && boolean(value.contains_workspace_data);
    case "web_open": return string(value.url) && boolean(value.private_target);
    default: return false;
  }
}
export function verificationReport(value: unknown): value is VerificationReport {
  return object(value) && string(value.name) && string(value.command) && string(value.cwd)
    && unsigned(value.timeout_ms)
    && ["not_run", "running", "passed", "failed", "cancelled", "denied", "interrupted"].includes(String(value.status))
    && nullable(value.operation_id, string) && nullable(value.started_at_ms, unsigned)
    && nullable(value.finished_at_ms, unsigned) && nullable(value.exit_code, integer)
    && blobs(value.output_refs) && nullable(value.message, string);
}
function event(value: unknown): value is AgentEvent {
  if (!object(value)) return false;
  switch (value.type) {
    case "text": return string(value.text);
    case "status": return string(value.message);
    case "approval": return object(value.request) && string(value.request.operation_id)
      && string(value.request.summary) && operation(value.request.operation) && "arguments" in value.request;
    case "tool_started": return string(value.operation_id) && string(value.name) && string(value.context);
    case "tool_output": return string(value.call_id) && string(value.stream) && string(value.chunk);
    case "tool_completed": return string(value.operation_id) && object(value.result)
      && string(value.result.call_id) && string(value.result.output) && boolean(value.result.is_error)
      && boolean(value.result.truncated) && blobs(value.result.blob_refs) && "metadata" in value.result;
    case "agent_updated": {
      const v = value.snapshot;
      return object(v) && string(v.id) && string(v.role) && string(v.objective) && string(v.profile)
        && ["queued", "running", "completed", "failed", "cancelled"].includes(String(v.state))
        && nullable(v.phase, string) && nullable(v.active_operation, string)
        && strings(v.changed_files) && nullable(v.last_error, string);
    }
    case "usage": return object(value.usage) && unsigned(value.usage.input_tokens)
      && unsigned(value.usage.output_tokens) && unsigned(value.usage.cached_input_tokens);
    case "verification": return verificationReport(value.report);
    case "runtime": return "event" in value;
    case "done": return ["completed", "cancelled", "failed"].includes(String(value.status))
      && (value.error === undefined || detail(value.error));
    default: return false;
  }
}
export function approvalResponse(value: unknown): value is ApprovalResponse {
  return value === "approve_once" || value === "approve_session" || value === "deny"
    || (object(value) && Object.keys(value).length === 1 && object(value.edit)
      && Object.keys(value.edit).length === 1 && "arguments" in value.edit);
}
export function parseFrame(bytes: Uint8Array): Frame {
  let value: unknown;
  try { value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)); }
  catch { throw new ProtocolError(); }
  if (!object(value)) throw new ProtocolError();
  if (value.type === "hello") {
    if (value.protocol_version !== 1) throw new IncompatibleProtocolError();
    const capabilities = value.capabilities;
    if (!string(value.server_version) || value.max_frame_bytes !== MAX_FRAME_BYTES || !strings(capabilities)
      || !["prompt", "stream", "approval", "cancel", "resume", "verify"].every(c => capabilities.includes(c))) {
      throw new IncompatibleProtocolError();
    }
    return value as unknown as Hello;
  }
  if (value.type === "response" && (value.id === null || (string(value.id) && /^[1-9][0-9]*$/.test(value.id)))) {
    if (detail(value.error) && !("result" in value)) return value as unknown as Frame;
    if (object(value.result) && !("error" in value) && value.id !== null) return value as unknown as Frame;
  }
  if (value.type === "event" && (value.request_id === null || (string(value.request_id) && /^[1-9][0-9]*$/.test(value.request_id)))
    && string(value.session_id) && event(value.event)) return value as unknown as Frame;
  throw new ProtocolError();
}

/** One fixed-size buffer avoids quadratic copying for arbitrarily fragmented frames. */
export class FrameDecoder {
  private readonly buffer = Buffer.allocUnsafe(MAX_FRAME_BYTES);
  private length = 0;
  constructor(private readonly accept: (frame: Frame, bytes: number) => void) {}
  push(chunk: Buffer): void {
    let offset = 0;
    while (offset < chunk.length) {
      const lf = chunk.indexOf(10, offset);
      const end = lf < 0 ? chunk.length : lf;
      const count = end - offset;
      if (this.length + count > MAX_FRAME_BYTES) throw new ProtocolError("The Kurama process exceeded the 1 MiB frame limit.");
      if (this.length === 0 && lf >= 0) {
        this.accept(parseFrame(chunk.subarray(offset, end)), count);
      } else {
        chunk.copy(this.buffer, this.length, offset, end);
        this.length += count;
        if (lf >= 0) {
          const size = this.length;
          this.length = 0;
          this.accept(parseFrame(this.buffer.subarray(0, size)), size);
        }
      }
      if (lf < 0) break;
      offset = lf + 1;
    }
  }
  end(): void {
    if (this.length !== 0) throw new ProtocolError("The Kurama process closed stdout in the middle of a frame.");
  }
}
