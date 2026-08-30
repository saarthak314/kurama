use std::{
    collections::VecDeque,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{Stream, StreamExt};
use kurama_protocol::{
    KuramaError,
    model::ModelEvent,
    traits::{CancelSignal, ModelStream},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

#[cfg(feature = "claude-bridge")]
pub mod claude;
#[cfg(feature = "codex-bridge")]
pub mod codex;
pub mod control;

pub(crate) const MAX_JSONL_LINE_BYTES: usize = 1024 * 1024;
const BRIDGE_RECORD_CAPACITY: usize = 4;
const STDERR_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const DEFAULT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const INACTIVITY_STDERR_JOIN_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) struct InactivityWatchdog {
    timeout: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl InactivityWatchdog {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
        }
    }

    pub(crate) fn observe_record(&mut self, record: &str) -> bool {
        if record.trim().is_empty() {
            return false;
        }
        self.sleep
            .as_mut()
            .reset(tokio::time::Instant::now() + self.timeout);
        true
    }

    pub(crate) async fn wait(&mut self) {
        self.sleep.as_mut().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCommand {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: String,
    pub cwd: Option<PathBuf>,
}

impl BridgeCommand {
    async fn spawn(
        self,
        secrets: Vec<String>,
        inactivity_timeout: Duration,
    ) -> Result<BridgeLineStream, KuramaError> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| {
            KuramaError::Model(format!("{} CLI unavailable: {error}", self.program))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| KuramaError::Model(format!("{} stdin unavailable", self.program)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| KuramaError::Model(format!("{} stdout unavailable", self.program)))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| KuramaError::Model(format!("{} stderr unavailable", self.program)))?;
        let (sender, receiver) = mpsc::channel(BRIDGE_RECORD_CAPACITY);
        let (cancel, cancel_rx) = oneshot::channel();
        let task = tokio::spawn(drive_process(
            self.program,
            self.stdin,
            child,
            stdin,
            stdout,
            stderr,
            sender,
            cancel_rx,
            secrets,
            inactivity_timeout,
        ));
        Ok(BridgeLineStream {
            receiver,
            cancel: Some(cancel),
            task: Some(task),
        })
    }
}

pub(crate) trait BridgeDecoder: Send + 'static {
    fn push_line(&mut self, line: &str) -> Result<Vec<ModelEvent>, KuramaError>;
    fn finish(&self) -> Result<(), KuramaError>;
}

pub(crate) async fn event_stream<D: BridgeDecoder>(
    command: BridgeCommand,
    decoder: D,
    cancel: &dyn CancelSignal,
    secrets: Vec<String>,
) -> Result<ModelStream, KuramaError> {
    event_stream_with_inactivity(
        command,
        decoder,
        cancel,
        secrets,
        DEFAULT_INACTIVITY_TIMEOUT,
    )
    .await
}

pub(crate) async fn event_stream_with_inactivity<D: BridgeDecoder>(
    command: BridgeCommand,
    mut decoder: D,
    cancel: &dyn CancelSignal,
    secrets: Vec<String>,
    inactivity_timeout: Duration,
) -> Result<ModelStream, KuramaError> {
    if cancel.is_cancelled() {
        return Err(KuramaError::Cancelled);
    }
    let mut lines = command.spawn(secrets.clone(), inactivity_timeout).await?;
    loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => {
                lines.cancel_and_wait().await;
                return Err(KuramaError::Cancelled);
            }
            line = lines.next() => line,
        };
        match line {
            Some(Ok(line)) => match decoder.push_line(&line) {
                Ok(events) => {
                    let terminal = decoder.finish().is_ok();
                    if terminal && events.is_empty() {
                        lines.cancel_and_wait().await;
                        return Ok(Box::pin(futures_util::stream::empty()));
                    }
                    if !events.is_empty() {
                        return Ok(decoded_event_stream(
                            lines, decoder, events, secrets, terminal,
                        ));
                    }
                }
                Err(error) => {
                    lines.cancel_and_wait().await;
                    return Err(control::bounded_kurama_error(error, &secrets));
                }
            },
            Some(Err(error)) => {
                lines.cancel_and_wait().await;
                return Err(control::bounded_kurama_error(error, &secrets));
            }
            None => {
                decoder
                    .finish()
                    .map_err(|error| control::bounded_kurama_error(error, &secrets))?;
                return Ok(Box::pin(futures_util::stream::empty()));
            }
        }
    }
}

