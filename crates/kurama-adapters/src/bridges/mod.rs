use std::{path::PathBuf, process::Stdio, time::Duration};

use kurama_protocol::{KuramaError, model::ModelEvent, traits::CancelSignal};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

#[cfg(feature = "claude-bridge")]
pub mod claude;
#[cfg(feature = "codex-bridge")]
pub mod codex;
pub mod control;

const MAX_JSONL_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCommand {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: String,
    pub cwd: Option<PathBuf>,
}

impl BridgeCommand {
    async fn run(
        &self,
        cancel: &dyn CancelSignal,
        secrets: &[String],
    ) -> Result<Vec<String>, KuramaError> {
        if cancel.is_cancelled() {
            return Err(KuramaError::Cancelled);
        }
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
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(self.stdin.as_bytes()).await?;
            stdin.shutdown().await?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| KuramaError::Model(format!("{} stdout unavailable", self.program)))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| KuramaError::Model(format!("{} stderr unavailable", self.program)))?;
        let stderr_task = tokio::spawn(read_bounded(stderr, 16 * 1024));
        let mut lines = BufReader::new(stdout).lines();
        let mut output = Vec::new();
        loop {
            let line = tokio::select! {
                _ = cancel.cancelled() => {
                    terminate_process_group(&mut child).await;
                    return Err(KuramaError::Cancelled);
                }
                line = lines.next_line() => line?,
            };
            let Some(line) = line else { break };
            if line.len() > MAX_JSONL_LINE_BYTES {
                terminate_process_group(&mut child).await;
                return Err(KuramaError::Protocol(format!(
                    "{} emitted an oversized JSONL record",
                    self.program
                )));
            }
            if !line.trim().is_empty() {
                output.push(line);
            }
        }
        let status = tokio::select! {
            _ = cancel.cancelled() => {
                terminate_process_group(&mut child).await;
                return Err(KuramaError::Cancelled);
            }
            status = child.wait() => status?,
        };
        let stderr = stderr_task.await.map_err(|error| {
            KuramaError::Model(format!("{} stderr task: {error}", self.program))
        })??;
        if !status.success() {
            let diagnostic = if stderr.trim().is_empty() {
                jsonl_error(&output).unwrap_or_else(|| output.join("\n"))
            } else {
                stderr
            };
            return Err(KuramaError::Model(format!(
                "{} exited with {}: {}",
                self.program,
                status,
                control::bounded_error(&diagnostic, secrets)
            )));
        }
        Ok(output)
    }
}

fn jsonl_error(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    })
}

pub(crate) fn event_stream(
    events: Vec<Result<ModelEvent, KuramaError>>,
) -> kurama_protocol::traits::ModelStream {
    Box::pin(futures_util::stream::iter(events))
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<String, KuramaError> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    while output.len() < limit {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit - output.len();
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

async fn terminate_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}
