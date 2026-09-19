import type { ApprovalRequest, ErrorDetail, Reply } from "./types.js";

export class KuramaError extends Error {
  constructor(public readonly code: string, message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}
/** A structured rejection from Kurama, distinct from transport or user callback failures. */
export class ServerError extends KuramaError {}
/** A failed execution; prompt failures retain the partial reply received before the failure. */
export class TurnFailed extends ServerError {
  constructor(error?: ErrorDetail, public readonly reply?: Reply) {
    super(error?.code ?? "runtime_error", error?.message ?? "The Kurama execution failed.");
  }
}
export class ProtocolError extends KuramaError {
  constructor(message = "The Kurama process sent an invalid protocol frame.") {
    super("invalid_protocol", message);
  }
}
export class IncompatibleProtocolError extends KuramaError {
  constructor() {
    super("incompatible_protocol", "Install a Kurama binary supporting stdio protocol version 1, or set the binary option / KURAMA_BIN to that binary.");
  }
}
export class ProcessError extends KuramaError {
  constructor(message: string, public readonly exitCode: number | null = null, public readonly signal: string | null = null) {
    super("process_exited", message);
  }
}
export class ClosedError extends KuramaError {
  constructor() { super("closed", "This Kurama agent is closed."); }
}
export class BusyError extends KuramaError {
  constructor() { super("busy", "Finish or cancel the current stream before starting another execution."); }
}
export class BackpressureError extends KuramaError {
  constructor() {
    super("backpressure", "The event consumer fell behind the bounded buffer. The agent was closed; consume events promptly or resume in a new agent.");
  }
}
export class ApprovalRequired extends KuramaError {
  constructor(public readonly request: ApprovalRequest) {
    super("approval_required", "This operation requires approval. Use stream() and approve(), or supply onApproval when opening the agent. The operation was cancelled.");
  }
}
export class ApprovalCallbackError extends KuramaError {
  constructor(cause: unknown) {
    super("approval_callback_failed", "The onApproval callback failed. The operation was cancelled.", { cause });
  }
}
