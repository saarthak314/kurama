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

enum StagingTarget {
    Path(PathBuf),
    Temporary { tool: String, stream: String },
}

struct StagingFile {
    target: StagingTarget,
    // Only a successfully created file belongs to us and may be removed.
    path: Option<PathBuf>,
    file: Option<File>,
    error: Option<String>,
}

impl StagingFile {
    fn new(target: StagingTarget) -> Self {
        Self {
            target,
            path: None,
            file: None,
            error: None,
        }
    }

    fn start(&mut self, prior: &[u8], current: &[u8]) {
        let opened = match &self.target {
            StagingTarget::Path(path) => open_staging(path).map(|file| (path.clone(), file)),
            StagingTarget::Temporary { tool, stream } => create_staging(tool, stream),
        };
        match opened {
            Ok((path, file)) => {
                self.path = Some(path);
                self.file = Some(file);
                self.push(prior);
                self.push(current);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.error.is_none()
            && let Some(file) = &mut self.file
            && let Err(error) = file.write_all(bytes)
        {
            self.error = Some(error.to_string());
        }
    }

    fn finish(mut self) -> (Option<PathBuf>, Option<String>) {
        if self.error.is_none()
            && let Some(file) = &mut self.file
            && let Err(error) = file.flush()
        {
            self.error = Some(error.to_string());
        }
        // Display recovery is disposable, not canonical durable storage: no fsync.
        self.file.take();
        if self.error.is_some() {
            (None, self.error.take())
        } else {
            (self.path.take(), None)
        }
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        self.file.take();
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub struct BoundedOutput {
    limits: ToolLimits,
    // Before truncation, head contains every original byte, without decoding.
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_lines: usize,
    total_bytes: usize,
    total_lines: usize,
    last_byte: Option<u8>,
    truncated: bool,
    hasher: Option<Sha256>,
    staging: Option<StagingFile>,
}

impl BoundedOutput {
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            head: Vec::new(),
            tail: VecDeque::new(),
            head_lines: 0,
            total_bytes: 0,
            total_lines: 0,
            last_byte: None,
            truncated: false,
            hasher: None,
            staging: None,
        }
    }

    pub fn with_staging(limits: ToolLimits, path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self::staging(
            limits,
            StagingTarget::Path(path.as_ref().to_path_buf()),
        ))
    }

    fn staging(limits: ToolLimits, target: StagingTarget) -> Self {
        let mut output = Self::new(limits);
        output.staging = Some(StagingFile::new(target));
        output
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(hasher) = &mut self.hasher {
            hasher.update(bytes);
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        self.total_lines = self
            .total_lines
            .saturating_add(added_lines(self.last_byte, bytes));
        self.last_byte = bytes.last().copied();

        if !self.truncated {
            if self.total_bytes <= self.limits.max_bytes
                && self.total_lines <= self.limits.max_lines
            {
                self.head.extend_from_slice(bytes);
                return;
            }
            self.begin_truncation(bytes);
        } else if let Some(staging) = &mut self.staging {
            staging.push(bytes);
        }
        self.push_edges(bytes);
    }

    fn begin_truncation(&mut self, current: &[u8]) {
        if let Some(staging) = &mut self.staging {
            staging.start(&self.head, current);
            let mut hasher = Sha256::new_with_prefix(&self.head);
            hasher.update(current);
            self.hasher = Some(hasher);
        }
        self.truncated = true;
        let end = prefix_end(
            &self.head,
            self.limits.max_bytes.div_ceil(2),
            self.limits.max_lines.div_ceil(2),
        );
        append_tail(&mut self.tail, &self.head[end..], self.limits);
        self.head.truncate(end);
        self.head_lines = logical_lines(&self.head);
    }

    fn push_edges(&mut self, bytes: &[u8]) {
        let continued_line = usize::from(!self.head.is_empty() && self.head.last() != Some(&b'\n'));
        let end = prefix_end(
            bytes,
            self.limits.max_bytes.div_ceil(2) - self.head.len(),
            self.limits.max_lines.div_ceil(2) - self.head_lines + continued_line,
        );
        self.head_lines += added_lines(self.head.last().copied(), &bytes[..end]);
        self.head.extend_from_slice(&bytes[..end]);
        append_tail(&mut self.tail, &bytes[end..], self.limits);
    }

    pub fn finish(self) -> BoundedText {
        self.finish_staged(false)
    }

    pub(super) fn full_bytes(&self) -> Option<&[u8]> {
        (!self.truncated).then_some(self.head.as_slice())
    }

    // An aggregate may truncate even though each individual stream fits.
    pub(super) fn finish_staged(mut self, force_staging: bool) -> BoundedText {
        if !self.truncated {
            let decoded = String::from_utf8_lossy(&self.head);
            if decoded.len() > self.limits.max_bytes {
                // Invalid UTF-8 can expand during display even when raw bytes fit.
                drop(decoded);
                self.begin_truncation(&[]);
            } else {
                if force_staging
                    && !self.head.is_empty()
                    && let Some(staging) = &mut self.staging
                {
                    staging.start(&self.head, &[]);
                    self.hasher = Some(Sha256::new_with_prefix(&self.head));
                }
                let text = match decoded {
                    std::borrow::Cow::Borrowed(_) => {
                        String::from_utf8(std::mem::take(&mut self.head)).expect("validated UTF-8")
                    }
                    std::borrow::Cow::Owned(text) => text,
                };
                let (blob_ref, staged_path, staging_error) = self.finish_staging();
                return BoundedText {
                    text,
                    truncated: false,
                    total_bytes: self.total_bytes,
                    total_lines: self.total_lines,
                    omitted_bytes: 0,
                    omitted_lines: 0,
                    blob_ref,
                    staged_path,
                    staging_error,
                };
            }
        }

        let tail_bytes = self.tail.make_contiguous();
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(tail_bytes);
        // Reserve once using total-count digit widths. Actual omissions cannot
        // require more digits, so no fixed-point formatting/copy loop is needed.
        let max_bytes = if self.limits.max_lines == 0 {
            0
        } else {
            self.limits.max_bytes
        };
        let separate_marker = self.limits.max_lines >= 3 && max_bytes >= 5;
        let separators = if separate_marker { 2 } else { 0 };
        let marker = Marker::new(self.total_bytes, self.total_lines, max_bytes - separators);
        let content_budget = max_bytes - marker.reserved_bytes - separators;
        let (head_visible, tail_visible) = if separate_marker {
            let lines = self.limits.max_lines - 1;
            let head_end = prefix_end(head.as_bytes(), usize::MAX, lines.div_ceil(2));
            let tail_start = suffix_start(tail.as_bytes(), usize::MAX, lines / 2);
            (&head[..head_end], &tail[tail_start..])
        } else {
            (head.as_ref(), tail.as_ref())
        };
        let (mut head_text, tail_text) = fit_utf8_edges(head_visible, tail_visible, content_budget);
        // An inline marker shares the tail's first line. Without a tail, do
        // not let a final head newline create an extra visible marker line.
        if !separate_marker && tail_text.is_empty() && head_text.ends_with('\n') {
            head_text = &head_text[..head_text.len() - 1];
        }
        let head_bytes = source_prefix_len(&self.head, head_text.len());
        let tail_omitted = source_prefix_len(tail_bytes, tail.len() - tail_text.len());
        let tail_retained = &tail_bytes[tail_omitted..];
        let omitted_bytes = self.total_bytes - head_bytes - tail_retained.len();
        let omitted_lines = self
            .total_lines
            .saturating_sub(logical_lines(&self.head[..head_bytes]) + logical_lines(tail_retained));
        let mut text = String::with_capacity(
            head_text.len() + marker.reserved_bytes + separators + tail_text.len(),
        );
        text.push_str(head_text);
        if separate_marker && !head_text.is_empty() && !head_text.ends_with('\n') {
            text.push('\n');
        }
        marker.write(&mut text, omitted_bytes, omitted_lines);
        if separate_marker && !tail_text.is_empty() {
            text.push('\n');
        }
        text.push_str(tail_text);
        let (blob_ref, staged_path, staging_error) = self.finish_staging();
        BoundedText {
            text,
            truncated: true,
            total_bytes: self.total_bytes,
            total_lines: self.total_lines,
            omitted_bytes,
            omitted_lines,
            blob_ref,
            staged_path,
            staging_error,
        }
    }

    fn finish_staging(&mut self) -> (Option<BlobRef>, Option<PathBuf>, Option<String>) {
        let (staged_path, staging_error) = self
            .staging
            .take()
            .map_or((None, None), StagingFile::finish);
        let blob_ref = staged_path.as_ref().map(|_| BlobRef {
            sha256: crate::id::hexadecimal(
                "",
                self.hasher
                    .take()
                    .expect("staging has a hasher")
                    .finalize()
                    .as_ref(),
            ),
            bytes: self.total_bytes as u64,
        });
        (blob_ref, staged_path, staging_error)
    }
}

pub(super) fn staged_output(
    limits: ToolLimits,
    tool: &str,
    stream: &str,
) -> std::io::Result<BoundedOutput> {
    Ok(BoundedOutput::staging(
        limits,
        StagingTarget::Temporary {
            tool: tool.to_owned(),
            stream: stream.to_owned(),
        },
    ))
}

fn open_staging(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_staging(tool: &str, stream: &str) -> std::io::Result<(PathBuf, File)> {
    for _ in 0..16 {
        let sequence = NEXT_STAGING_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kurama-{tool}-{}-{sequence}-{stream}.tmp",
            std::process::id()
        ));
        match open_staging(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique tool display staging file",
    ))
}

pub(super) fn take_truncated_staging(
    bounded: &mut BoundedText,
) -> std::io::Result<Option<PathBuf>> {
    if bounded.truncated
        && let Some(error) = &bounded.staging_error
    {
        return Err(std::io::Error::other(error.clone()));
    }
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
    bytes.iter().filter(|&&byte| byte == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
}

fn added_lines(previous: Option<u8>, bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        logical_lines(bytes) - usize::from(previous.is_some_and(|byte| byte != b'\n'))
    }
}

fn prefix_end(bytes: &[u8], max_bytes: usize, max_lines: usize) -> usize {
    if max_lines == 0 {
        return 0;
    }
    let bytes = &bytes[..bytes.len().min(max_bytes)];
    bytes
        .iter()
        .enumerate()
        .filter(|&(_, &byte)| byte == b'\n')
        .nth(max_lines - 1)
        .map_or(bytes.len(), |(index, _)| index + 1)
}

fn suffix_start(bytes: &[u8], max_bytes: usize, max_lines: usize) -> usize {
    if max_lines == 0 || max_bytes == 0 {
        return bytes.len();
    }
    let start = bytes.len().saturating_sub(max_bytes);
    let end = bytes.len() - usize::from(bytes.last() == Some(&b'\n'));
    bytes[start..end]
        .iter()
        .enumerate()
        .rev()
        .filter(|&(_, &byte)| byte == b'\n')
        .nth(max_lines - 1)
        .map_or(start, |(index, _)| start + index + 1)
}

fn append_tail(tail: &mut VecDeque<u8>, bytes: &[u8], limits: ToolLimits) {
    if bytes.is_empty() {
        return;
    }
    let byte_limit = limits.max_bytes / 2;
    let line_limit = limits.max_lines / 2;
    let start = suffix_start(bytes, byte_limit, line_limit);
    if start > 0 {
        tail.clear();
    }
    tail.extend(bytes[start..].iter().copied());
    let start = suffix_start(tail.make_contiguous(), byte_limit, line_limit);
    tail.drain(..start);
}

struct Marker {
    style: MarkerStyle,
    reserved_bytes: usize,
}

enum MarkerStyle {
    Verbose,
    Compact,
    Tiny,
    Dots,
}

impl Marker {
    fn new(bytes: usize, lines: usize, max_bytes: usize) -> Self {
        let digits = decimal_digits(bytes) + decimal_digits(lines);
        for (style, fixed) in [
            (
                MarkerStyle::Verbose,
                "[... omitted  bytes /  lines ...]".len(),
            ),
            (MarkerStyle::Compact, "[omitted B/L]".len()),
            (MarkerStyle::Tiny, "~B/L~".len()),
        ] {
            let reserved_bytes = fixed + digits;
            if reserved_bytes <= max_bytes {
                return Self {
                    style,
                    reserved_bytes,
                };
            }
        }
        Self {
            style: MarkerStyle::Dots,
            reserved_bytes: max_bytes.min(3),
        }
    }

