use std::{fs, time::UNIX_EPOCH};

use super::{BoundedOutput, PathGuard, limits::sha256_hex};
use kurama_protocol::{
    KuramaError,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};
use serde::Deserialize;

const MAX_FILES: usize = 16;
const MAX_PRE_READ_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct ReadTool {
    _private: (),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    files: Vec<ReadFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFile {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
}

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
        let mut selected = None;
        let mut external = false;
        for file in arguments.files {
            validate_range(&file)?;
            let path = guard.resolve_existing_file(&file.path)?;
            if selected.is_none() || path.external {
                selected = Some(path.absolute);
            }
            external |= path.external;
        }
        Ok(Operation::Read {
            path: selected.expect("validated non-empty files"),
            external,
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
            let guard = PathGuard::new(&context)?;
            let mut aggregate = BoundedOutput::new(context.limits);
            let mut files = Vec::with_capacity(arguments.files.len());
            let mut truncated = false;
            let mut blob_refs = Vec::new();

            for file in arguments.files {
                if cancel.is_cancelled() {
                    return Err(KuramaError::Cancelled);
                }
                let range = validate_range(&file)?;
                let guarded = guard.resolve_existing_file(&file.path)?;
                let metadata = fs::metadata(&guarded.absolute)?;
                if metadata.len() > MAX_PRE_READ_BYTES {
                    return Err(KuramaError::Tool(format!(
                        "file exceeds {MAX_PRE_READ_BYTES} byte read ceiling: {}",
                        guarded.absolute.display()
                    )));
                }
                let bytes = fs::read(&guarded.absolute)?;
                let sha256 = sha256_hex(&bytes);
                let utf8 = std::str::from_utf8(&bytes).is_ok();
                let (selected, range_metadata) = select_range(&bytes, range)?;
                let mut bounded = BoundedOutput::new(context.limits);
                bounded.push(selected);
                let bounded = bounded.finish();
                let heading = format!("== {} ==\n", file.path);
                aggregate.push(heading.as_bytes());
                aggregate.push(bounded.text.as_bytes());
                if !bounded.text.ends_with('\n') {
                    aggregate.push(b"\n");
                }
                truncated |= bounded.truncated;
                if let Some(reference) = bounded.blob_ref.clone() {
                    blob_refs.push(reference);
                }

                files.push(serde_json::json!({
                    "path": file.path,
                    "absolute_path": guarded.absolute,
                    "external": guarded.external,
                    "range": range_metadata,
                    "total_bytes": bytes.len(),
                    "selected_bytes": selected.len(),
                    "modified_unix_ms": modified_unix_ms(&metadata),
                    "sha256": sha256,
                    "utf8": utf8,
                    "lossy": !utf8,
                    "truncated": bounded.truncated,
                    "omitted_bytes": bounded.omitted_bytes,
                    "omitted_lines": bounded.omitted_lines
                }));
            }

            let aggregate = aggregate.finish();
            truncated |= aggregate.truncated;
            if let Some(reference) = aggregate.blob_ref.clone() {
                blob_refs.push(reference);
            }
            Ok(ToolResult {
                call_id: invocation.call_id,
                output: aggregate.text,
                is_error: false,
                metadata: serde_json::json!({
                    "files": files,
                    "total_bytes": aggregate.total_bytes,
                    "total_lines": aggregate.total_lines,
                    "omitted_bytes": aggregate.omitted_bytes,
                    "omitted_lines": aggregate.omitted_lines
                }),
                truncated,
                blob_refs,
            })
        })
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

fn select_range(
    bytes: &[u8],
    range: RequestedRange,
) -> Result<(&[u8], serde_json::Value), KuramaError> {
    match range {
        RequestedRange::Bytes { start, end } => {
            if end > bytes.len() {
                return Err(KuramaError::Tool(format!(
                    "byte range {start}..{end} exceeds file length {}",
                    bytes.len()
                )));
            }
            Ok((
                &bytes[start..end],
                serde_json::json!({"kind": "bytes", "start": start, "end": end}),
            ))
        }
        RequestedRange::Lines { start, end } => {
            let mut starts = vec![0];
            for (index, &byte) in bytes.iter().enumerate() {
                if byte == b'\n' && index + 1 < bytes.len() {
                    starts.push(index + 1);
                }
            }
            if bytes.is_empty() || start > starts.len() {
                return Err(KuramaError::Tool(format!(
                    "line {start} exceeds file line count {}",
                    starts.len().saturating_sub(usize::from(bytes.is_empty()))
                )));
            }
            let actual_end = end.min(starts.len());
            let start_offset = starts[start - 1];
            let end_offset = if actual_end < starts.len() {
                starts[actual_end]
            } else {
                bytes.len()
            };
            Ok((
                &bytes[start_offset..end_offset],
                serde_json::json!({
                    "kind": "lines",
                    "start": start,
                    "end": actual_end
                }),
            ))
        }
    }
}

fn modified_unix_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
