use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use kurama_protocol::{
    KuramaError,
    runtime::RuntimeEvent,
    tool::{
        CommandClass, Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult,
        split_shell_commands,
    },
    traits::{BoxFuture, CancelSignal, EventSink, Tool},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

use super::{BoundedText, PathGuard, limits::staged_output};

const MAX_COMMAND_BYTES: usize = 32_768;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const EVENT_CHUNK_BYTES: usize = 4 * 1024;
const DISPLAY_STAGING_KEY: &str = "_display_staging";

pub struct BashTool {
    shell_path: PathBuf,
    event_sink: Option<Arc<dyn EventSink>>,
}

impl BashTool {
    pub fn new(shell_path: impl Into<PathBuf>) -> Self {
        Self {
            shell_path: shell_path.into(),
            event_sink: None,
        }
    }

    pub fn with_event_sink(shell_path: impl Into<PathBuf>, event_sink: Arc<dyn EventSink>) -> Self {
        Self {
            shell_path: shell_path.into(),
            event_sink: Some(event_sink),
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new("/bin/bash")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArguments {
    command: String,
    cwd: String,
    timeout_ms: u64,
}

impl Tool for BashTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".into(),
            description: "Run one bounded Bash command in the workspace.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "maxLength": 32768},
                    "cwd": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 3600000}
                },
                "required": ["command", "cwd", "timeout_ms"],
                "additionalProperties": false
            }),
        }
    }

    fn classify(
        &self,
        context: &ToolContext,
        invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError> {
        let arguments = parse_arguments(invocation)?;
        let cwd = PathGuard::new(context)?
            .resolve_workspace_directory(&arguments.cwd)?
            .absolute;
        Ok(Operation::Bash {
            class: classify_command(&arguments.command),
            command: arguments.command,
            cwd,
            timeout_ms: arguments.timeout_ms,
        })
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(KuramaError::Cancelled);
            }
            let arguments = parse_arguments(&invocation)?;
            let cwd = PathGuard::new(&context)?
                .resolve_workspace_directory(&arguments.cwd)?
                .absolute;
            let started = Instant::now();
            let mut command = Command::new(&self.shell_path);
            command
                .arg("--noprofile")
                .arg("--norc")
                .arg("-c")
                .arg(&arguments.command)
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(false)
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .env("TERM", "dumb")
                .env("NO_COLOR", "1")
                .env("PAGER", "cat")
                .env("GIT_PAGER", "cat");
            configure_process_group(&mut command);

            let mut child = command.spawn().map_err(|error| {
                KuramaError::Tool(format!(
                    "failed to spawn {}: {error}",
                    self.shell_path.display()
                ))
            })?;
            let pid = child.id().ok_or_else(|| {
                KuramaError::Tool("spawned Bash process has no process identifier".into())
            })?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| KuramaError::Tool("Bash stdout pipe is unavailable".into()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| KuramaError::Tool("Bash stderr pipe is unavailable".into()))?;
            let event_sink = if context.agent_id.is_none() {
                self.event_sink.clone()
            } else {
                None
            };
            let mut stdout_task = tokio::spawn(capture_stream(
                stdout,
                context.limits,
                invocation.call_id.clone(),
                "stdout",
                event_sink.clone(),
            ));
            let mut stderr_task = tokio::spawn(capture_stream(
                stderr,
                context.limits,
                invocation.call_id.clone(),
                "stderr",
                event_sink,
            ));

            let mut status = None;
            let mut stdout = None;
            let mut stderr = None;
            let mut wait = Box::pin(child.wait());
            let deadline = tokio::time::sleep(Duration::from_millis(arguments.timeout_ms));
            tokio::pin!(deadline);
            let outcome = loop {
                if status.is_some() && stdout.is_some() && stderr.is_some() {
                    break WaitOutcome::Completed;
                }
                tokio::select! {
                    result = &mut wait, if status.is_none() => status = Some(result?),
                    result = &mut stdout_task, if stdout.is_none() => stdout = Some(join_capture(result)?),
                    result = &mut stderr_task, if stderr.is_none() => stderr = Some(join_capture(result)?),
                    _ = &mut deadline => break WaitOutcome::TimedOut,
                    _ = cancel.cancelled() => break WaitOutcome::Cancelled,
                }
            };
            drop(wait);

            let (status, mut stdout, mut stderr, timed_out) = match outcome {
                WaitOutcome::Completed => (
                    Some(status.expect("completed command has an exit status")),
                    stdout.expect("completed command has captured stdout"),
                    stderr.expect("completed command has captured stderr"),
                    false,
                ),
                WaitOutcome::TimedOut => {
                    terminate_process_group(&mut child, pid).await?;
                    if status.is_none() {
                        status = Some(child.wait().await?);
                    }
                    let (stdout, stderr) = finish_remaining_capture(
                        &mut stdout_task,
                        &mut stderr_task,
                        stdout,
                        stderr,
                    )
                    .await?;
                    (status, stdout, stderr, true)
                }
                WaitOutcome::Cancelled => {
                    terminate_process_group(&mut child, pid).await?;
                    if status.is_none() {
                        child.wait().await?;
                    }
                    finish_remaining_capture(&mut stdout_task, &mut stderr_task, stdout, stderr)
                        .await?;
                    return Err(KuramaError::Cancelled);
                }
            };
            let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let exit_code = status.as_ref().and_then(ExitStatus::code);
            let signal = status.as_ref().and_then(exit_signal);
            let is_error = timed_out || status.as_ref().is_none_or(|status| !status.success());
            let truncated = stdout.truncated || stderr.truncated;
            let timeout_message =
                timed_out.then(|| format!("command timed out after {} ms", arguments.timeout_ms));
            let mut output = combined_output(&stdout.text, &stderr.text);
            if let Some(timeout_message) = &timeout_message {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(timeout_message);
            }
            let mut metadata = serde_json::json!({
                "command_class": classify_command(&arguments.command),
                "cwd": cwd,
                "exit_code": exit_code,
                "signal": signal,
                "elapsed_ms": elapsed_ms,
                "timed_out": timed_out,
                "stdout": stdout.text.clone(),
                "stderr": stderr.text.clone(),
                "stdout_truncated": stdout.truncated,
                "stderr_truncated": stderr.truncated,
                "stdout_total_bytes": stdout.total_bytes,
                "stderr_total_bytes": stderr.total_bytes,
                "stdout_omitted_bytes": stdout.omitted_bytes,
                "stderr_omitted_bytes": stderr.omitted_bytes,
                "stdout_omitted_lines": stdout.omitted_lines,
                "stderr_omitted_lines": stderr.omitted_lines
            });
            if let Some(timeout_message) = timeout_message {
                metadata["execution_error"] = serde_json::Value::Bool(true);
                metadata["display_output"] = serde_json::Value::String(timeout_message);
            }
            if truncated {
                let mut staging = serde_json::Map::new();
                if let Some(path) = stdout.take_staged_path() {
                    staging.insert(
                        "stdout".into(),
                        serde_json::Value::String(path.display().to_string()),
                    );
                }
                if let Some(path) = stderr.take_staged_path() {
                    staging.insert(
                        "stderr".into(),
                        serde_json::Value::String(path.display().to_string()),
                    );
                }
                if !staging.is_empty() {
                    metadata[DISPLAY_STAGING_KEY] = serde_json::Value::Object(staging);
                }
            }

            Ok(ToolResult {
                call_id: invocation.call_id,
                output,
                is_error,
                metadata,
                truncated,
                blob_refs: Vec::new(),
            })
        })
    }
}

