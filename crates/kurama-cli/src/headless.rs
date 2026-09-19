//! Versioned, bounded JSON-lines transport for the installed Kurama runtime.
use std::{
    collections::BTreeSet,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use kurama_adapters::{AppPaths, SessionSecrets, read_verification_recipes};
use kurama_protocol::{
    KuramaError,
    id::{OperationId, SessionId},
    policy::{ApprovalResponse, ExecutionMode},
    runtime::RuntimeEvent,
    traits::EventSink,
};
use kurama_sdk::{Events, Handle};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, mpsc},
    task::JoinHandle,
};

use crate::{
    args::{Args, ResumeChoice},
    bootstrap::{self, BootstrapState},
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
const QUEUE_CAPACITY: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_CAPACITY: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    method: String,
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialize {
    protocol_version: u32,
    workspace: PathBuf,
    profile: Option<String>,
    #[serde(default)]
    mode: ExecutionMode,
    state_dir: Option<PathBuf>,
    session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    text: String,
    #[serde(default)]
    explicit_delegation: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verify {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approve {
    operation_id: String,
    response: ApprovalResponse,
}

struct Active {
    id: String,
    cancelled: bool,
}

struct Session {
    handle: Handle,
    events: Events,
    tools: mpsc::Receiver<RuntimeEvent>,
    overflow: Arc<Notify>,
    id: SessionId,
    workspace: PathBuf,
    initialization: Option<(Active, Value)>,
    active: Option<Active>,
    inspection: Option<String>,
    approvals: BTreeSet<OperationId>,
}

struct Output(mpsc::Sender<Vec<u8>>);

impl Output {
    fn send(&self, value: Value) -> Result<(), String> {
        let mut bytes =
            serde_json::to_vec(&value).map_err(|_| "cannot serialize protocol frame")?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err("outgoing protocol frame exceeds limit".into());
        }
        bytes.push(b'\n');
        self.0
            .try_send(bytes)
            .map_err(|_| "protocol output backpressure limit exceeded".into())
    }

    fn result(&self, id: &str, result: Value) -> Result<(), String> {
        self.send(json!({"type":"response", "id":id, "result":result}))
    }

    fn error(&self, id: Option<&str>, code: &str, message: &str) -> Result<(), String> {
        self.send(json!({"type":"response", "id":id, "error":{"code":code,"message":message}}))
    }
}

/// Bash's synchronous sink cannot await capacity. Overflow terminates the transport
/// explicitly instead of silently losing output, and never blocks a runtime thread.
struct ToolSink {
    sender: mpsc::Sender<RuntimeEvent>,
    overflow: Arc<Notify>,
}

impl EventSink for ToolSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        if matches!(event, RuntimeEvent::ToolOutputDelta { .. })
            && self.sender.try_send(event).is_err()
        {
            self.overflow.notify_one();
            return Err(KuramaError::Cancelled);
        }
        Ok(())
    }
}

/// Own spawned I/O tasks so every early-return path cancels them as well.
struct IoTask<T>(JoinHandle<T>);
impl<T> Drop for IoTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn read_frames<R: AsyncRead + Unpin>(
    mut input: R,
    sender: mpsc::Sender<Vec<u8>>,
) -> Result<(), String> {
    let mut frame = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = input
            .read(&mut buffer)
            .await
            .map_err(|_| "protocol input failed")?;
        if count == 0 {
            return if frame.is_empty() {
                Ok(())
            } else {
                Err("unterminated protocol frame".into())
            };
        }
        for part in buffer[..count].split_inclusive(|byte| *byte == b'\n') {
            let terminated = part.last() == Some(&b'\n');
            let contents = if terminated {
                &part[..part.len() - 1]
            } else {
                part
            };
            if frame.len() + contents.len() > MAX_FRAME_BYTES {
                return Err("protocol frame exceeds limit".into());
            }
            frame.extend_from_slice(contents);
            if terminated {
                sender
                    .send(std::mem::take(&mut frame))
                    .await
                    .map_err(|_| "protocol input closed")?;
            }
        }
    }
}

async fn write_frames<W: AsyncWrite + Unpin>(
    mut output: W,
    mut receiver: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    while let Some(frame) = receiver.recv().await {
        tokio::time::timeout(IO_TIMEOUT, output.write_all(&frame))
            .await
            .map_err(|_| "protocol output stalled")?
            .map_err(|_| "protocol output failed")?;
    }
    tokio::time::timeout(IO_TIMEOUT, output.flush())
        .await
        .map_err(|_| "protocol output stalled")?
        .map_err(|_| "protocol output failed".to_owned())
}