fn decoded_event_stream<D: BridgeDecoder>(
    lines: BridgeLineStream,
    decoder: D,
    events: Vec<ModelEvent>,
    secrets: Vec<String>,
    terminal: bool,
) -> ModelStream {
    let state = DecodedStreamState {
        lines,
        decoder,
        pending: events.into_iter().map(Ok).collect(),
        secrets,
        terminal,
        ended: false,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }
                if state.terminal {
                    state.lines.cancel_and_wait().await;
                    return None;
                }
                if state.ended {
                    return None;
                }
                match state.lines.next().await {
                    Some(Ok(line)) => match state.decoder.push_line(&line) {
                        Ok(events) => {
                            state.pending.extend(events.into_iter().map(Ok));
                            state.terminal = state.decoder.finish().is_ok();
                        }
                        Err(error) => {
                            state.lines.cancel_and_wait().await;
                            state.ended = true;
                            let error = control::bounded_kurama_error(error, &state.secrets);
                            return Some((Err(error), state));
                        }
                    },
                    Some(Err(error)) => {
                        if state.decoder.finish().is_ok() {
                            return None;
                        }
                        state.lines.cancel_and_wait().await;
                        state.ended = true;
                        let error = control::bounded_kurama_error(error, &state.secrets);
                        return Some((Err(error), state));
                    }
                    None => {
                        state.ended = true;
                        if let Err(error) = state.decoder.finish() {
                            let error = control::bounded_kurama_error(error, &state.secrets);
                            return Some((Err(error), state));
                        }
                        return None;
                    }
                }
            }
        },
    ))
}

struct DecodedStreamState<D> {
    lines: BridgeLineStream,
    decoder: D,
    pending: VecDeque<Result<ModelEvent, KuramaError>>,
    secrets: Vec<String>,
    terminal: bool,
    ended: bool,
}

struct BridgeLineStream {
    receiver: mpsc::Receiver<Result<String, KuramaError>>,
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl BridgeLineStream {
    fn request_cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }

