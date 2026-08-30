use std::{
    collections::VecDeque,
    fmt::Write as _,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use kurama_protocol::{session::BlobRef, tool::ToolLimits};
use sha2::{Digest, Sha256};

static NEXT_STAGING_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedText {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: usize,
    pub total_lines: usize,
    pub omitted_bytes: usize,
    pub omitted_lines: usize,
    pub blob_ref: Option<BlobRef>,
    pub staged_path: Option<PathBuf>,
    pub staging_error: Option<String>,
}

struct StagingFile {
    path: PathBuf,
    file: Option<File>,
    error: Option<String>,
    preserve: bool,
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = self.file.take();
        if !self.preserve {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct BoundedOutput {
    limits: ToolLimits,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_lines: usize,
    tail_lines: usize,
    total_bytes: usize,
    total_lines: usize,
    last_byte: Option<u8>,
    hasher: Sha256,
    staging: Option<StagingFile>,
}

impl BoundedOutput {
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            head: Vec::new(),
            tail: VecDeque::new(),
            head_lines: 0,
            tail_lines: 0,
            total_bytes: 0,
            total_lines: 0,
            last_byte: None,
            hasher: Sha256::new(),
            staging: None,
        }
    }

    pub fn with_staging(limits: ToolLimits, path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        let mut output = Self::new(limits);
        output.staging = Some(StagingFile {
            path,
            file: Some(file),
            error: None,
            preserve: false,
        });
        Ok(output)
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        self.hasher.update(bytes);
        if let Some(staging) = &mut self.staging
            && staging.error.is_none()
            && let Some(file) = staging.file.as_mut()
            && let Err(error) = file.write_all(bytes)
        {
            staging.error = Some(error.to_string());
        }

        for &byte in bytes {
            if self.total_bytes == 0 || self.last_byte == Some(b'\n') {
                self.total_lines = self.total_lines.saturating_add(1);
            }
            self.total_bytes = self.total_bytes.saturating_add(1);
            self.last_byte = Some(byte);

            if self.can_push_head(byte) {
                if self.head.is_empty() || self.head.last() == Some(&b'\n') {
                    self.head_lines += 1;
                }
                self.head.push(byte);
            } else {
                self.push_tail(byte);
            }
        }
    }

    pub fn finish(mut self) -> BoundedText {
        let staging_error = self.staging.as_mut().and_then(|staging| {
            if staging.error.is_none()
                && let Some(file) = staging.file.as_mut()
                && let Err(error) = file.flush().and_then(|_| file.sync_all())
            {
                staging.error = Some(error.to_string());
            }
            let _ = staging.file.take();
            staging.error.clone()
        });
        if staging_error.is_none()
            && let Some(staging) = &mut self.staging
        {
            staging.preserve = true;
        }
        let staged_path = self
            .staging
            .as_ref()
            .filter(|_| staging_error.is_none())
            .map(|staging| staging.path.clone());
        let blob_ref = self.staging.as_ref().and_then(|_| {
            staging_error.is_none().then(|| BlobRef {
                sha256: hex_bytes(self.hasher.finalize().as_ref()),
                bytes: self.total_bytes as u64,
            })
        });

        let retained_bytes = self.head.len() + self.tail.len();
        let mut retained = self.head.clone();
        retained.extend(self.tail.iter().copied());
        let retained_lines = logical_lines(&retained);
        let truncated = retained_bytes < self.total_bytes || retained_lines < self.total_lines;
        let (head, tail) = fit_utf8_edges(&self.head, &self.tail, self.limits.max_bytes);
        let mut text = format!("{head}{tail}");
        let rendered_truncated = retained_bytes_to_utf8_len(&self.head, &self.tail) > text.len();
        let mut omitted_bytes = self.total_bytes.saturating_sub(retained_bytes);
        let mut omitted_lines = self.total_lines.saturating_sub(retained_lines);

        if truncated || rendered_truncated {
            for _ in 0..32 {
                let marker = omission_marker(omitted_bytes, omitted_lines, self.limits.max_bytes);
                let content_budget = self.limits.max_bytes.saturating_sub(marker.len());
                let (head, tail) = fit_utf8_edges(&self.head, &self.tail, content_budget);
                let retained_text = format!("{head}{tail}");
                let next_omitted_bytes = self
                    .total_bytes
                    .saturating_sub(retained_bytes.min(retained_text.len()));
                let next_omitted_lines = self
                    .total_lines
                    .saturating_sub(retained_lines.min(logical_lines(retained_text.as_bytes())));
                text = format!("{head}{marker}{tail}");
                if next_omitted_bytes == omitted_bytes && next_omitted_lines == omitted_lines {
                    break;
                }
                omitted_bytes = next_omitted_bytes;
                omitted_lines = next_omitted_lines;
            }
        }

        BoundedText {
            text,
            truncated: truncated || rendered_truncated,
            total_bytes: self.total_bytes,
            total_lines: self.total_lines,
            omitted_bytes,
            omitted_lines,
            blob_ref,
            staged_path,
            staging_error,
        }
    }

    fn can_push_head(&self, byte: u8) -> bool {
        let head_byte_limit = self.limits.max_bytes.div_ceil(2);
        let head_line_limit = self.limits.max_lines.div_ceil(2);
        if self.head.len() >= head_byte_limit || head_line_limit == 0 {
            return false;
        }

        let adds_line = self.head.is_empty() || self.head.last() == Some(&b'\n');
        let lines_after = self.head_lines + usize::from(adds_line);
        let _ = byte;
        lines_after <= head_line_limit
    }

    fn push_tail(&mut self, byte: u8) {
        let tail_byte_limit = self.limits.max_bytes / 2;
        let tail_line_limit = self.limits.max_lines / 2;
        if tail_byte_limit == 0 || tail_line_limit == 0 {
            return;
        }

        if self.tail.is_empty() || self.tail.back() == Some(&b'\n') {
            self.tail_lines += 1;
        }
        self.tail.push_back(byte);

        while self.tail.len() > tail_byte_limit || self.tail_lines > tail_line_limit {
            if let Some(removed) = self.tail.pop_front() {
                if self.tail.is_empty() {
                    self.tail_lines = 0;
                } else if removed == b'\n' {
                    self.tail_lines = self.tail_lines.saturating_sub(1);
                }
            }
        }
    }
}

pub(super) fn staged_output(
    limits: ToolLimits,
    tool: &str,
    stream: &str,
) -> std::io::Result<BoundedOutput> {
    let mut collision = None;
    for _ in 0..16 {
        let sequence = NEXT_STAGING_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kurama-{tool}-{}-{sequence}-{stream}.tmp",
            std::process::id()
        ));
        match BoundedOutput::with_staging(limits, path) {
            Ok(output) => return Ok(output),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                collision = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique tool display staging file",
        )
    }))
}