fn increasing_id(id: &str, previous: &str) -> bool {
    !id.is_empty()
        && id.as_bytes()[0] != b'0'
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && (id.len() > previous.len() || (id.len() == previous.len() && id > previous))
}

fn empty_params(params: &Value) -> bool {
    params.as_object().is_some_and(|object| object.is_empty())
}

impl Session {
    fn open(params: Initialize, launch: &Args, request_id: String) -> Result<Self, String> {
        let paths = params
            .state_dir
            .map(AppPaths::from_root)
            .map_or_else(AppPaths::discover, Ok)
            .map_err(|_| "cannot open Kurama state directory")?;
        let (tool_tx, tools) = mpsc::channel(64);
        let overflow = Arc::new(Notify::new());
        let prepared = bootstrap::prepare(
            &Args { profile: params.profile, resume: params.session_id.map(ResumeChoice::Id), yolo: launch.yolo, stdio: true, ..Args::default() },
            params.workspace, paths, Arc::new(Mutex::new(SessionSecrets::default())), Some(params.mode),
            Arc::new(ToolSink { sender: tool_tx, overflow: overflow.clone() }),
        ).map_err(|_| "cannot initialize configured agent; check workspace, profile, credentials, and session in Kurama configuration")?;
        let connection = match prepared.state {
            BootstrapState::Connected(connection) => connection,
            BootstrapState::Onboarding => return Err("Kurama is not configured; run kurama interactively first".into()),
            BootstrapState::Credential { .. } => return Err("profile requires an interactive credential; configure an environment or stored credential reference".into()),
        };
        let id = connection.metadata.id.clone();
        let result = json!({"session_id":id, "workspace":prepared.project, "profile":connection.metadata.profile, "mode":connection.metadata.mode});
        let (handle, events) = connection
            .agent
            .launch(connection.metadata, connection.replay)
            .map_err(|_| "cannot launch configured agent session")?;
        Ok(Self {
            handle,
            events,
            tools,
            overflow,
            id,
            workspace: prepared.project,
            initialization: Some((
                Active {
                    id: request_id,
                    cancelled: false,
                },
                result,
            )),
            active: None,
            inspection: None,
            approvals: BTreeSet::new(),
        })
    }

    fn busy(&self) -> bool {
        self.initialization.is_some() || self.active.is_some()
    }

    fn correlation(&self) -> Option<&str> {
        self.initialization
            .as_ref()
            .map(|(active, _)| active.id.as_str())
            .or_else(|| self.active.as_ref().map(|active| active.id.as_str()))
    }

    fn event(&self, output: &Output, event: Value) -> Result<(), String> {
        output.send(json!({"type":"event", "request_id":self.correlation(), "session_id":self.id, "event":event}))
    }

