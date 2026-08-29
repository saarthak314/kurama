use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{PathGuard, limits::sha256_hex};
use diffy::Patch;
use kurama_protocol::{
    KuramaError,
    policy::ExecutionMode,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};
use serde::Deserialize;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default)]
pub struct WriteTool {
    _private: (),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArguments {
    path: String,
    expected_sha256: Option<String>,
    content: Option<String>,
    patch: Option<String>,
}

impl Tool for WriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write".into(),
            description: "Atomically create, replace, or patch one file.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "expected_sha256": {"type": "string", "minLength": 64, "maxLength": 64},
                    "content": {"type": "string"},
                    "patch": {"type": "string"}
                },
                "required": ["path"],
                "oneOf": [
                    {"required": ["content"], "not": {"required": ["patch"]}},
                    {"required": ["patch"], "not": {"required": ["content"]}}
                ],
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
        let guarded =
            PathGuard::new(context)?.resolve_write(&arguments.path, &context.write_scope)?;
        Ok(Operation::Write {
            paths: vec![guarded.absolute.clone()],
            destructive: guarded.absolute.exists(),
            external: guarded.external,
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
            let guarded =
                PathGuard::new(&context)?.resolve_write(&arguments.path, &context.write_scope)?;
            let existed = guarded.absolute.exists();
            let original = if existed {
                fs::read(&guarded.absolute)?
            } else {
                Vec::new()
            };
            let original_hash = existed.then(|| sha256_hex(&original));

            if existed && context.mode != ExecutionMode::Yolo && arguments.expected_sha256.is_none()
            {
                return Err(KuramaError::Tool(format!(
                    "expected_sha256 is required for existing file {}",
                    guarded.absolute.display()
                )));
            }
            if let Some(expected) = &arguments.expected_sha256 {
                validate_sha256(expected)?;
                if original_hash.as_deref() != Some(expected.as_str()) {
                    return Err(KuramaError::Tool(format!(
                        "stale expected_sha256 for {}",
                        guarded.absolute.display()
                    )));
                }
            }

            let post_image = match (arguments.content, arguments.patch) {
                (Some(content), None) => content.into_bytes(),
                (None, Some(patch)) => apply_patch(&original, &patch)?,
                _ => unreachable!("validated payload"),
            };
            let post_hash = sha256_hex(&post_image);

            if cancel.is_cancelled() {
                return Err(KuramaError::Cancelled);
            }
            atomic_replace(
                &guarded.absolute,
                &post_image,
                original_hash.as_deref(),
                existed,
                cancel,
            )?;

            Ok(ToolResult {
                call_id: invocation.call_id,
                output: format!(
                    "wrote {} bytes to {} ({post_hash})",
                    post_image.len(),
                    arguments.path
                ),
                is_error: false,
                metadata: serde_json::json!({
                    "path": arguments.path,
                    "absolute_path": guarded.absolute,
                    "external": guarded.external,
                    "created": !existed,
                    "bytes": post_image.len(),
                    "sha256": post_hash
                }),
                truncated: false,
                blob_refs: Vec::new(),
            })
        })
    }
}

fn parse_arguments(invocation: &ToolInvocation) -> Result<WriteArguments, KuramaError> {
    if invocation.name != "write" {
        return Err(KuramaError::Tool(format!(
            "write tool received invocation for {}",
            invocation.name
        )));
    }
    let arguments: WriteArguments = serde_json::from_value(invocation.arguments.clone())
        .map_err(|error| KuramaError::Tool(format!("invalid write arguments: {error}")))?;
    if arguments.path.is_empty() {
        return Err(KuramaError::Tool("write path must not be empty".into()));
    }
    if arguments.content.is_some() == arguments.patch.is_some() {
        return Err(KuramaError::Tool(
            "write requires exactly one of content or patch".into(),
        ));
    }
    Ok(arguments)
}

fn validate_sha256(value: &str) -> Result<(), KuramaError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(KuramaError::Tool(
            "expected_sha256 must be 64 hexadecimal characters".into(),
        ));
    }
    Ok(())
}

fn apply_patch(original: &[u8], patch: &str) -> Result<Vec<u8>, KuramaError> {
    let original = std::str::from_utf8(original)
        .map_err(|_| KuramaError::Tool("unified patches require a UTF-8 file".into()))?;
    let patch = Patch::from_str(patch)
        .map_err(|error| KuramaError::Tool(format!("invalid unified patch: {error}")))?;
    diffy::apply(original, &patch)
        .map(String::into_bytes)
        .map_err(|error| KuramaError::Tool(format!("patch does not apply: {error}")))
}

fn atomic_replace(
    target: &Path,
    post_image: &[u8],
    expected_hash: Option<&str>,
    existed: bool,
    cancel: &dyn CancelSignal,
) -> Result<(), KuramaError> {
    let parent = target
        .parent()
        .ok_or_else(|| KuramaError::Tool("write target has no parent".into()))?;
    let temp_path = temporary_path(parent);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(post_image)?;
        if existed {
            file.set_permissions(fs::metadata(target)?.permissions())?;
        }
        file.sync_all()?;

        if cancel.is_cancelled() {
            return Err(KuramaError::Cancelled);
        }
        match (expected_hash, target.exists()) {
            (Some(expected), true) if sha256_hex(&fs::read(target)?) != expected => {
                return Err(KuramaError::Tool(format!(
                    "file changed before atomic rename: {}",
                    target.display()
                )));
            }
            (Some(_), false) => {
                return Err(KuramaError::Tool(format!(
                    "file disappeared before atomic rename: {}",
                    target.display()
                )));
            }
            (None, true) if !existed => {
                return Err(KuramaError::Tool(format!(
                    "file appeared before atomic rename: {}",
                    target.display()
                )));
            }
            _ => {}
        }

        fs::rename(&temp_path, target)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn temporary_path(parent: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".kurama-{}-{sequence}.tmp", std::process::id()))
}