    async fn cancel_and_wait(&mut self) {
        self.request_cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Stream for BridgeLineStream {
    type Item = Result<String, KuramaError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

impl Drop for BridgeLineStream {
    fn drop(&mut self) {
        self.request_cancel();
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_process(
    program: String,
    input: String,
    mut child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    sender: mpsc::Sender<Result<String, KuramaError>>,
    mut cancel: oneshot::Receiver<()>,
    secrets: Vec<String>,
    inactivity_timeout: Duration,
) {
    let mut stdin_task = tokio::spawn(write_stdin(stdin, input));
    let mut stderr_task = tokio::spawn(drain_bounded(stderr, STDERR_DIAGNOSTIC_BYTES));
    let mut stdout = BoundedJsonlReader::new(stdout);
    let mut stdout_error = None;
    let mut last_stdout = None;
    let mut inactivity = InactivityWatchdog::new(inactivity_timeout);

    loop {
        let record = tokio::select! {
            _ = &mut cancel => {
                terminate_process_group(&mut child).await;
                stdin_task.abort();
                stderr_task.abort();
                return;
            }
            record = stdout.next_record(&program) => record,
            _ = inactivity.wait() => {
                terminate_process_group(&mut child).await;
                stdin_task.abort();
                let diagnostic = bounded_stderr_diagnostic(
                    &program,
                    &mut stderr_task,
                    INACTIVITY_STDERR_JOIN_TIMEOUT,
                )
                .await
                .or(stdout_error)
                .or(last_stdout)
                .unwrap_or_else(|| "no diagnostic output".into());
                let error = inactivity_error(
                    &program,
                    inactivity_timeout,
                    &diagnostic,
                    &secrets,
                );
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        let line = match record {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                terminate_process_group(&mut child).await;
                stdin_task.abort();
                stderr_task.abort();
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        if !inactivity.observe_record(&line) {
            continue;
        }
        last_stdout = Some(control::bounded_error(&line, &secrets));
        if let Some(error) = jsonl_error(&line) {
            stdout_error = Some(control::bounded_error(&error, &secrets));
        }
        let sent = tokio::select! {
            _ = &mut cancel => false,
            result = sender.send(Ok(line)) => result.is_ok(),
        };
        if !sent {
            terminate_process_group(&mut child).await;
            stdin_task.abort();
            stderr_task.abort();
            return;
        }
    }

    let status = tokio::select! {
        _ = &mut cancel => {
            terminate_process_group(&mut child).await;
            stdin_task.abort();
            stderr_task.abort();
            return;
        }
        _ = inactivity.wait() => {
            terminate_process_group(&mut child).await;
            stdin_task.abort();
            let diagnostic = bounded_stderr_diagnostic(
                &program,
                &mut stderr_task,
                INACTIVITY_STDERR_JOIN_TIMEOUT,
            )
            .await
            .or(stdout_error)
            .or(last_stdout)
            .unwrap_or_else(|| "no diagnostic output".into());
            let error = inactivity_error(
                &program,
                inactivity_timeout,
                &diagnostic,
                &secrets,
            );
            let _ = sender.send(Err(error)).await;
            return;
        }
        status = child.wait() => status,
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            stdin_task.abort();
            stderr_task.abort();
            let _ = sender.send(Err(error.into())).await;
            return;
        }
    };
    let stdin_result = tokio::select! {
        _ = &mut cancel => {
            terminate_process_group(&mut child).await;
            stdin_task.abort();
            stderr_task.abort();
            return;
        }
        result = &mut stdin_task => result,
    };
    let stderr = tokio::select! {
        _ = &mut cancel => {
            terminate_process_group(&mut child).await;
            stderr_task.abort();
            return;
        }
        result = &mut stderr_task => result,
    };
    let stderr = match stderr {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(error)) => {
            let _ = sender.send(Err(error)).await;
            return;
        }
        Err(error) => {
            let _ = sender
                .send(Err(KuramaError::Model(format!(
                    "{program} stderr task: {error}"
                ))))
                .await;
            return;
        }
    };

    if !status.success() {
        let diagnostic = if stderr.trim().is_empty() {
            stdout_error
                .or(last_stdout)
                .unwrap_or_else(|| "no diagnostic output".into())
        } else {
            stderr
        };
        let error = KuramaError::Model(format!(
            "{program} exited with {status}: {}",
            control::bounded_error(&diagnostic, &secrets)
        ));
        let _ = sender.send(Err(error)).await;
        return;
    }
    match stdin_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = sender.send(Err(error)).await;
        }
        Err(error) => {
            let _ = sender
                .send(Err(KuramaError::Model(format!(
                    "{program} stdin task: {error}"
                ))))
                .await;
        }
    }
}

pub(crate) async fn bounded_stderr_diagnostic(
    program: &str,
    stderr_task: &mut JoinHandle<Result<String, KuramaError>>,
    timeout: Duration,
) -> Option<String> {
    match tokio::time::timeout(timeout, &mut *stderr_task).await {
        Ok(Ok(Ok(stderr))) if !stderr.trim().is_empty() => Some(stderr),
        Ok(Ok(Ok(_))) => None,
        Ok(Ok(Err(error))) => Some(error.to_string()),
        Ok(Err(error)) => Some(format!("{program} stderr task: {error}")),
        Err(_) => {
            stderr_task.abort();
            None
        }
    }
}

pub(crate) fn inactivity_error(
    program: &str,
    inactivity_timeout: Duration,
    diagnostic: &str,
    secrets: &[String],
) -> KuramaError {
    control::bounded_kurama_error(
        KuramaError::Model(format!(
            "{program} CLI inactive for {} ms: {diagnostic}",
            inactivity_timeout.as_millis()
        )),
        secrets,
    )
}

async fn write_stdin(mut stdin: ChildStdin, input: String) -> Result<(), KuramaError> {
    stdin.write_all(input.as_bytes()).await?;
    stdin.shutdown().await?;
    Ok(())
}

fn jsonl_error(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

struct BoundedJsonlReader<R> {
    reader: R,
    buffer: Vec<u8>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> BoundedJsonlReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            eof: false,
        }
    }

    async fn next_record(&mut self, program: &str) -> Result<Option<String>, KuramaError> {
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                if newline > MAX_JSONL_LINE_BYTES {
                    return Err(oversized_record(program));
                }
                let mut record = self.buffer.drain(..=newline).collect::<Vec<_>>();
                record.pop();
                if record.last() == Some(&b'\r') {
                    record.pop();
                }
                return String::from_utf8(record).map(Some).map_err(|_| {
                    KuramaError::Protocol(format!("{program} emitted non-UTF-8 JSONL"))
                });
            }
            if self.eof {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                if self.buffer.len() > MAX_JSONL_LINE_BYTES {
                    return Err(oversized_record(program));
                }
                let record = std::mem::take(&mut self.buffer);
                return String::from_utf8(record).map(Some).map_err(|_| {
                    KuramaError::Protocol(format!("{program} emitted non-UTF-8 JSONL"))
                });
            }

            let mut chunk = [0_u8; 8192];
            let read = self.reader.read(&mut chunk).await?;
            if read == 0 {
                self.eof = true;
                continue;
            }
            let first_newline = chunk[..read].iter().position(|byte| *byte == b'\n');
            let record_growth = first_newline.unwrap_or(read);
            if self.buffer.len().saturating_add(record_growth) > MAX_JSONL_LINE_BYTES {
                return Err(oversized_record(program));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

fn oversized_record(program: &str) -> KuramaError {
    KuramaError::Protocol(format!("{program} emitted an oversized JSONL record"))
}

async fn drain_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<String, KuramaError> {
    let mut output = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len() < limit {
            let remaining = limit - output.len();
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

async fn terminate_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        signal_process_group(pid, libc::SIGTERM);
        tokio::time::sleep(Duration::from_millis(100)).await;
        if process_group_exists(pid) {
            signal_process_group(pid, libc::SIGKILL);
        }
        let _ = child.wait().await;
        return;
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) {
    let _ = unsafe { libc::kill(-(pid as i32), signal) };
}

#[cfg(unix)]
fn process_group_exists(pid: u32) -> bool {
    if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