    fn terminal(&mut self, output: &Output, error: Option<Value>) -> Result<(), String> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        self.approvals.clear();
        let status = if active.cancelled {
            "cancelled"
        } else if error.is_some() {
            "failed"
        } else {
            "completed"
        };
        let mut event = json!({"type":"done", "status":status});
        if let Some(error) = error {
            event["error"] = error;
        }
        output.send(
            json!({"type":"event", "request_id":active.id, "session_id":self.id, "event":event}),
        )
    }

    fn runtime(&mut self, output: &Output, event: RuntimeEvent) -> Result<(), String> {
        // Tool output uses a separate synchronous sink; drain it before the canonical
        // completion/terminal so no prior output can become the next request's event.
        if matches!(
            event,
            RuntimeEvent::ToolCompleted { .. }
                | RuntimeEvent::TurnCompleted
                | RuntimeEvent::Error { .. }
                | RuntimeEvent::Ready
                | RuntimeEvent::Shutdown
        ) {
            while let Ok(delta) = self.tools.try_recv() {
                self.runtime(output, delta)?;
            }
        }
        let event = match event {
            RuntimeEvent::Ready => {
                if let Some((active, result)) = self.initialization.take() {
                    self.approvals.clear();
                    if active.cancelled {
                        output.error(
                            Some(&active.id),
                            "cancelled",
                            "initialization recovery cancelled",
                        )?;
                        return Err("initialization recovery cancelled".into());
                    }
                    output.result(&active.id, result)?;
                }
                return Ok(());
            }
            RuntimeEvent::AssistantDelta { text } => json!({"type":"text", "text":text}),
            RuntimeEvent::ApprovalRequired { request } => {
                if self.approvals.len() >= 64 {
                    return Err("pending approval limit exceeded".into());
                }
                self.approvals.insert(request.operation_id.clone());
                json!({"type":"approval", "request":request})
            }
            RuntimeEvent::ToolStarted {
                operation_id,
                name,
                context,
            } => {
                json!({"type":"tool_started", "operation_id":operation_id, "name":name, "context":context})
            }
            RuntimeEvent::ToolOutputDelta {
                call_id,
                stream,
                chunk,
            } => json!({"type":"tool_output", "call_id":call_id, "stream":stream, "chunk":chunk}),
            RuntimeEvent::ToolCompleted {
                operation_id,
                result,
            } => {
                self.approvals.remove(&operation_id);
                json!({"type":"tool_completed", "operation_id":operation_id, "result":result})
            }
            RuntimeEvent::AgentUpdated { snapshot } => {
                json!({"type":"agent_updated", "snapshot":snapshot})
            }
            RuntimeEvent::Usage { usage } => json!({"type":"usage", "usage":usage}),
            RuntimeEvent::Status { message } => json!({"type":"status", "message":message}),
            RuntimeEvent::VerificationUpdated { report } => {
                json!({"type":"verification", "report":report})
            }
            RuntimeEvent::VerificationsInspected { reports } => {
                if let Some(id) = self.inspection.take() {
                    output.result(&id, json!({"recipes":reports}))?;
                }
                return Ok(());
            }
            RuntimeEvent::TurnCompleted => {
                if self.initialization.is_some() {
                    return Ok(());
                }
                return self.terminal(output, None);
            }
            RuntimeEvent::Error { .. } => {
                // Provider error strings can contain URLs, headers or request bodies.
                // The wire does not expose unsanitized diagnostic strings.
                if let Some((active, _)) = self.initialization.take() {
                    output.error(
                        Some(&active.id),
                        if active.cancelled {
                            "cancelled"
                        } else {
                            "initialization_failed"
                        },
                        "session recovery failed",
                    )?;
                    return Err("session recovery failed".into());
                }
                if self.active.is_some() {
                    return self.terminal(
                        output,
                        Some(json!({"code":"runtime_error", "message":"agent execution failed"})),
                    );
                }
                json!({"type":"runtime", "event":{"type":"error", "message":"agent runtime error"}})
            }
            RuntimeEvent::Shutdown => return Err("agent runtime closed".into()),
            event => json!({"type":"runtime", "event":event}),
        };
        self.event(output, event)
    }

    async fn request(&mut self, output: &Output, request: Request) -> Result<bool, String> {
        let id = request.id.as_str();
        match request.method.as_str() {
            "shutdown" if empty_params(&request.params) => {
                output.result(id, json!({"closed":true}))?;
                return Ok(true);
            }
            "cancel" if empty_params(&request.params) => {
                let active = self
                    .initialization
                    .as_mut()
                    .map(|(active, _)| active)
                    .or(self.active.as_mut());
                if let Some(active) = active {
                    if !active.cancelled {
                        command(self.handle.cancel_turn()).await?;
                        active.cancelled = true;
                    }
                    output.result(id, json!({"cancelled":true}))?;
                } else {
                    output.result(id, json!({"cancelled":false}))?;
                }
            }
            "approve" => {
                let Ok(params) = serde_json::from_value::<Approve>(request.params) else {
                    return output
                        .error(Some(id), "invalid_params", "invalid approval parameters")
                        .map(|_| false);
                };
                let operation_id = OperationId::from(params.operation_id);
                if !self.busy() || !self.approvals.remove(&operation_id) {
                    output.error(
                        Some(id),
                        "unknown_approval",
                        "operation has no pending approval in this session",
                    )?;
                } else {
                    command(self.handle.resolve_approval(operation_id, params.response)).await?;
                    output.result(id, json!({"accepted":true}))?;
                }
            }
            "prompt" | "verify" if self.busy() => {
                output.error(
                    Some(id),
                    "busy",
                    "agent is executing or recovering a session",
                )?;
            }
            "prompt" => {
                let Ok(params) = serde_json::from_value::<Prompt>(request.params) else {
                    return output
                        .error(Some(id), "invalid_params", "invalid prompt parameters")
                        .map(|_| false);
                };
                if params.text.trim().is_empty() {
                    output.error(Some(id), "invalid_params", "prompt text must not be blank")?;
                } else {
                    command(self.handle.submit(params.text, params.explicit_delegation)).await?;
                    self.active = Some(Active {
                        id: request.id.clone(),
                        cancelled: false,
                    });
                    output.result(id, json!({"accepted":true}))?;
                }
            }
            "verify" => {
                let Ok(params) = serde_json::from_value::<Verify>(request.params) else {
                    return output
                        .error(
                            Some(id),
                            "invalid_params",
                            "invalid verification parameters",
                        )
                        .map(|_| false);
                };
                match read_verification_recipes(&self.workspace) {
                    Ok(mut recipes) => {
                        if let Some(recipe) = recipes.remove(&params.name) {
                            match tokio::time::timeout(
                                IO_TIMEOUT,
                                self.handle.verify(params.name, recipe),
                            )
                            .await
                            {
                                Ok(Ok(())) => {
                                    self.active = Some(Active {
                                        id: request.id.clone(),
                                        cancelled: false,
                                    });
                                    output.result(id, json!({"accepted":true}))?;
                                }
                                Ok(Err(KuramaError::Policy(_))) => {
                                    output.error(
                                        Some(id),
                                        "busy",
                                        "agent is executing or recovering a session",
                                    )?;
                                }
                                Ok(Err(_)) => return Err("agent command channel closed".into()),
                                Err(_) => return Err("agent command queue stalled".into()),
                            }
                        } else {
                            output.error(
                                Some(id),
                                "unknown_recipe",
                                "verification recipe is not configured",
                            )?;
                        }
                    }
                    Err(_) => {
                        output.error(
                            Some(id),
                            "invalid_recipes",
                            "cannot load project verification recipes",
                        )?;
                    }
                }
            }
            "verification_status" if empty_params(&request.params) => {
                if self.initialization.is_some() || self.inspection.is_some() {
                    output.error(
                        Some(id),
                        "busy",
                        "agent is recovering or inspecting verification status",
                    )?;
                } else {
                    match read_verification_recipes(&self.workspace) {
                        Ok(recipes) => {
                            command(self.handle.inspect_verifications(recipes)).await?;
                            self.inspection = Some(request.id);
                        }
                        Err(_) => {
                            output.error(
                                Some(id),
                                "invalid_recipes",
                                "cannot load project verification recipes",
                            )?;
                        }
                    }
                }
            }
            "initialize" => {
                output.error(
                    Some(id),
                    "already_initialized",
                    "initialize may only be called once",
                )?;
            }
            "cancel" | "shutdown" | "verification_status" => {
                output.error(
                    Some(id),
                    "invalid_params",
                    "method requires empty parameters",
                )?;
            }
            _ => {
                output.error(Some(id), "unknown_method", "unsupported protocol method")?;
            }
        }
        Ok(false)
    }

    async fn cleanup(&mut self, output: &Output) {
        let handle = self.handle.clone();
        // Continue draining while Shutdown is delivered: the actor may currently be
        // blocked publishing to its bounded event queue. Waiting only on send deadlocks.
        let shutdown = handle.shutdown();
        tokio::pin!(shutdown);
        let deadline = tokio::time::sleep(CLEANUP_TIMEOUT);
        tokio::pin!(deadline);
        let mut sent = false;
        if let Some((active, _)) = self.initialization.as_mut() {
            active.cancelled = true;
        }
        if let Some(active) = self.active.as_mut() {
            active.cancelled = true;
        }
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                _ = &mut shutdown, if !sent => { sent = true; }
                event = self.events.recv() => match event {
                    Some(RuntimeEvent::Shutdown) | None => break,
                    Some(event) => { let _ = self.runtime(output, event); }
                },
                Some(event) = self.tools.recv() => { let _ = self.runtime(output, event); }
            }
        }
        while let Ok(event) = self.tools.try_recv() {
            let _ = self.runtime(output, event);
        }
        if let Some((active, _)) = self.initialization.take() {
            let _ = output.error(Some(&active.id), "cancelled", "initialization closed");
        }
        if let Some(id) = self.inspection.take() {
            let _ = output.error(Some(&id), "cancelled", "inspection closed");
        }
        let _ = self.terminal(output, None);
        self.events.close();
        self.tools.close();
    }
}

