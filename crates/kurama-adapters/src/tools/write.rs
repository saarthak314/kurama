use std::{
    ffi::OsStr,
    fs::{File, Permissions, TryLockError},
    io::{Read, Write},
    sync::atomic::AtomicBool,
    time::Duration,
};

use crate::fs_safe::{Directory, blocking, checkpoint};
use sha2::{Digest, Sha256};

use super::PathGuard;
use diffy::Patch;
use kurama_protocol::{
    KuramaError,
    policy::ExecutionMode,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};
use serde::Deserialize;

#[derive(Debug, Default)]
pub struct WriteTool {
    _private: (),
}

#[derive(Debug, Deserialize)]
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
        Box::pin(blocking(cancel, move |cancel| {
            let arguments = parse_arguments(&invocation)?;
            let guarded =
                PathGuard::new(&context)?.resolve_write(&arguments.path, &context.write_scope)?;
            let (directory, name) = guarded.parent_directory()?;
            // A stable parent lock serializes cooperating writers across processes,
            // including the original read and expected-hash check. Advisory locks
            // cannot provide atomic compare-and-swap against noncooperating writers.
            let _lock = lock_writes(&directory, &cancel)?;
            let original_file = match directory.open_file(&name, false, false) {
                Ok(file) => Some(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let existed = original_file.is_some();
            if existed && context.mode != ExecutionMode::Yolo && arguments.expected_sha256.is_none()
            {
                return Err(KuramaError::Tool(format!(
                    "expected_sha256 is required for existing file {}",
                    guarded.absolute.display()
                )));
            }
            let (original, original_hash, permissions) = match original_file {
                Some(mut file) => {
                    let permissions = file.metadata()?.permissions();
                    let (bytes, hash) =
                        read_original(&mut file, arguments.patch.is_some(), &cancel)?;
                    (bytes, Some(hash), Some(permissions))
                }
                None => (Vec::new(), None, None),
            };
            if let Some(expected) = &arguments.expected_sha256
                && original_hash.as_deref() != Some(expected.as_str())
            {
                return Err(KuramaError::Tool(format!(
                    "stale expected_sha256 for {}",
                    guarded.absolute.display()
                )));
            }
            checkpoint(&cancel)?;
            let post_image = match (arguments.content, arguments.patch) {
                (Some(content), None) => content.into_bytes(),
                (None, Some(patch)) => apply_patch(&original, &patch)?,
                _ => unreachable!("validated payload"),
            };
            checkpoint(&cancel)?;
            let post_hash = hash_bytes(&post_image, &cancel)?;
            atomic_replace(
                &directory,
                &name,
                &post_image,
                original_hash.as_deref(),
                permissions,
                &cancel,
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
        }))
    }
}

fn parse_arguments(invocation: &ToolInvocation) -> Result<WriteArguments, KuramaError> {
    if invocation.name != "write" {
        return Err(KuramaError::Tool(format!(
            "write tool received invocation for {}",
            invocation.name
        )));
    }
    let mut arguments: WriteArguments = serde_json::from_value(invocation.arguments.clone())
        .map_err(|error| KuramaError::Tool(format!("invalid write arguments: {error}")))?;
    if arguments.path.is_empty() {
        return Err(KuramaError::Tool("write path must not be empty".into()));
    }
    if arguments.content.is_some() == arguments.patch.is_some() {
        return Err(KuramaError::Tool(
            "write requires exactly one of content or patch".into(),
        ));
    }
    if let Some(expected) = &mut arguments.expected_sha256 {
        validate_sha256(expected)?;
        expected.make_ascii_lowercase();
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
    directory: &Directory,
    name: &OsStr,
    post_image: &[u8],
    expected_hash: Option<&str>,
    permissions: Option<Permissions>,
    cancel: &AtomicBool,
) -> Result<(), KuramaError> {
    checkpoint(cancel)?;
    let (temporary, mut file) = directory.temporary()?;
    let result = (|| {
        for chunk in post_image.chunks(64 * 1024) {
            checkpoint(cancel)?;
            file.write_all(chunk)?;
        }
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        checkpoint(cancel)?;
        let current_hash = match directory.open_file(name, false, false) {
            Ok(mut current) => Some(read_original(&mut current, false, cancel)?.1),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if current_hash.as_deref() != expected_hash {
            return Err(KuramaError::Tool(
                "file changed before atomic rename".into(),
            ));
        }
        checkpoint(cancel)?;
        // Commit begins here. Cancellation after rename must not claim no write occurred.
        directory.rename(&temporary, name)?;
        directory.sync()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = directory.remove_file(&temporary);
    }
    result
}

fn lock_writes(directory: &Directory, cancel: &AtomicBool) -> Result<File, KuramaError> {
    let lock = directory.lock_handle()?;
    loop {
        checkpoint(cancel)?;
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

fn read_original(
    file: &mut File,
    retain: bool,
    cancel: &AtomicBool,
) -> Result<(Vec<u8>, String), KuramaError> {
    let mut original = Vec::new();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        checkpoint(cancel)?;
        let count = match file.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        if retain {
            original.extend_from_slice(&buffer[..count]);
        }
    }
    Ok((
        original,
        crate::id::hexadecimal("", digest.finalize().as_ref()),
    ))
}

fn hash_bytes(bytes: &[u8], cancel: &AtomicBool) -> Result<String, KuramaError> {
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        checkpoint(cancel)?;
        digest.update(chunk);
    }
    Ok(crate::id::hexadecimal("", digest.finalize().as_ref()))
}
