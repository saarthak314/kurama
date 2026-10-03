import type { ApprovalRequest, Reply } from "./types.js";

const messages: Record<string, string> = {
  invalid_protocol: "The Kurama process sent an invalid protocol frame.",
  incompatible_protocol: "Install a Kurama binary supporting stdio protocol version 1, or set binary / KURAMA_BIN to that binary.",
  process_error: "The Kurama process exited unexpectedly.",
  closed: "This Kurama agent is closed.",
  busy: "Finish or cancel the current stream before starting another execution.",
  backpressure: "The event consumer fell behind the bounded buffer. The agent was closed; consume events promptly or resume in a new agent.",
  approval_required: "This operation requires approval. Supply approve: true or an approval callback, or use stream() and approve(). The operation was cancelled.",
  runtime_error: "The Kurama execution failed.",
};

export class KuramaError extends Error {
  readonly reply: Reply | undefined;
  readonly exitCode: number | null | undefined;
  readonly signal: string | null | undefined;

  constructor(
    public readonly code: string,
    message?: string,
    public readonly details?: { reply?: Reply; exitCode?: number | null; signal?: string | null },
  ) {
    super(message ?? (Object.hasOwn(messages, code) ? messages[code] : "The Kurama operation failed."));
    this.name = new.target.name;
    this.reply = details?.reply;
    this.exitCode = details?.exitCode;
    this.signal = details?.signal;
  }
}

export class ApprovalRequired extends KuramaError {
  constructor(public readonly request: ApprovalRequest) {
    super("approval_required");
  }
}
