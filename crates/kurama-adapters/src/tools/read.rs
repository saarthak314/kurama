use std::{fs, io::Read, sync::atomic::AtomicBool, time::UNIX_EPOCH};

use crate::fs_safe::{blocking, checkpoint};
use sha2::{Digest, Sha256};

use super::{
    BoundedOutput, PathGuard,
    limits::{staged_output, take_truncated_staging},
};
use kurama_protocol::{
    KuramaError,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};
use serde::Deserialize;

const MAX_FILES: usize = 16;
const MAX_PRE_READ_BYTES: u64 = 16 * 1024 * 1024;
const SCAN_BYTES: usize = 64 * 1024;

#[derive(Debug, Default)]
pub struct ReadTool {
    _private: (),
}

#[derive(Debug, Deserialize)]
struct ReadArguments {
    files: Vec<ReadFile>,
}

#[derive(Debug, Deserialize)]
struct ReadFile {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
}

#[derive(Clone, Copy)]
enum RequestedRange {
    Lines { start: usize, end: usize },
    Bytes { start: usize, end: usize },
}

impl Tool for ReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read".into(),
            description: "Read explicit line or byte ranges from up to 16 files.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "maxItems": 16,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "start_line": {"type": "integer", "minimum": 1},
                                "end_line": {"type": "integer", "minimum": 1},
                                "start_byte": {"type": "integer", "minimum": 0},
                                "end_byte": {"type": "integer", "minimum": 0}
                            },
                            "required": ["path"],
                            "oneOf": [
                                {"required": ["start_line", "end_line"]},
                                {"required": ["start_byte", "end_byte"]}
                            ],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["files"],
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
        let guard = PathGuard::new(context)?;
        let mut paths = Vec::with_capacity(arguments.files.len());
        let mut external = false;
        for file in arguments.files {
            validate_range(&file)?;
            let path = guard.resolve_existing_file(&file.path)?;
            paths.push(path.absolute);
            external |= path.external;
        }
        Ok(Operation::Read { paths, external })
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(blocking(cancel, move |cancel| {
            let arguments = parse_arguments(&invocation)?;
            let guard = PathGuard::new(&context)?;
            let mut aggregate = staged_output(context.limits, "read", "output")?;
            let mut files = Vec::with_capacity(arguments.files.len());

            for file in arguments.files {
                checkpoint(&cancel)?;
                let range = validate_range(&file)?;
                let guarded = guard.resolve_existing(&file.path)?;
                let mut input = guarded.open_file()?;
                let metadata = input.metadata()?;
                if metadata.len() > MAX_PRE_READ_BYTES {
                    return Err(KuramaError::Tool(format!(
                        "file exceeds {MAX_PRE_READ_BYTES} byte read ceiling: {}",
                        guarded.absolute.display()
                    )));
                }
                let scan = select_range(&mut input, range, &cancel)?;
                let selected = scan.selected.as_slice();
                let mut bounded = BoundedOutput::new(context.limits);
                bounded.push(selected);
                let bounded = bounded.finish();
                let heading = format!("== {} ==\n", file.path);
                aggregate.push(heading.as_bytes());
                aggregate.push(selected);
                if !selected.ends_with(b"\n") {
                    aggregate.push(b"\n");
                }

                files.push(serde_json::json!({
                    "path": file.path,
                    "absolute_path": guarded.absolute,
                    "external": guarded.external,
                    "range": scan.range,
                    "total_bytes": scan.total_bytes,
                    "selected_bytes": selected.len(),
                    "modified_unix_ms": modified_unix_ms(&metadata),
                    "sha256": scan.sha256,
                    "utf8": scan.utf8,
                    "lossy": !scan.utf8,
                    "truncated": bounded.truncated,
                    "omitted_bytes": bounded.omitted_bytes,
                    "omitted_lines": bounded.omitted_lines
                }));
            }

            checkpoint(&cancel)?;
            let mut aggregate = aggregate.finish();
            let staged_path = take_truncated_staging(&mut aggregate)?;
            let mut metadata = serde_json::json!({
                "files": files,
                "total_bytes": aggregate.total_bytes,
                "total_lines": aggregate.total_lines,
                "omitted_bytes": aggregate.omitted_bytes,
                "omitted_lines": aggregate.omitted_lines
            });
            if let Some(path) = staged_path {
                metadata["_display_staging"] = serde_json::json!({
                    "output": path
                });
            }
            Ok(ToolResult {
                call_id: invocation.call_id,
                output: aggregate.text,
                is_error: false,
                metadata,
                truncated: aggregate.truncated,
                blob_refs: Vec::new(),
            })
        }))
    }
}

