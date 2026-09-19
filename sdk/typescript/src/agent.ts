import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import {
  ApprovalCallbackError, ApprovalRequired, BackpressureError, BusyError, ClosedError,
  IncompatibleProtocolError, KuramaError, ProcessError, ProtocolError, ServerError, TurnFailed,
} from "./errors.js";
import { approvalResponse, FrameDecoder, MAX_FRAME_BYTES, verificationReport, type Frame, type Hello, type RecordValue } from "./protocol.js";
import type { AgentEvent, AgentOptions, ApprovalRequest, ApprovalResponse, PromptOptions, Reply, VerificationReport } from "./types.js";

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T | PromiseLike<T>) => void;
  reject: (error: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  // A process can fail before the caller reaches its await.
  void promise.catch(() => {});
  return { promise, resolve, reject };
}

/** A single-consumer queue that fails explicitly rather than silently dropping events. */
class EventQueue implements AsyncIterableIterator<AgentEvent> {
  private items: { event: AgentEvent; bytes: number }[] = [];
  private bytes = 0;
  private waiter: Deferred<IteratorResult<AgentEvent>> | undefined;
  private ended = false;
  private error: unknown;
  push(event: AgentEvent, bytes: number): void {
    if (this.ended) throw new ProtocolError("An event arrived after its terminal event.");
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = undefined;
      waiter.resolve({ value: event, done: false });
    } else {
      if (this.items.length >= 128 || this.bytes + bytes > 4 * MAX_FRAME_BYTES) throw new BackpressureError();
      this.items.push({ event, bytes });
      this.bytes += bytes;
    }
  }
  end(error?: unknown): void {
    this.ended = true;
    this.error = error;
    if (error !== undefined) { this.items = []; this.bytes = 0; }
    if (this.waiter) {
      if (error !== undefined) this.waiter.reject(error);
      else this.waiter.resolve({ value: undefined, done: true });
      this.waiter = undefined;
    }
  }
  discard(): void { this.items = []; this.bytes = 0; }
  next(): Promise<IteratorResult<AgentEvent>> {
    if (this.error !== undefined) return Promise.reject(this.error);
    const item = this.items.shift();
    if (item) { this.bytes -= item.bytes; return Promise.resolve({ value: item.event, done: false }); }
    if (this.ended) return Promise.resolve({ value: undefined, done: true });
    if (this.waiter) return Promise.reject(new KuramaError("concurrent_consumer", "Only one event iterator may read this stream."));
    this.waiter = deferred<IteratorResult<AgentEvent>>();
    return this.waiter.promise;
  }
  [Symbol.asyncIterator](): AsyncIterableIterator<AgentEvent> { return this; }
}

type Done = Extract<AgentEvent, { type: "done" }>;
interface Execution {
  id: string;
  queue: EventQueue;
  terminal: Deferred<Done>;
  accepted: boolean;
  done: boolean;
  draining: boolean;
}
interface Pending { resolve: (result: RecordValue) => void; reject: (error: unknown) => void }

async function within<T>(promise: Promise<T>, milliseconds: number): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new KuramaError("timeout", "The Kurama process did not respond before the cleanup deadline.")), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

export class Agent {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly hello = deferred<Hello>();
  private readonly exited = deferred<void>();
  private readonly stopped = deferred<never>();
  private readonly unsolicited = new EventQueue();
  private readonly recovery = new EventQueue();
  private readonly pending = new Map<string, Pending>();
  private nextId = 1n;
  private active: Execution | undefined;
  private initializing: string | undefined;
  private receivedHello = false;
  private failure: unknown;
  private closing = false;
  private closePromise: Promise<void> | undefined;
  private childExited = false;
  private killTimer: NodeJS.Timeout | undefined;
  private session = "";
  private readonly cleanupMs: number;

