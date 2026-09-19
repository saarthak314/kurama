export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Mode = "supervised" | "auto" | "yolo";
export type TurnStatus = "completed" | "cancelled" | "failed";
export interface ErrorDetail { code: string; message: string }

export type Operation =
  | { type: "read"; paths: string[]; external: boolean }
  | { type: "write"; paths: string[]; destructive: boolean; external: boolean }
  | { type: "bash"; command: string; cwd: string; class: "read_only" | "mutating" | "unknown"; timeout_ms: number }
  | { type: "web_search"; query: string; contains_workspace_data: boolean }
  | { type: "web_open"; url: string; private_target: boolean };

export interface ApprovalRequest {
  operation_id: string;
  operation: Operation;
  summary: string;
  arguments: Json;
}
export type ApprovalResponse = "approve_once" | "approve_session" | "deny" | { edit: { arguments: Json } };
export type ApprovalHandler = (request: ApprovalRequest) => ApprovalResponse | Promise<ApprovalResponse>;
export interface BlobRef { sha256: string; bytes: number }
export interface ToolResult {
  call_id: string;
  output: string;
  is_error: boolean;
  metadata: Json;
  truncated: boolean;
  blob_refs: BlobRef[];
}
export interface AgentSnapshot {
  id: string;
  role: string;
  objective: string;
  profile: string;
  state: "queued" | "running" | "completed" | "failed" | "cancelled";
  phase: string | null;
  active_operation: string | null;
  changed_files: string[];
  last_error: string | null;
}
export interface Usage { input_tokens: number; output_tokens: number; cached_input_tokens: number }
export type VerificationStatus = "not_run" | "running" | "passed" | "failed" | "cancelled" | "denied" | "interrupted";
export interface VerificationReport {
  name: string;
  command: string;
  cwd: string;
  timeout_ms: number;
  status: VerificationStatus;
  operation_id: string | null;
  started_at_ms: number | null;
  finished_at_ms: number | null;
  exit_code: number | null;
  output_refs: BlobRef[];
  message: string | null;
}

/** Friendly events retain Rust's typed payloads; no wire envelopes or request IDs are needed. */
export type AgentEvent =
  | { type: "text"; text: string }
  | { type: "approval"; request: ApprovalRequest }
  | { type: "tool_started"; operation_id: string; name: string; context: string }
  | { type: "tool_output"; call_id: string; stream: string; chunk: string }
  | { type: "tool_completed"; operation_id: string; result: ToolResult }
  | { type: "agent_updated"; snapshot: AgentSnapshot }
  | { type: "usage"; usage: Usage }
  | { type: "status"; message: string }
  | { type: "verification"; report: VerificationReport }
  | { type: "runtime"; event: Json }
  | { type: "done"; status: TurnStatus; error?: ErrorDetail };

export interface Reply { text: string; sessionId: string; status: TurnStatus }
export interface PromptOptions { explicitDelegation?: boolean }
export interface AgentOptions {
  workspace?: string;
  profile?: string;
  mode?: Mode;
  binary?: string;
  stateDir?: string;
  sessionId?: string;
  /** Child environment overrides. Values are never sent in protocol frames or logged. */
  env?: Record<string, string | undefined>;
  onApproval?: ApprovalHandler;
  /** Maximum time for the installed binary to send its initial hello (default 10 seconds). */
  startupTimeoutMs?: number;
  /** Maximum graceful shutdown/cancellation drain time (default 2 seconds). */
  shutdownTimeoutMs?: number;
}