fn parse_arguments(invocation: &ToolInvocation) -> Result<ReadArguments, KuramaError> {
    if invocation.name != "read" {
        return Err(KuramaError::Tool(format!(
            "read tool received invocation for {}",
            invocation.name
        )));
    }
    let arguments: ReadArguments = serde_json::from_value(invocation.arguments.clone())
        .map_err(|error| KuramaError::Tool(format!("invalid read arguments: {error}")))?;
    if arguments.files.is_empty() || arguments.files.len() > MAX_FILES {
        return Err(KuramaError::Tool(format!(
            "read requires between 1 and {MAX_FILES} files"
        )));
    }
    Ok(arguments)
}

fn validate_range(file: &ReadFile) -> Result<RequestedRange, KuramaError> {
    if file.path.is_empty() {
        return Err(KuramaError::Tool("read path must not be empty".into()));
    }
    match (
        file.start_line,
        file.end_line,
        file.start_byte,
        file.end_byte,
    ) {
        (Some(start), Some(end), None, None) if start >= 1 && start <= end => {
            Ok(RequestedRange::Lines { start, end })
        }
        (None, None, Some(start), Some(end)) if start <= end => {
            Ok(RequestedRange::Bytes { start, end })
        }
        _ => Err(KuramaError::Tool(format!(
            "{} must specify one complete, non-inverted line or byte range",
            file.path
        ))),
    }
}

struct SelectedRange {
    selected: Vec<u8>,
    range: serde_json::Value,
    total_bytes: usize,
    sha256: String,
    utf8: bool,
}

fn select_range(
    input: &mut impl Read,
    range: RequestedRange,
    cancel: &AtomicBool,
) -> Result<SelectedRange, KuramaError> {
    let mut selected = Vec::new();
    let mut digest = Sha256::new();
    let mut total_bytes = 0_usize;
    let mut line = 1_usize;
    let mut terminal_newline = false;
    let mut utf8 = true;
    let mut pending = 0;
    let mut buffer = [0_u8; SCAN_BYTES + 3];
    loop {
        checkpoint(cancel)?;
        let count = match input.read(&mut buffer[pending..pending + SCAN_BYTES]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        let next_total = total_bytes
            .checked_add(count)
            .filter(|total| *total as u64 <= MAX_PRE_READ_BYTES)
            .ok_or_else(|| {
                KuramaError::Tool(format!(
                    "file exceeds {MAX_PRE_READ_BYTES} byte read ceiling"
                ))
            })?;
        let bytes = &buffer[pending..pending + count];
        digest.update(bytes);
        match range {
            RequestedRange::Bytes { start, end } => {
                let first = start.saturating_sub(total_bytes).min(count);
                let last = end.saturating_sub(total_bytes).min(count);
                selected.extend_from_slice(&bytes[first..last]);
            }
            RequestedRange::Lines { start, end } => {
                for part in bytes.split_inclusive(|byte| *byte == b'\n') {
                    if (start..=end).contains(&line) {
                        selected.extend_from_slice(part);
                    }
                    if part.last() == Some(&b'\n') {
                        line += 1;
                    }
                }
            }
        }
        terminal_newline = bytes.last() == Some(&b'\n');
        total_bytes = next_total;
        if utf8 {
            let length = pending + count;
            match std::str::from_utf8(&buffer[..length]) {
                Ok(_) => pending = 0,
                Err(error) if error.error_len().is_none() => {
                    pending = length - error.valid_up_to();
                    buffer.copy_within(error.valid_up_to()..length, 0);
                }
                Err(_) => {
                    utf8 = false;
                    pending = 0;
                }
            }
        }
    }
    utf8 &= pending == 0;
    let range = match range {
        RequestedRange::Bytes { start, end } => {
            if end > total_bytes {
                return Err(KuramaError::Tool(format!(
                    "byte range {start}..{end} exceeds file length {total_bytes}"
                )));
            }
            serde_json::json!({"kind": "bytes", "start": start, "end": end})
        }
        RequestedRange::Lines { start, end } => {
            let lines = if total_bytes == 0 {
                0
            } else {
                line - usize::from(terminal_newline)
            };
            if start > lines {
                return Err(KuramaError::Tool(format!(
                    "line {start} exceeds file line count {lines}"
                )));
            }
            serde_json::json!({"kind": "lines", "start": start, "end": end.min(lines)})
        }
    };
    Ok(SelectedRange {
        selected,
        range,
        total_bytes,
        sha256: crate::id::hexadecimal("", digest.finalize().as_ref()),
        utf8,
    })
}

fn modified_unix_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_growth_cannot_bypass_the_read_ceiling_for_a_tiny_selection() {
        struct GrowingFile(usize);
        impl Read for GrowingFile {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let count = bytes.len().min(self.0);
                bytes[..count].fill(b'x');
                self.0 -= count;
                Ok(count)
            }
        }
        let mut input = GrowingFile(MAX_PRE_READ_BYTES as usize + 1);
        let result = select_range(
            &mut input,
            RequestedRange::Bytes { start: 0, end: 1 },
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(KuramaError::Tool(_))));
    }
}