  private constructor(private readonly options: AgentOptions) {
    this.cleanupMs = options.shutdownTimeoutMs ?? 5_000;
    const env = { ...process.env, ...options.env };
    const binary = options.binary ?? env.KURAMA_BIN ?? "kurama";
    this.child = spawn(binary, options.mode === "yolo" ? ["--stdio", "--yolo"] : ["--stdio"], {
      env,
      stdio: ["pipe", "pipe", "pipe"],
      // Only our own process group is eligible for forceful cleanup.
      detached: process.platform !== "win32",
      windowsHide: true,
    });
    const decoder = new FrameDecoder((frame, bytes) => this.receive(frame, bytes));
    this.child.stdout.on("data", (chunk: Buffer) => {
      if (this.failure !== undefined) return;
      try { decoder.push(chunk); } catch (error) { this.fail(error); }
    });
    this.child.stdout.on("end", () => {
      try { decoder.end(); } catch (error) { this.fail(error); return; }
      if (!this.closing) this.fail(this.receivedHello
        ? new ProcessError("The Kurama process closed its protocol stream unexpectedly.")
        : new IncompatibleProtocolError());
    });
    this.child.stdout.on("error", () => this.fail(new ProcessError("Cannot read the Kurama protocol stream.")));
    this.child.stdin.on("error", () => {
      if (!this.closing) this.fail(new ProcessError("Cannot write to the Kurama protocol stream."));
    });
    // Always drain diagnostics, but never accumulate or expose potentially sensitive stderr.
    this.child.stderr.resume();
    this.child.stderr.on("error", () => {});
    this.child.on("error", () => {
      this.childExited = true;
      this.exited.resolve();
      this.fail(new ProcessError("Cannot start Kurama. Install a stdio-capable Kurama binary, or set binary / KURAMA_BIN to its executable."));
    });
    this.child.on("exit", (code, signal) => {
      this.childExited = true;
      this.exited.resolve();
      if (!this.closing && this.failure === undefined) this.fail(this.receivedHello
        ? new ProcessError("The Kurama process exited unexpectedly.", code, signal)
        : new IncompatibleProtocolError());
      else this.rejectPending(new ClosedError());
    });
  }

  static async open(options: AgentOptions = {}): Promise<Agent> {
    for (const timeout of [options.startupTimeoutMs, options.shutdownTimeoutMs]) {
      if (timeout !== undefined && (!Number.isFinite(timeout) || timeout <= 0 || timeout > 2_147_483_647)) {
        throw new RangeError("SDK timeouts must be positive milliseconds within the Node timer range.");
      }
    }
    const agent = new Agent(options);
    try {
      try { await within(agent.hello.promise, options.startupTimeoutMs ?? 10_000); }
      catch (error) {
        if (error instanceof KuramaError && error.code === "timeout") throw new IncompatibleProtocolError();
        throw error;
      }
      const params: RecordValue = { protocol_version: 1, workspace: options.workspace ?? process.cwd() };
      if (options.profile !== undefined) params.profile = options.profile;
      if (options.mode !== undefined) params.mode = options.mode;
      if (options.stateDir !== undefined) params.state_dir = options.stateDir;
      if (options.sessionId !== undefined) params.session_id = options.sessionId;
      const request = agent.request("initialize", params, id => { agent.initializing = id; });
      const recover = (async () => {
        for await (const event of agent.recovery) {
          if (event.type === "approval") await agent.handleApproval(event.request, true);
        }
      })();
      void recover.catch(() => {});
      const result = await Promise.race([request, recover.then(() => request)]);
      agent.initializing = undefined;
      agent.recovery.end();
      await recover;
      if (typeof result.session_id !== "string" || typeof result.workspace !== "string" || typeof result.profile !== "string"
        || !["supervised", "auto", "yolo"].includes(String(result.mode))) throw new ProtocolError("Invalid initialize response.");
      if (agent.session && agent.session !== result.session_id) throw new ProtocolError("Recovery events changed session identity.");
      agent.session = result.session_id;
      return agent;
    } catch (error) {
      await agent.close();
      throw error;
    }
  }

  get sessionId(): string { return this.session; }

  /** Recovery and unsolicited events, never reassigned to a later prompt. Single consumer. */
  events(): AsyncIterable<AgentEvent> { return this.unsolicited; }

  stream(text: string, options: PromptOptions = {}): AsyncGenerator<AgentEvent, void, unknown> {
    return this.execute("prompt", { text, explicit_delegation: options.explicitDelegation ?? false }, false);
  }

  async prompt(text: string, options: PromptOptions = {}): Promise<Reply> {
    const chunks: string[] = [];
    let bytes = 0;
    let status: Reply["status"] = "completed";
    for await (const event of this.execute("prompt", { text, explicit_delegation: options.explicitDelegation ?? false }, true)) {
      if (event.type === "text") {
        bytes += Buffer.byteLength(event.text);
        if (bytes > 16 * MAX_FRAME_BYTES) throw new KuramaError("reply_too_large", "The reply exceeded the 16 MiB aggregation limit. Use stream() for large replies.");
        chunks.push(event.text);
      }
      if (event.type === "done") {
        status = event.status;
        if (status === "failed") throw new TurnFailed(event.error, { text: chunks.join(""), sessionId: this.session, status });
      }
    }
    return { text: chunks.join(""), sessionId: this.session, status };
  }

  async verify(name: string): Promise<VerificationReport> {
    let report: VerificationReport | undefined;
    let terminal: Done | undefined;
    for await (const event of this.execute("verify", { name }, true)) {
      if (event.type === "verification") report = event.report;
      if (event.type === "done") terminal = event;
    }
    if (terminal?.status === "failed") throw new TurnFailed(terminal.error);
    if (!report || report.status === "running") {
      if (terminal?.status === "cancelled") throw new KuramaError("cancelled", "Verification was cancelled before a final report was available.");
      throw new ProtocolError("Verification ended without a final report.");
    }
    return report;
  }