    fn write(&self, text: &mut String, bytes: usize, lines: usize) {
        match self.style {
            MarkerStyle::Verbose => write!(text, "[... omitted {bytes} bytes / {lines} lines ...]"),
            MarkerStyle::Compact => write!(text, "[omitted {bytes}B/{lines}L]"),
            MarkerStyle::Tiny => write!(text, "~{bytes}B/{lines}L~"),
            MarkerStyle::Dots => {
                text.push_str(&"..."[..self.reserved_bytes]);
                Ok(())
            }
        }
        .expect("writing to a string cannot fail");
    }
}

fn decimal_digits(value: usize) -> usize {
    value
        .checked_ilog10()
        .map_or(1, |digits| digits as usize + 1)
}

fn fit_utf8_edges<'a>(head: &'a str, tail: &'a str, max_bytes: usize) -> (&'a str, &'a str) {
    if head.len() + tail.len() <= max_bytes {
        return (head, tail);
    }
    let head_budget = max_bytes.div_ceil(2).min(head.len());
    let tail_budget = (max_bytes - head_budget).min(tail.len());
    let head_budget = max_bytes - tail_budget;
    (
        truncate_utf8_end(head, head_budget),
        truncate_utf8_start(tail, tail_budget),
    )
}

// Map a lossy UTF-8 display boundary back to original bytes. Invalid sequences
// contribute one replacement character, not three supposedly retained bytes.
fn source_prefix_len(mut bytes: &[u8], mut display_bytes: usize) -> usize {
    let mut source_bytes = 0;
    while display_bytes > 0 {
        match std::str::from_utf8(bytes) {
            Ok(_) => return source_bytes + display_bytes,
            Err(error) => {
                let valid = error.valid_up_to();
                if display_bytes <= valid {
                    return source_bytes + display_bytes;
                }
                let invalid = error.error_len().unwrap_or(bytes.len() - valid);
                source_bytes += valid + invalid;
                display_bytes -= valid + '\u{fffd}'.len_utf8();
                bytes = &bytes[valid + invalid..];
            }
        }
    }
    source_bytes
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_starts_at_overflow_and_preserves_every_original_byte() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("output");
        let mut output = BoundedOutput::with_staging(
            ToolLimits {
                max_bytes: 12,
                max_lines: 4,
            },
            &path,
        )
        .expect("configure staging");
        let mut expected = Vec::new();
        for chunk in [b"a\xe2".as_slice(), b"\x82\xac\nlast"] {
            output.push(chunk);
            expected.extend_from_slice(chunk);
            assert!(!path.exists());
        }
        for chunk in [b"\xff-overflow\n".as_slice(), b"tail\0"] {
            output.push(chunk);
            expected.extend_from_slice(chunk);
            assert_eq!(
                std::fs::read(&path).expect("complete staged bytes"),
                expected
            );
        }
        let mut bounded = output.finish();
        assert!(bounded.truncated);
        assert_eq!(bounded.total_bytes, expected.len());
        assert_eq!(
            bounded.blob_ref.as_ref().expect("blob").sha256,
            crate::id::hexadecimal("", Sha256::digest(&expected).as_ref())
        );
        assert_eq!(
            bounded.blob_ref.as_ref().expect("blob").bytes,
            expected.len() as u64
        );
        assert_eq!(
            take_truncated_staging(&mut bounded).expect("staging"),
            Some(path.clone())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn below_limit_output_never_opens_a_staging_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Opening this path would fail; a display that fits must not need it.
        let path = temp.path().join("missing-parent/output");
        let mut output = BoundedOutput::with_staging(
            ToolLimits {
                max_bytes: 32,
                max_lines: 2,
            },
            &path,
        )
        .expect("configure staging");
        for chunk in [
            b"abcdefghijklmnopq\xe2".as_slice(),
            b"\x82",
            b"\xac\nlast\xff",
        ] {
            output.push(chunk);
            assert!(!path.exists());
        }
        let mut bounded = output.finish();
        assert_eq!(bounded.text, "abcdefghijklmnopq€\nlast�");
        assert!(!bounded.truncated);
        assert_eq!((bounded.omitted_bytes, bounded.omitted_lines), (0, 0));
        assert_eq!(bounded.staging_error, None);
        assert_eq!(bounded.blob_ref, None);
        assert_eq!(
            take_truncated_staging(&mut bounded).expect("no staging needed"),
            None
        );
        assert!(!path.parent().expect("parent").exists());
    }

    #[test]
    fn lossy_expansion_stages_raw_bytes_only_when_display_overflows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("output");
        let bytes = [0xff; 4];
        let mut output = BoundedOutput::with_staging(
            ToolLimits {
                max_bytes: 4,
                max_lines: 2,
            },
            &path,
        )
        .expect("configure staging");
        output.push(&bytes);
        assert!(!path.exists());
        let bounded = output.finish();
        assert!(bounded.truncated);
        assert!(bounded.text.len() <= 4);
        assert_eq!(bounded.omitted_bytes, 4);
        assert_eq!(std::fs::read(path).expect("original bytes"), bytes);
    }

    #[test]
    fn staging_failures_are_reported_without_deleting_a_collision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let occupied = temp.path().join("occupied");
        std::fs::write(&occupied, b"unrelated").expect("existing file");
        for path in [occupied.clone(), temp.path().join("missing/output")] {
            let mut output = BoundedOutput::with_staging(
                ToolLimits {
                    max_bytes: 0,
                    max_lines: 0,
                },
                path,
            )
            .expect("configure staging");
            output.push(b"cannot display");
            let mut bounded = output.finish();
            assert!(bounded.text.is_empty());
            assert!(take_truncated_staging(&mut bounded).is_err());
            assert_eq!(bounded.staged_path, None);
            assert_eq!(bounded.blob_ref, None);
        }
        assert_eq!(
            std::fs::read(occupied).expect("existing file survives"),
            b"unrelated"
        );
    }

    #[test]
    fn failed_or_abandoned_staging_removes_partial_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("output");
        for fail_write in [false, true] {
            let mut output = BoundedOutput::with_staging(
                ToolLimits {
                    max_bytes: 1,
                    max_lines: 1,
                },
                &path,
            )
            .expect("configure staging");
            output.push(b"overflow");
            assert_eq!(std::fs::read(&path).expect("partial staging"), b"overflow");
            if fail_write {
                // Inject a real write failure without global filesystem state.
                output.staging.as_mut().expect("staging").file =
                    Some(File::open(&path).expect("read-only handle"));
                output.push(b"write fails");
                let mut bounded = output.finish();
                assert!(take_truncated_staging(&mut bounded).is_err());
                assert_eq!(bounded.staged_path, None);
                assert_eq!(bounded.blob_ref, None);
            } else {
                drop(output);
            }
            assert!(!path.exists());
        }
    }

    #[test]
    fn omitted_counts_measure_source_bytes_not_replacement_characters() {
        let mut output = BoundedOutput::new(ToolLimits {
            max_bytes: 80,
            max_lines: 2,
        });
        output.push(b"a\xff\nmiddle\nz\xff");
        let bounded = output.finish();
        assert!(bounded.text.starts_with("a�\n"));
        assert!(bounded.text.ends_with("z�"));
        assert_eq!((bounded.total_bytes, bounded.total_lines), (12, 3));
        assert_eq!((bounded.omitted_bytes, bounded.omitted_lines), (7, 1));
        assert_eq!(bounded.text.lines().count(), 2);
    }

    #[test]
    fn omission_marker_has_its_own_line_when_limits_allow_it() {
        let mut output = BoundedOutput::new(ToolLimits {
            max_bytes: 80,
            max_lines: 3,
        });
        output.push(b"first\nsecond\nthird\nlast\n");
        let bounded = output.finish();
        let lines: Vec<_> = bounded.text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "first");
        assert_eq!(lines[2], "last");
        assert_eq!((bounded.omitted_bytes, bounded.omitted_lines), (13, 2));
    }

    #[test]
    fn byte_line_and_utf8_bounds_do_not_depend_on_chunk_boundaries() {
        let inputs = [
            Vec::new(),
            b"one line without a newline".to_vec(),
            b"\n\n\n\n\n\n\n\n".to_vec(),
            "α\n🙂\n尾\nlast".as_bytes().to_vec(),
            b"a\xff\xe2\x82\nb\xf0\x90\x80\nc\0\n".repeat(5),
        ];
        for bytes in inputs {
            for max_bytes in 0..=64 {
                for max_lines in 0..=8 {
                    let limits = ToolLimits {
                        max_bytes,
                        max_lines,
                    };
                    let mut whole = BoundedOutput::new(limits);
                    whole.push(&bytes);
                    let expected = whole.finish();
                    assert!(expected.text.len() <= max_bytes, "{limits:?}: {expected:?}");
                    assert!(
                        expected.text.lines().count() <= max_lines,
                        "{limits:?}: {expected:?}"
                    );
                    assert_eq!(expected.total_bytes, bytes.len());
                    assert_eq!(
                        expected.total_lines,
                        bytes.split_inclusive(|&byte| byte == b'\n').count()
                    );
                    assert!(expected.omitted_bytes <= expected.total_bytes);
                    assert!(expected.omitted_lines <= expected.total_lines);
                    if !expected.truncated {
                        assert_eq!(expected.text, String::from_utf8_lossy(&bytes));
                        assert_eq!((expected.omitted_bytes, expected.omitted_lines), (0, 0));
                    }
                    for chunk_size in [1, 3, 17] {
                        let mut chunked = BoundedOutput::new(limits);
                        for chunk in bytes.chunks(chunk_size) {
                            chunked.push(chunk);
                        }
                        assert_eq!(
                            chunked.finish(),
                            expected,
                            "{limits:?}, chunk size {chunk_size}"
                        );
                    }
                }
            }
        }
    }
}