pub(super) fn take_truncated_staging(
    bounded: &mut BoundedText,
) -> std::io::Result<Option<PathBuf>> {
    let Some(path) = bounded.staged_path.take() else {
        return Ok(None);
    };
    if bounded.truncated {
        Ok(Some(path))
    } else {
        std::fs::remove_file(path)?;
        Ok(None)
    }
}

fn logical_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        bytes.iter().filter(|&&byte| byte == b'\n').count()
            + usize::from(bytes.last() != Some(&b'\n'))
    }
}

fn retained_bytes_to_utf8_len(head: &[u8], tail: &VecDeque<u8>) -> usize {
    let tail: Vec<u8> = tail.iter().copied().collect();
    String::from_utf8_lossy(head).len() + String::from_utf8_lossy(&tail).len()
}

fn omission_marker(omitted_bytes: usize, omitted_lines: usize, max_bytes: usize) -> String {
    let verbose = format!("\n[... omitted {omitted_bytes} bytes / {omitted_lines} lines ...]\n");
    if verbose.len() <= max_bytes {
        return verbose;
    }

    let compact = format!("[omitted {omitted_bytes}B/{omitted_lines}L]");
    if compact.len() <= max_bytes {
        return compact;
    }

    format!("~{omitted_bytes}B/{omitted_lines}L~")
}

fn fit_utf8_edges(head: &[u8], tail: &VecDeque<u8>, max_bytes: usize) -> (String, String) {
    let head = String::from_utf8_lossy(head).into_owned();
    let tail_bytes: Vec<u8> = tail.iter().copied().collect();
    let tail = String::from_utf8_lossy(&tail_bytes).into_owned();
    if head.len() + tail.len() <= max_bytes {
        return (head, tail);
    }

    let head_budget = max_bytes.div_ceil(2);
    let tail_budget = max_bytes.saturating_sub(head_budget);
    (
        truncate_utf8_end(&head, head_budget).to_owned(),
        truncate_utf8_start(&tail, tail_budget).to_owned(),
    )
}

fn truncate_utf8_end(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn truncate_utf8_start(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    hex_bytes(Sha256::digest(bytes).as_ref())
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