async fn command(future: impl Future<Output = Result<(), KuramaError>>) -> Result<(), String> {
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| "agent command queue stalled")?
        .map_err(|_| "agent command channel closed".into())
}

async fn serve<R, W>(input: R, output: W, args: Args) -> Result<(), String>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (input_tx, mut input_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CAPACITY);
    let mut reader = IoTask(tokio::spawn(read_frames(input, input_tx)));
    let mut writer = IoTask(tokio::spawn(write_frames(output, output_rx)));
    let output = Output(output_tx);
    output.send(json!({"type":"hello", "protocol_version":PROTOCOL_VERSION, "server_version":env!("CARGO_PKG_VERSION"), "max_frame_bytes":MAX_FRAME_BYTES, "capabilities":["prompt","stream","approval","cancel","resume","verify"]}))?;
    let mut session: Option<Session> = None;
    let mut previous_id = String::new();
    let mut initialized = false;
    let mut writer_finished = false;
    let result = async {
        loop {
            // Let the writer make progress even when every engine receive is ready.
            tokio::task::yield_now().await;
            let overflow = session.as_ref().map(|session| session.overflow.clone());
            tokio::select! {
                biased;
                result = &mut writer.0 => { writer_finished = true; return result.map_err(|_| "protocol writer stopped")?; }
                _ = async { overflow.as_ref().expect("session").notified().await }, if session.is_some() => {
                    return Err("tool output backpressure limit exceeded".into());
                }
                event = async {
                    let session = session.as_mut().expect("session");
                    tokio::select! {
                        biased;
                        event = session.events.recv() => event,
                        Some(event) = session.tools.recv() => Some(event),
                    }
                }, if session.is_some() => {
                    match event {
                        Some(event) => session.as_mut().expect("session").runtime(&output, event)?,
                        None => return Err("agent runtime closed".into()),
                    }
                }
                frame = input_rx.recv() => {
                    let Some(frame) = frame else {
                        return (&mut reader.0).await.map_err(|_| "protocol reader stopped")?;
                    };
                    let request = match serde_json::from_slice::<Request>(&frame) {
                        Ok(request) => request,
                        Err(_) => { output.error(None, "invalid_request", "expected a UTF-8 JSON request with id, method and params")?; return Err("invalid protocol request".into()); }
                    };
                    if !increasing_id(&request.id, &previous_id) {
                        output.error(None, "invalid_id", "request IDs must be increasing canonical positive decimal strings")?;
                        continue;
                    }
                    previous_id.clone_from(&request.id);
                    if let Some(session) = session.as_mut() {
                        if session.request(&output, request).await? { return Ok(()); }
                        continue;
                    }
                    match request.method.as_str() {
                        "shutdown" if empty_params(&request.params) => { output.result(&request.id, json!({"closed":true}))?; return Ok(()); }
                        "cancel" if empty_params(&request.params) => { output.result(&request.id, json!({"cancelled":false}))?; }
                        "initialize" if initialized => { output.error(Some(&request.id), "already_initialized", "initialize may only be called once")?; }
                        "initialize" => {
                            let Ok(params) = serde_json::from_value::<Initialize>(request.params) else { output.error(Some(&request.id), "invalid_params", "invalid initialization parameters")?; continue; };
                            initialized = true;
                            if params.protocol_version != PROTOCOL_VERSION {
                                output.error(Some(&request.id), "incompatible_protocol", "SDK and Kurama protocol versions are incompatible; update both")?;
                                return Ok(());
                            }
                            if params.mode == ExecutionMode::Yolo && !args.yolo {
                                output.error(Some(&request.id), "yolo_not_enabled", "yolo mode requires launching kurama --stdio --yolo")?;
                                continue;
                            }
                            match Session::open(params, &args, request.id.clone()) {
                                Ok(opened) => session = Some(opened),
                                Err(message) => output.error(Some(&request.id), "initialization_failed", &message)?,
                            }
                        }
                        "prompt" | "verify" | "approve" | "verification_status" => { output.error(Some(&request.id), "not_initialized", "initialize the agent first")?; }
                        "shutdown" | "cancel" => { output.error(Some(&request.id), "invalid_params", "method requires empty parameters")?; }
                        _ => { output.error(Some(&request.id), "unknown_method", "unsupported protocol method")?; }
                    }
                }
            }
        }
    }.await;
    if let Err(message) = &result {
        let _ = output.error(None, "transport_error", message);
    }
    reader.0.abort();
    input_rx.close();
    if let Some(session) = session.as_mut() {
        session.cleanup(&output).await;
    }
    drop(output);
    if !writer_finished {
        match tokio::time::timeout(IO_TIMEOUT, &mut writer.0).await {
            Ok(Ok(Ok(()))) => {}
            _ if result.is_ok() => return Err("protocol output closed before drain".into()),
            _ => {}
        }
    }
    result
}