  async verificationStatus(): Promise<VerificationReport[]> {
    const result = await this.request("verification_status", {});
    if (!Array.isArray(result.recipes) || !result.recipes.every(verificationReport)) {
      const error = new ProtocolError("Invalid verification status response.");
      this.fail(error);
      throw error;
    }
    return result.recipes;
  }

  async approve(operationId: string, response: ApprovalResponse = "approve_once"): Promise<void> {
    if (!approvalResponse(response)) throw new TypeError("Invalid approval response.");
    const result = await this.request("approve", { operation_id: operationId, response });
    this.accepted(result);
  }

  async cancel(): Promise<boolean> {
    this.ensureOpen();
    if ((!this.active || this.active.done) && this.initializing === undefined) return false;
    const result = await this.request("cancel", {});
    if (typeof result.cancelled !== "boolean") {
      const error = new ProtocolError("Invalid cancellation response.");
      this.fail(error);
      throw error;
    }
    return result.cancelled;
  }

  close(): Promise<void> {
    if (this.closePromise) return this.closePromise;
    this.closing = true;
    this.stopped.reject(new ClosedError());
    this.unsolicited.end(new ClosedError());
    this.recovery.end(new ClosedError());
    if (this.active) { this.active.draining = true; this.active.queue.end(new ClosedError()); }
    this.closePromise = this.finishClose();
    return this.closePromise;
  }

  async [Symbol.asyncDispose](): Promise<void> { await this.close(); }

  private async finishClose(): Promise<void> {
    try {
      if (!this.childExited && this.failure === undefined && this.receivedHello) {
        await within((async () => {
          const result = await this.request("shutdown", {}, undefined, true);
          if (result.closed !== true) throw new ProtocolError("Invalid shutdown response.");
          this.child.stdin.end();
          await this.exited.promise;
        })(), this.cleanupMs);
      }
    } catch { /* Bounded forced cleanup below is mandatory even for a broken protocol. */ }
    finally {
      this.rejectPending(new ClosedError());
      this.active?.terminal.reject(new ClosedError());
      this.child.stdin.destroy();
      this.child.stdout.destroy();
      this.child.stderr.destroy();
      if (!this.childExited) {
        this.signalGroup("SIGTERM");
        try { await within(this.exited.promise, this.cleanupMs); }
        catch { this.signalGroup("SIGKILL"); }
      }
      // A stuck descendant may have inherited pipes after the leader exited.
      this.signalGroup("SIGKILL");
      clearTimeout(this.killTimer);
      if (!this.childExited) {
        try { await within(this.exited.promise, this.cleanupMs); }
        catch { this.child.unref(); }
      }
    }
  }

  private async *execute(method: "prompt" | "verify", params: RecordValue, requireApproval: boolean): AsyncGenerator<AgentEvent, void, unknown> {
    this.ensureOpen();
    if (this.active || this.initializing !== undefined) throw new BusyError();
    const execution: Execution = { id: "", queue: new EventQueue(), terminal: deferred<Done>(), accepted: false, done: false, draining: false };
    this.active = execution;
    try {
      this.accepted(await this.request(method, params, id => { execution.id = id; }));
      for await (const event of execution.queue) {
        if (event.type === "approval") await this.handleApproval(event.request, requireApproval, execution);
        yield event;
      }
    } finally {
      if (execution.accepted && !execution.done && !this.closing && this.failure === undefined) {
        execution.draining = true;
        execution.queue.discard();
        try { await within((async () => { await this.cancel(); await execution.terminal.promise; })(), this.cleanupMs); }
        catch (error) { this.fail(error); await this.close(); }
      }
      if (this.active === execution) this.active = undefined;
    }
  }

  private async handleApproval(request: ApprovalRequest, required: boolean, execution?: Execution): Promise<void> {
    if (!this.options.onApproval) {
      if (required) throw new ApprovalRequired(request);
      return;
    }
    let response: ApprovalResponse;
    try {
      const callback = Promise.resolve().then(() => this.options.onApproval!(request));
      // Cancellation/close must release a callback that never resolves.
      const stopped = execution?.terminal.promise.then(() => undefined);
      const outcome = await Promise.race([callback, this.stopped.promise, ...(stopped ? [stopped] : [])]);
      if (outcome === undefined && execution?.done) return;
      if (!approvalResponse(outcome)) throw new TypeError("onApproval returned an invalid approval response.");
      response = outcome;
    } catch (error) {
      if (this.closing || this.failure !== undefined) throw this.failure ?? new ClosedError();
      throw new ApprovalCallbackError(error);
    }
    if (!execution?.done) await this.approve(request.operation_id, response);
  }