enum WaitOutcome {
    Completed,
    TimedOut,
    Cancelled,
}

fn parse_arguments(invocation: &ToolInvocation) -> Result<BashArguments, KuramaError> {
    if invocation.name != "bash" {
        return Err(KuramaError::Tool(format!(
            "bash tool received invocation for {}",
            invocation.name
        )));
    }
    let arguments: BashArguments = serde_json::from_value(invocation.arguments.clone())
        .map_err(|error| KuramaError::Tool(format!("invalid bash arguments: {error}")))?;
    if arguments.command.is_empty() || arguments.command.len() > MAX_COMMAND_BYTES {
        return Err(KuramaError::Tool(format!(
            "command must contain between 1 and {MAX_COMMAND_BYTES} bytes"
        )));
    }
    if arguments.cwd.is_empty() {
        return Err(KuramaError::Tool("bash cwd must not be empty".into()));
    }
    if !(1..=MAX_TIMEOUT_MS).contains(&arguments.timeout_ms) {
        return Err(KuramaError::Tool(format!(
            "timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"
        )));
    }
    Ok(arguments)
}

fn classify_command(command: &str) -> CommandClass {
    let Some(segments) = split_shell_commands(command) else {
        return CommandClass::Unknown;
    };
    let mut class = CommandClass::ReadOnly;
    for segment in segments {
        match classify_simple_command(segment) {
            CommandClass::Mutating => return CommandClass::Mutating,
            CommandClass::Unknown => class = CommandClass::Unknown,
            CommandClass::ReadOnly => {}
        }
    }
    class
}