#[cfg(unix)]
mod unix {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::{
        io,
        os::fd::{AsFd, OwnedFd},
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf, unix::AsyncFd};

    pub(super) struct Pipe {
        fd: AsyncFd<OwnedFd>,
        original: OFlags,
    }
    impl Pipe {
        pub(super) fn new(source: impl AsFd, interest: Interest) -> io::Result<Self> {
            let fd = rustix::io::dup(&source)?;
            let original = fcntl_getfl(&fd)?;
            fcntl_setfl(&fd, original | OFlags::NONBLOCK)?;
            match AsyncFd::with_interest(fd, interest) {
                Ok(fd) => Ok(Self { fd, original }),
                Err(error) => {
                    let _ = fcntl_setfl(&source, original);
                    Err(error)
                }
            }
        }
    }
    impl Drop for Pipe {
        fn drop(&mut self) {
            let _ = fcntl_setfl(self.fd.get_ref(), self.original);
        }
    }
    impl AsyncRead for Pipe {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                let mut ready = std::task::ready!(self.fd.poll_read_ready(cx))?;
                match ready.try_io(|fd| {
                    rustix::io::read(fd.get_ref(), buf.initialize_unfilled())
                        .map_err(io::Error::from)
                }) {
                    Ok(Ok(count)) => {
                        buf.advance(count);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(error)) => return Poll::Ready(Err(error)),
                    Err(_) => continue,
                }
            }
        }
    }
    impl AsyncWrite for Pipe {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            loop {
                let mut ready = std::task::ready!(self.fd.poll_write_ready(cx))?;
                match ready
                    .try_io(|fd| rustix::io::write(fd.get_ref(), bytes).map_err(io::Error::from))
                {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

pub async fn run(args: Args) -> Result<(), String> {
    #[cfg(unix)]
    {
        let input = unix::Pipe::new(io::stdin(), tokio::io::Interest::READABLE)
            .map_err(|_| "cannot open nonblocking protocol input")?;
        let output = unix::Pipe::new(io::stdout(), tokio::io::Interest::WRITABLE)
            .map_err(|_| "cannot open nonblocking protocol output")?;
        serve(input, output, args).await
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err("headless stdio is supported on Linux and macOS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurama_adapters::ConfigRepository;
    use kurama_protocol::{
        session::{EventEnvelope, SessionEvent, SessionMetadata},
        tool::{CommandClass, Operation, ToolInvocation},
        traits::SessionStore,
    };
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

    struct Client {
        input: BufReader<ReadHalf<DuplexStream>>,
        output: WriteHalf<DuplexStream>,
        server: JoinHandle<Result<(), String>>,
    }

    impl Client {
        fn start() -> Self {
            let (client, server) = tokio::io::duplex(65_536);
            let (input, output) = tokio::io::split(server);
            let server = tokio::spawn(serve(input, output, Args::default()));
            let (input, output) = tokio::io::split(client);
            Self {
                input: BufReader::new(input),
                output,
                server,
            }
        }

        async fn receive(&mut self) -> Value {
            let mut line = String::new();
            let count =
                tokio::time::timeout(Duration::from_secs(10), self.input.read_line(&mut line))
                    .await
                    .expect("protocol deadline")
                    .expect("read frame");
            assert_ne!(count, 0, "unexpected protocol EOF");
            serde_json::from_str(&line).expect("JSON frame")
        }

        async fn send(&mut self, value: Value) {
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            self.output.write_all(&bytes).await.unwrap();
        }

        async fn request(&mut self, id: &str, method: &str, params: Value) -> Value {
            self.send(json!({"id":id,"method":method,"params":params}))
                .await;
            let response = self.receive().await;
            assert_eq!(response["type"], "response");
            assert_eq!(response["id"], id);
            response
        }

        async fn event(&mut self, id: &str, kind: &str) -> Value {
            loop {
                let frame = self.receive().await;
                assert_eq!(frame["type"], "event", "{frame}");
                assert_eq!(frame["request_id"], id);
                if frame["event"]["type"] == kind {
                    return frame["event"].clone();
                }
                assert_ne!(
                    frame["event"]["type"], "done",
                    "turn ended before expected event"
                );
            }
        }

        async fn close(mut self, id: &str) {
            assert_eq!(
                self.request(id, "shutdown", json!({})).await["result"]["closed"],
                true
            );
            tokio::time::timeout(Duration::from_secs(6), &mut self.server)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    fn configuration() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        std::fs::create_dir_all(workspace.join(".kurama")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("config.toml"), "version = 1\ndefault_profile = 'test'\ndefault_mode = 'auto'\n[profiles.test]\nkind = 'open_ai_compatible'\nmodel = 'test'\nendpoint = 'http://127.0.0.1:9/v1'\nmax_input_tokens = 32000\nmax_output_tokens = 4000\n").unwrap();
        std::fs::write(workspace.join(".kurama/verification.toml"), "version = 1\n[recipes.quick]\ncommand = 'printf checked > verified'\n[recipes.wait]\ncommand = 'sleep 30; printf leaked > leaked'\n").unwrap();
        (temp, workspace, state)
    }

    async fn initialize(
        client: &mut Client,
        workspace: &std::path::Path,
        state: &std::path::Path,
        session: Option<&str>,
    ) -> String {
        assert_eq!(client.receive().await["type"], "hello");
        let mut params = json!({"protocol_version":1,"workspace":workspace,"state_dir":state});
        if let Some(session) = session {
            params["session_id"] = json!(session);
        }
        let response = client.request("1", "initialize", params).await;
        assert_eq!(response["result"]["mode"], "supervised", "{response}");
        response["result"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn shared_requests_and_fragmented_utf8_have_deterministic_correlation() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../protocol/sdk.fixtures.json")).unwrap();
        let mut client = Client::start();
        assert_eq!(client.receive().await, fixtures["frames"]["hello"]);
        let mut frame = fixtures["frames"]["prompt_request"].clone();
        frame["params"]["text"] = json!("Hello, 世界\n");
        let mut bytes = serde_json::to_vec(&frame).unwrap();
        bytes.push(b'\n');
        for byte in bytes {
            client.output.write_all(&[byte]).await.unwrap();
        }
        let response = client.receive().await;
        assert_eq!(response["id"], "2");
        assert_eq!(response["error"]["code"], "not_initialized");
        client
            .send(fixtures["frames"]["prompt_request"].clone())
            .await;
        assert_eq!(client.receive().await["error"]["code"], "invalid_id");
        client
            .send(fixtures["frames"]["shutdown_request"].clone())
            .await;
        assert_eq!(
            client.receive().await,
            fixtures["frames"]["shutdown_response"]
        );
        client.server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn incompatible_version_closes_without_initializing() {
        let mut client = Client::start();
        client.receive().await;
        let response = client
            .request(
                "1",
                "initialize",
                json!({"protocol_version":2,"workspace":"/"}),
            )
            .await;
        assert_eq!(response["error"]["code"], "incompatible_protocol");
        assert!(client.server.await.unwrap().is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_is_busy_and_cancellable_before_initialize_succeeds() {
        let (_temp, workspace, state) = configuration();
        let store = kurama_adapters::FsSessionStore::open(state.clone()).unwrap();
        let session_id = SessionId::from("ses_recovery");
        let operation_id = OperationId::from("op_recovery");
        let invocation = ToolInvocation {
            call_id: "call_recovery".into(),
            name: "bash".into(),
            arguments: json!({"command":"printf leaked > recovered","cwd":".","timeout_ms":60000}),
        };
        let metadata = SessionMetadata {
            id: session_id.clone(),
            created_at_ms: 1,
            project_root: workspace.canonicalize().unwrap().display().to_string(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        };
        store.create(&metadata).unwrap();
        let records = [
            SessionEvent::SessionStarted { metadata },
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: Operation::Bash {
                    command: "printf leaked > recovered".into(),
                    cwd: workspace.clone(),
                    class: CommandClass::Mutating,
                    timeout_ms: 60000,
                },
            },
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
            SessionEvent::ApprovalRequested {
                operation_id,
                summary: "recover pending command".into(),
            },
        ];
        for (index, record) in records.into_iter().enumerate() {
            store
                .append(&EventEnvelope::new(
                    index as u64,
                    1,
                    session_id.clone(),
                    None,
                    record,
                ))
                .unwrap();
        }
        let mut client = Client::start();
        client.receive().await;
        client.send(json!({"id":"1","method":"initialize","params":{"protocol_version":1,"workspace":workspace,"state_dir":state,"session_id":session_id}})).await;
        client.event("1", "approval").await;
        assert_eq!(
            client
                .request("2", "prompt", json!({"text":"must wait"}))
                .await["error"]["code"],
            "busy"
        );
        assert_eq!(
            client.request("3", "cancel", json!({})).await["result"]["cancelled"],
            true
        );
        loop {
            let frame = client.receive().await;
            if frame["type"] == "response" {
                assert_eq!(frame["id"], "1");
                assert_eq!(frame["error"]["code"], "cancelled");
                break;
            }
            assert_eq!(frame["request_id"], "1");
            assert_ne!(frame["event"]["type"], "done");
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(6), client.server)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(!workspace.join("recovered").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_cancels_a_running_child_without_waiting_for_its_timeout() {
        let (_temp, workspace, state) = configuration();
        let mut client = Client::start();
        initialize(&mut client, &workspace, &state, None).await;
        client.request("2", "verify", json!({"name":"wait"})).await;
        let approval = client.event("2", "approval").await;
        client.request("3", "approve", json!({"operation_id":approval["request"]["operation_id"],"response":"approve_once"})).await;
        client.event("2", "tool_started").await;
        client.output.shutdown().await.unwrap();
        assert_eq!(client.event("2", "done").await["status"], "cancelled");
        tokio::time::timeout(Duration::from_secs(6), client.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!workspace.join("leaked").exists());
    }

    #[tokio::test]
    async fn unterminated_and_oversized_frames_fail_boundedly() {
        for bytes in [b"{\"id\":".to_vec(), vec![b'x'; MAX_FRAME_BYTES + 1]] {
            let mut client = Client::start();
            client.receive().await;
            client.output.write_all(&bytes).await.unwrap();
            client.output.shutdown().await.unwrap();
            assert_eq!(client.receive().await["error"]["code"], "transport_error");
            assert!(
                tokio::time::timeout(Duration::from_secs(6), client.server)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn broken_output_and_idle_eof_do_not_leave_reader_tasks_waiting() {
        let (client, server) = tokio::io::duplex(64);
        let (input, output) = tokio::io::split(server);
        drop(client);
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            serve(input, output, Args::default()),
        )
        .await;
        assert!(
            result.is_ok(),
            "stdio shutdown waited on a blocked read/write"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approval_busy_cancel_and_resume_use_the_authoritative_runtime() {
        let (_temp, workspace, state) = configuration();
        let repository = ConfigRepository::open(AppPaths::from_root(state.clone())).unwrap();
        let previous_mode = repository.read_state().unwrap().last_mode;
        let mut client = Client::start();
        let session = initialize(&mut client, &workspace, &state, None).await;
        assert_eq!(repository.read_state().unwrap().last_mode, previous_mode);
        assert_eq!(
            client.request("2", "verify", json!({"name":"quick"})).await["result"]["accepted"],
            true
        );
        let approval = client.event("2", "approval").await;
        assert_eq!(
            client
                .request("3", "prompt", json!({"text":"must not execute"}))
                .await["error"]["code"],
            "busy"
        );
        assert_eq!(
            client
                .request(
                    "4",
                    "approve",
                    json!({"operation_id":"foreign","response":"approve_once"})
                )
                .await["error"]["code"],
            "unknown_approval"
        );
        let inspection = client.request("5", "verification_status", json!({})).await;
        assert_eq!(inspection["result"]["recipes"][0]["status"], "running");
        assert_eq!(client.request("6", "approve", json!({"operation_id":approval["request"]["operation_id"],"response":"approve_once"})).await["result"]["accepted"], true);
        assert_eq!(client.event("2", "done").await["status"], "completed");
        assert_eq!(
            std::fs::read_to_string(workspace.join("verified")).unwrap(),
            "checked"
        );
        assert_eq!(
            client.request("7", "cancel", json!({})).await["result"]["cancelled"],
            false
        );

        assert_eq!(
            client.request("8", "verify", json!({"name":"wait"})).await["result"]["accepted"],
            true
        );
        let approval = client.event("8", "approval").await;
        client.request("9", "approve", json!({"operation_id":approval["request"]["operation_id"],"response":"approve_once"})).await;
        client.event("8", "tool_started").await;
        assert_eq!(
            client.request("10", "cancel", json!({})).await["result"]["cancelled"],
            true
        );
        assert_eq!(client.event("8", "done").await["status"], "cancelled");
        assert!(!workspace.join("leaked").exists());
        assert_eq!(
            client
                .request("11", "verify", json!({"name":"quick"}))
                .await["result"]["accepted"],
            true
        );
        let approval = client.event("11", "approval").await;
        client
            .request(
                "12",
                "approve",
                json!({"operation_id":approval["request"]["operation_id"],"response":"deny"}),
            )
            .await;
        assert_eq!(client.event("11", "done").await["status"], "completed");
        client.close("13").await;

        let mut resumed = Client::start();
        assert_eq!(
            initialize(&mut resumed, &workspace, &state, Some(&session)).await,
            session
        );
        let inspection = resumed.request("2", "verification_status", json!({})).await;
        assert_eq!(inspection["result"]["recipes"][0]["status"], "denied");
        assert_eq!(inspection["result"]["recipes"][1]["status"], "cancelled");
        resumed.close("3").await;
    }
}