  private accepted(result: RecordValue): void {
    if (result.accepted !== true) {
      const error = new ProtocolError("Invalid execution acknowledgement.");
      this.fail(error);
      throw error;
    }
  }

  private ensureOpen(): void {
    if (this.failure !== undefined) throw this.failure;
    if (this.closing) throw new ClosedError();
  }

  private request(method: string, params: RecordValue, beforeWrite?: (id: string) => void, allowClosing = false): Promise<RecordValue> {
    if (!allowClosing) this.ensureOpen();
    if (this.pending.size >= 32) throw new KuramaError("too_many_requests", "At most 32 control requests may be outstanding.");
    const id = String(this.nextId++);
    let line: string;
    try { line = JSON.stringify({ id, method, params }); }
    catch { throw new TypeError("Request parameters must contain JSON-serializable values."); }
    const bytes = Buffer.byteLength(line);
    if (bytes > MAX_FRAME_BYTES) throw new KuramaError("frame_too_large", "The request exceeds the 1 MiB protocol frame limit.");
    if (this.child.stdin.writableLength + bytes + 1 > 4 * MAX_FRAME_BYTES) {
      const error = new BackpressureError();
      this.fail(error);
      throw error;
    }
    const result = deferred<RecordValue>();
    this.pending.set(id, result);
    beforeWrite?.(id);
    try { this.child.stdin.write(line + "\n"); }
    catch { this.fail(new ProcessError("Cannot write to the Kurama protocol stream.")); }
    return result.promise;
  }

  private receive(frame: Frame, bytes: number): void {
    if (frame.type === "hello") {
      if (this.receivedHello) throw new ProtocolError("The process sent more than one hello.");
      this.receivedHello = true;
      this.hello.resolve(frame);
      return;
    }
    if (!this.receivedHello) throw new ProtocolError("The process did not begin with a hello.");
    if (frame.type === "response") {
      if (frame.id === null) throw new ProtocolError("The process rejected a fatal protocol frame.");
      const pending = this.pending.get(frame.id);
      if (!pending) throw new ProtocolError("The process replied to an unknown request.");
      this.pending.delete(frame.id);
      if (frame.error) {
        pending.reject(frame.error.code === "incompatible_protocol" ? new IncompatibleProtocolError() : new ServerError(frame.error.code, frame.error.message));
      } else {
        if (this.active?.id === frame.id) this.active.accepted = frame.result?.accepted === true;
        pending.resolve(frame.result!);
      }
      return;
    }
    if (this.session && this.session !== frame.session_id) throw new ProtocolError("An event changed session identity.");
    if (!this.session) this.session = frame.session_id;
    if (frame.request_id !== null && frame.request_id === this.active?.id) {
      const execution = this.active;
      if (!execution.accepted || execution.done) throw new ProtocolError("An execution event violated acknowledgement/terminal ordering.");
      if (frame.event.type === "done") {
        execution.done = true;
        execution.terminal.resolve(frame.event);
      }
      if (!execution.draining) execution.queue.push(frame.event, bytes);
      if (execution.done) execution.queue.end();
    } else if (frame.request_id === null || frame.request_id === this.initializing) {
      if (frame.event.type === "done") throw new ProtocolError("A terminal event did not belong to an execution.");
      if (!this.closing) {
        this.unsolicited.push(frame.event, bytes);
        if (this.initializing !== undefined && frame.event.type === "approval") this.recovery.push(frame.event, bytes);
      }
    } else throw new ProtocolError("An event did not belong to a current request.");
  }

  private rejectPending(error: unknown): void {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }

  private fail(error: unknown): void {
    if (this.failure !== undefined) return;
    this.failure = error;
    this.hello.reject(error);
    this.stopped.reject(error);
    this.rejectPending(error);
    this.active?.queue.end(error);
    this.active?.terminal.reject(error);
    this.unsolicited.end(error);
    this.recovery.end(error);
    this.signalGroup("SIGTERM");
    this.killTimer = setTimeout(() => {
      this.signalGroup("SIGKILL");
      this.child.stdin.destroy();
      this.child.stdout.destroy();
      this.child.stderr.destroy();
    }, this.cleanupMs);
    this.killTimer.unref();
  }

  private signalGroup(signal: NodeJS.Signals): void {
    if (this.child.pid === undefined) return;
    try {
      if (process.platform === "win32") {
        if (!this.childExited) this.child.kill(signal);
      } else process.kill(-this.child.pid, signal);
    } catch { /* Already exited; never fall back to an unrelated PID. */ }
  }
}