fn classify_simple_command(command: &str) -> CommandClass {
    let Some(tokens) = shlex::split(command) else {
        return CommandClass::Unknown;
    };
    let Some(program) = tokens.first().and_then(|token| {
        std::path::Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
    }) else {
        return CommandClass::Unknown;
    };
    match program {
        "basename" | "cat" | "dirname" | "file" | "grep" | "head" | "ls" | "printf" | "pwd"
        | "realpath" | "rg" | "stat" | "tail" | "uniq" | "wc" => CommandClass::ReadOnly,
        "sort" => {
            if tokens
                .iter()
                .skip(1)
                .any(|token| token == "-o" || token.starts_with("--output="))
            {
                CommandClass::Mutating
            } else {
                CommandClass::ReadOnly
            }
        }
        "find" => {
            if tokens.iter().skip(1).any(|token| {
                matches!(
                    token.as_str(),
                    "-delete"
                        | "-exec"
                        | "-execdir"
                        | "-fprint"
                        | "-fprint0"
                        | "-fls"
                        | "-ok"
                        | "-okdir"
                )
            }) {
                CommandClass::Mutating
            } else {
                CommandClass::ReadOnly
            }
        }
        "sed" => {
            if tokens
                .iter()
                .skip(1)
                .any(|token| token == "--in-place" || token.starts_with("-i"))
            {
                CommandClass::Mutating
            } else {
                CommandClass::ReadOnly
            }
        }
        "git" => match tokens.get(1).map(String::as_str) {
            Some(
                "diff" | "grep" | "log" | "ls-files" | "ls-tree" | "rev-parse" | "show" | "status",
            ) => CommandClass::ReadOnly,
            Some(
                "add" | "am" | "apply" | "branch" | "checkout" | "cherry-pick" | "clean" | "clone"
                | "commit" | "fetch" | "init" | "merge" | "mv" | "pull" | "push" | "rebase"
                | "reset" | "restore" | "revert" | "rm" | "switch" | "tag",
            ) => CommandClass::Mutating,
            _ => CommandClass::Unknown,
        },
        "rm" | "mv" | "cp" | "mkdir" | "rmdir" | "touch" | "chmod" | "chown" | "ln" | "install"
        | "truncate" | "tee" | "dd" => CommandClass::Mutating,
        _ => CommandClass::Unknown,
    }
}

async fn capture_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    limits: kurama_protocol::tool::ToolLimits,
    call_id: kurama_protocol::id::CallId,
    stream: &'static str,
    event_sink: Option<Arc<dyn EventSink>>,
) -> Result<CapturedStream, KuramaError> {
    let mut bounded = if event_sink.is_some() {
        staged_output(limits, "bash", stream)?
    } else {
        super::BoundedOutput::new(limits)
    };
    let mut pending_utf8 = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        bounded.push(&chunk[..read]);
        if let Some(sink) = &event_sink {
            emit_decoded_output(
                &mut pending_utf8,
                &chunk[..read],
                false,
                sink.as_ref(),
                &call_id,
                stream,
            );
        }
    }
    if let Some(sink) = &event_sink {
        emit_decoded_output(
            &mut pending_utf8,
            &[],
            true,
            sink.as_ref(),
            &call_id,
            stream,
        );
    }
    Ok(CapturedStream(bounded.finish()))
}

fn emit_decoded_output(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    flush: bool,
    sink: &dyn EventSink,
    call_id: &kurama_protocol::id::CallId,
    stream: &str,
) {
    pending.extend_from_slice(bytes);
    let mut consumed = 0;
    let mut visible = String::new();
    loop {
        let remaining = &pending[consumed..];
        if remaining.is_empty() {
            break;
        }
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                visible.push_str(text);
                consumed = pending.len();
                break;
            }
            Err(error) => {
                let valid_end = consumed + error.valid_up_to();
                if valid_end > consumed {
                    let prefix = std::str::from_utf8(&pending[consumed..valid_end])
                        .expect("validated UTF-8 prefix");
                    visible.push_str(prefix);
                    consumed = valid_end;
                }
                if let Some(error_len) = error.error_len() {
                    visible.push('�');
                    consumed += error_len;
                    continue;
                }
                if flush {
                    visible.push_str(&String::from_utf8_lossy(&pending[consumed..]));
                    consumed = pending.len();
                }
                break;
            }
        }
    }
    pending.drain(..consumed);
    emit_visible_output(sink, call_id, stream, &visible);
}

fn emit_visible_output(
    sink: &dyn EventSink,
    call_id: &kurama_protocol::id::CallId,
    stream: &str,
    visible: &str,
) {
    for visible in utf8_chunks(visible, EVENT_CHUNK_BYTES) {
        let _ = sink.emit(RuntimeEvent::ToolOutputDelta {
            call_id: call_id.clone(),
            stream: stream.into(),
            chunk: visible.to_owned(),
        });
    }
}

struct CapturedStream(BoundedText);

impl CapturedStream {
    fn take_staged_path(&mut self) -> Option<PathBuf> {
        self.0.staged_path.take()
    }
}

impl std::ops::Deref for CapturedStream {
    type Target = BoundedText;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for CapturedStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for CapturedStream {
    fn drop(&mut self) {
        if let Some(path) = &self.0.staged_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn finish_remaining_capture(
    stdout_task: &mut tokio::task::JoinHandle<Result<CapturedStream, KuramaError>>,
    stderr_task: &mut tokio::task::JoinHandle<Result<CapturedStream, KuramaError>>,
    stdout: Option<CapturedStream>,
    stderr: Option<CapturedStream>,
) -> Result<(CapturedStream, CapturedStream), KuramaError> {
    let stdout = match stdout {
        Some(stdout) => stdout,
        None => join_capture(stdout_task.await)?,
    };
    let stderr = match stderr {
        Some(stderr) => stderr,
        None => join_capture(stderr_task.await)?,
    };
    Ok((stdout, stderr))
}

fn join_capture(
    result: Result<Result<CapturedStream, KuramaError>, tokio::task::JoinError>,
) -> Result<CapturedStream, KuramaError> {
    result.map_err(|error| KuramaError::Tool(format!("output capture task failed: {error}")))?
}

fn combined_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (true, true) => String::new(),
        (false, false) => format!("{stdout}\n[stderr]\n{stderr}"),
    }
}

fn utf8_chunks(value: &str, max_bytes: usize) -> Vec<&str> {
    if value.is_empty() || max_bytes == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < value.len() {
        let mut end = (start + max_bytes).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = value[start..]
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(offset, _)| start + offset);
        }
        chunks.push(&value[start..end]);
        start = end;
    }
    chunks
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
async fn terminate_process_group(child: &mut Child, pid: u32) -> Result<ExitStatus, KuramaError> {
    let pid = i32::try_from(pid)
        .map_err(|_| KuramaError::Tool("Bash process identifier exceeds i32".into()))?;
    signal_process_group(pid, libc::SIGTERM)?;
    match tokio::time::timeout(Duration::from_millis(50), child.wait()).await {
        Ok(status) => {
            let status = status?;
            if process_group_is_owned(pid)? {
                signal_process_group(pid, libc::SIGKILL)?;
            }
            Ok(status)
        }
        Err(_) => {
            if process_group_is_owned(pid)? {
                signal_process_group(pid, libc::SIGKILL)?;
            }
            Ok(child.wait().await?)
        }
    }
}

#[cfg(unix)]
fn signal_process_group(pid: i32, signal: i32) -> Result<(), KuramaError> {
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(KuramaError::Io(error))
    }
}

#[cfg(unix)]
fn process_group_is_owned(pid: i32) -> Result<bool, KuramaError> {
    let result = unsafe { libc::kill(-pid, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH | libc::EPERM) => Ok(false),
        _ => Err(KuramaError::Io(error)),
    }
}

#[cfg(not(unix))]
async fn terminate_process_group(child: &mut Child, _pid: u32) -> Result<ExitStatus, KuramaError> {
    child.start_kill()?;
    Ok(child.wait().await?)
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}
