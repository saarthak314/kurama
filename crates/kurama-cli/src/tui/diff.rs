use std::{
    cell::Cell,
    collections::BTreeSet,
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::timeout,
};

use super::{
    theme::{ACCENT, AMBER, BORDER, DIM, GREEN, RED, TEXT},
    transcript::{for_each_wrapped_line, sanitize_terminal_text, truncate_display},
};

const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_HUNK_BYTES: usize = 256 * 1024;
const MAX_FEEDBACK_BYTES: usize = MAX_HUNK_BYTES + 64 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 512;
const MAX_ENTRIES: usize = 8192;
const MAX_PATCH_LINES: usize = 100_000;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const MAX_FILTER_KEY_BYTES: usize = 16 * 1024;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
const REVIEW_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Patch {
    bytes: Vec<u8>,
    source: &'static str,
}

#[derive(Debug)]
struct FileDiff {
    patch: usize,
    header: Range<usize>,
    path: String,
}

#[derive(Debug)]
struct ReviewEntry {
    file: usize,
    // A metadata-only change is deliberately not a synthetic text hunk.
    hunk: Option<Range<usize>>,
}

#[derive(Debug)]
pub(crate) struct DiffReview {
    patches: Vec<Patch>,
    files: Vec<FileDiff>,
    entries: Vec<ReviewEntry>,
    selected: usize,
    scroll: Cell<usize>,
    viewport_height: Cell<usize>,
    measured: Cell<Option<(usize, usize, usize)>>,
    notice: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiffAction {
    None,
    Close,
    Feedback(String),
}

impl DiffReview {
    fn new() -> Self {
        Self {
            patches: Vec::new(),
            files: Vec::new(),
            entries: Vec::new(),
            selected: 0,
            scroll: Cell::new(0),
            viewport_height: Cell::new(1),
            measured: Cell::new(None),
            notice: None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> DiffAction {
        if key.kind == KeyEventKind::Release {
            return DiffAction::None;
        }
        // Leave modified application shortcuts to the caller; do not turn Ctrl+P
        // into an accidental selection change.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return DiffAction::None;
        }
        match key.code {
            KeyCode::Esc => return DiffAction::Close,
            KeyCode::Char('n') | KeyCode::Tab => {
                if !self.entries.is_empty() {
                    self.selected = (self.selected + 1) % self.entries.len();
                    self.reset_scroll();
                }
            }
            KeyCode::Char('p') | KeyCode::BackTab => {
                if !self.entries.is_empty() {
                    self.selected = if self.selected == 0 {
                        self.entries.len() - 1
                    } else {
                        self.selected - 1
                    };
                    self.reset_scroll();
                }
            }
            KeyCode::Up => self.scroll.set(self.scroll.get().saturating_sub(1)),
            KeyCode::Down => self.scroll.set(self.scroll.get().saturating_add(1)),
            KeyCode::PageUp => self.scroll.set(
                self.scroll
                    .get()
                    .saturating_sub(self.viewport_height.get().max(1)),
            ),
            KeyCode::PageDown => self.scroll.set(
                self.scroll
                    .get()
                    .saturating_add(self.viewport_height.get().max(1)),
            ),
            KeyCode::Home => self.scroll.set(0),
            KeyCode::End => self.scroll.set(usize::MAX),
            KeyCode::Enter => match self.feedback() {
                Ok(text) => return DiffAction::Feedback(text),
                Err(error) => self.notice = Some(error),
            },
            _ => {}
        }
        DiffAction::None
    }

    fn reset_scroll(&mut self) {
        self.scroll.set(0);
        self.measured.set(None);
        self.notice = None;
    }

    fn feedback(&self) -> Result<String, String> {
        let entry = self
            .entries
            .get(self.selected)
            .ok_or("No changes to review.")?;
        let hunk = entry.hunk.as_ref().ok_or(
            "No text hunk: binary, mode-only, rename-only and empty-file changes have no hunk feedback.",
        )?;
        let file = &self.files[entry.file];
        let patch = &self.patches[file.patch];
        let header = std::str::from_utf8(&patch.bytes[file.header.clone()])
            .map_err(|_| "Feedback unavailable: the patch header is not UTF-8.")?;
        let body = std::str::from_utf8(&patch.bytes[hunk.clone()]).map_err(|_| {
            "Feedback unavailable: this hunk is not UTF-8; its displayed replacement characters are not the original bytes."
        })?;
        let range = body.split('\n').next().unwrap_or_default();
        let prefix = format!(
            "Review this {} hunk in {} ({range}):\n\n",
            patch.source, file.path
        );
        let suffix = if body.ends_with('\n') {
            "\nFeedback: "
        } else {
            "\n\nFeedback: "
        };
        let size = prefix.len() + header.len() + body.len() + suffix.len();
        if size > MAX_FEEDBACK_BYTES {
            return Err(format!(
                "Feedback exceeds the {} KiB limit; it was not truncated.",
                MAX_FEEDBACK_BYTES / 1024
            ));
        }
        let mut feedback = String::with_capacity(size);
        feedback.push_str(&prefix);
        feedback.push_str(header);
        feedback.push_str(body);
        feedback.push_str(suffix);
        Ok(feedback)
    }

    fn visit_lines(&self, mut visit: impl FnMut(&str, Color)) {
        let Some(entry) = self.entries.get(self.selected) else {
            visit("No working changes (staged, unstaged or untracked).", DIM);
            return;
        };
        let file = &self.files[entry.file];
        let patch = &self.patches[file.patch];
        for (index, range) in std::iter::once(&file.header)
            .chain(entry.hunk.iter())
            .enumerate()
        {
            for bytes in patch.bytes[range.clone()].split_inclusive(|byte| *byte == b'\n') {
                let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
                let text = String::from_utf8_lossy(bytes);
                let color = match (index, bytes.first()) {
                    (1, Some(b'+')) => GREEN,
                    (1, Some(b'-')) => RED,
                    (1, Some(b'@')) => ACCENT,
                    (1, Some(b' ')) => TEXT,
                    _ => DIM,
                };
                visit(&text, color);
            }
        }
        if entry.hunk.is_none() {
            visit(
                "No text hunks in this change; Enter cannot prepare hunk feedback.",
                AMBER,
            );
        }
        if std::str::from_utf8(&patch.bytes[file.header.clone()]).is_err()
            || entry
                .hunk
                .as_ref()
                .is_some_and(|hunk| std::str::from_utf8(&patch.bytes[hunk.clone()]).is_err())
        {
            visit(
                "Non-UTF-8 bytes shown as replacements; exact feedback is unavailable.",
                AMBER,
            );
        }
    }
}

/// Takes a read-only snapshot. All subprocesses and file metadata operations are
/// asynchronous, bounded, and cancelled with the loader; no index refresh occurs.
pub(crate) async fn load_diff(project: PathBuf) -> Result<DiffReview, String> {
    if !cfg!(unix) {
        return Err("Read-only diff review requires Unix process-group cancellation.".into());
    }
    timeout(REVIEW_TIMEOUT, load_git_diff(&project))
        .await
        .map_err(|_| "Diff review exceeded 60 seconds; no partial review was opened.".to_owned())?
}

async fn load_git_diff(project: &Path) -> Result<DiffReview, String> {
    let root = run_git(
        project,
        &["rev-parse".into(), "--show-toplevel".into()],
        &[],
        64 * 1024,
        false,
    )
    .await?;
    let root = root.strip_suffix(b"\n").unwrap_or(&root);
    let root = PathBuf::from(path_from_bytes(root)?);
    let filters = disabled_filters(&root).await?;
    let mut review = DiffReview::new();
    let mut remaining = MAX_PATCH_BYTES;
    for (source, cached) in [("staged", true), ("unstaged", false)] {
        let mut args = patch_args();
        if cached {
            args.push("--cached".into());
        }
        args.push("--".into());
        let bytes = run_git(&root, &args, &filters, remaining, false).await?;
        remaining -= bytes.len();
        append_patch(&mut review, source, bytes)?;
    }
    let untracked = run_git(
        &root,
        &[
            "ls-files".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ],
        &filters,
        1024 * 1024,
        false,
    )
    .await?;
    let mut untracked_count = 0;
    for bytes in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        untracked_count += 1;
        if untracked_count > MAX_FILES || review.files.len() >= MAX_FILES {
            return Err(format!(
                "Diff review exceeds {MAX_FILES} file changes; no partial review was opened."
            ));
        }
        let relative = PathBuf::from(path_from_bytes(bytes)?);
        // ls-files paths are repository-relative. Refuse unexpected traversal
        // instead of accidentally reviewing a file outside the selected project.
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err("Git returned an untracked path outside the repository.".into());
        }
        let path = root.join(&relative);
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
            format!(
                "Cannot inspect untracked {}: {error}",
                path_label(bytes, false)
            )
        })?;
        if !metadata.is_file() && !metadata.file_type().is_symlink() {
            return Err(format!(
                "Cannot review untracked {}: not a regular file or symlink (nested repositories must be reviewed separately).",
                path_label(bytes, false)
            ));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(format!(
                "Untracked {} exceeds the {} MiB file limit; it was not truncated.",
                path_label(bytes, false),
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        let bytes = if metadata.file_type().is_symlink() {
            symlink_patch(&path, bytes, remaining).await?
        } else {
            let mut args = patch_args();
            args.extend(["--no-index".into(), "--".into(), "/dev/null".into()]);
            // Keep `-` filesystem-local instead of becoming Git's stdin operand.
            args.push(Path::new(".").join(relative).into_os_string());
            run_git(&root, &args, &filters, remaining, true).await?
        };
        remaining -= bytes.len();
        append_patch(&mut review, "untracked", bytes)?;
    }
    Ok(review)
}

// --no-ext-diff/--no-textconv do not disable clean/process filters. Discover
// names only (never commands), then override every driver for raw review. The
// same overrides reach submodule status children through Git's command config.
async fn disabled_filters(root: &Path) -> Result<Vec<OsString>, String> {
    let mut pending = vec![root.to_owned()];
    let mut drivers = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut repositories = 0;
    let mut driver_bytes = 0;
    while let Some(repository) = pending.pop() {
        repositories += 1;
        if repositories > MAX_FILES {
            return Err("Too many submodule repositories to disable diff helpers safely.".into());
        }
        let repository = tokio::fs::canonicalize(repository)
            .await
            .map_err(|error| format!("Cannot resolve submodule repository: {error}"))?;
        if !seen.insert(repository.clone()) {
            return Err(
                "Repeated or cyclic submodule repositories cannot be reviewed safely.".into(),
            );
        }
        let names = run_git(
            &repository,
            &[
                "config".into(),
                "--includes".into(),
                "--null".into(),
                "--name-only".into(),
                "--get-regexp".into(),
                r"^filter\..*\.(clean|process|required)$".into(),
            ],
            &[],
            64 * 1024,
            true,
        )
        .await?;
        for name in names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            // `-c` separates key/value at the first '='; refusing such a name is
            // safer than accidentally leaving a configured helper enabled.
            if name.contains(&b'=') {
                return Err("Cannot safely disable a content filter with '=' in its name.".into());
            }
            let end = name
                .iter()
                .rposition(|byte| *byte == b'.')
                .ok_or("Git returned an invalid content-filter key.")?;
            if drivers.insert(name[..end].to_vec()) {
                driver_bytes += end;
            }
            if drivers.len() > MAX_FILES || driver_bytes > MAX_FILTER_KEY_BYTES {
                return Err(
                    "Content-filter configuration exceeds the safe diff review limit.".into(),
                );
            }
        }
        // A superproject diff can inspect initialized submodules, each of which
        // has independent configuration. Read index metadata only, never their
        // contents, until all of their filter drivers have been neutralized.
        let index = run_git(
            &repository,
            &["ls-files".into(), "--stage".into(), "-z".into()],
            &[],
            MAX_PATCH_BYTES,
            false,
        )
        .await?;
        for entry in index.split(|byte| *byte == 0) {
            if !entry.starts_with(b"160000 ") {
                continue;
            }
            let separator = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or("Git returned an invalid submodule index entry.")?;
            let relative = PathBuf::from(path_from_bytes(&entry[separator + 1..])?);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err("Git returned a submodule path outside the repository.".into());
            }
            let submodule = repository.join(relative);
            match tokio::fs::symlink_metadata(submodule.join(".git")).await {
                Ok(_) => {
                    if pending.len() + repositories >= MAX_FILES {
                        return Err(
                            "Too many submodule repositories to disable diff helpers safely."
                                .into(),
                        );
                    }
                    pending.push(submodule);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("Cannot inspect submodule configuration: {error}"));
                }
            }
        }
    }
    let mut overrides = Vec::with_capacity(drivers.len() * 6);
    for driver in drivers {
        for suffix in [".clean=", ".process=", ".required=false"] {
            let mut value = path_from_bytes(&driver)?;
            value.push(suffix);
            overrides.push("-c".into());
            overrides.push(value);
        }
    }
    Ok(overrides)
}

async fn symlink_patch(path: &Path, name: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    // Never pass a symlink to `git diff --no-index`: Git may dereference a
    // directory/FIFO target. read_link returns only the stored link bytes.
    let target = tokio::fs::read_link(path).await.map_err(|error| {
        format!(
            "Cannot read untracked symlink {}: {error}",
            path_label(name, false)
        )
    })?;
    let target = target.as_os_str().as_encoded_bytes();
    if target.len() as u64 > MAX_FILE_BYTES {
        return Err("Untracked symlink exceeds the file limit; it was not truncated.".into());
    }
    let lines = target.split_inclusive(|byte| *byte == b'\n').count();
    let a = quote_git_path(b"a/", name);
    let b = quote_git_path(b"b/", name);
    let range = if lines == 1 {
        "1".to_owned()
    } else {
        format!("1,{lines}")
    };
    let header = format!(
        "diff --git {a} {b}\nnew file mode 120000\n--- /dev/null\n+++ {b}\n@@ -0,0 +{range} @@\n"
    );
    let marker = if target.ends_with(b"\n") {
        b"".as_slice()
    } else {
        b"\n\\ No newline at end of file\n".as_slice()
    };
    let size = header.len() + target.len() + lines + marker.len();
    if size > limit {
        return Err("Untracked symlink patch exceeds the review output limit; no partial review was opened.".into());
    }
    let mut patch = Vec::with_capacity(size);
    patch.extend_from_slice(header.as_bytes());
    for line in target.split_inclusive(|byte| *byte == b'\n') {
        patch.push(b'+');
        patch.extend_from_slice(line);
    }
    patch.extend_from_slice(marker);
    Ok(patch)
}

fn quote_git_path(prefix: &[u8], name: &[u8]) -> String {
    // Always C-quote: octal escapes preserve arbitrary Unix filename bytes,
    // including control characters and names that cannot be displayed as UTF-8.
    let mut quoted = String::with_capacity(prefix.len() + name.len() + 2);
    quoted.push('"');
    for &byte in prefix.iter().chain(name) {
        match byte {
            b'"' | b'\\' => {
                quoted.push('\\');
                quoted.push(char::from(byte));
            }
            0x20..=0x7e => quoted.push(char::from(byte)),
            _ => {
                quoted.push('\\');
                for digit in [byte >> 6, (byte >> 3) & 7, byte & 7] {
                    quoted.push(char::from(b'0' + digit));
                }
            }
        }
    }
    quoted.push('"');
    quoted
}

fn patch_args() -> Vec<OsString> {
    [
        "diff",
        "--patch",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=3",
        "--find-renames=50%",
        "-l512",
        "--submodule=short",
        "--ignore-submodules=none",
        "--output-indicator-new=+",
        "--output-indicator-old=-",
        "--output-indicator-context= ",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

async fn run_git(
    root: &Path,
    args: &[OsString],
    config: &[OsString],
    limit: usize,
    exit_one_is_success: bool,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args([
            "--no-pager",
            "-c",
            "core.quotePath=true",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "diff.autoRefreshIndex=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "diff.suppressBlankEmpty=false",
        ])
        .args(config)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut process = GitProcess::spawn(&mut command)?;
    let child = process.child.as_mut().expect("new Git child");
    let stdout = child.stdout.take().ok_or("Git stdout was unavailable.")?;
    let stderr = child.stderr.take().ok_or("Git stderr was unavailable.")?;
    let operation = async {
        let (stdout, stderr) = tokio::try_join!(
            read_limited(stdout, limit, "patch/output"),
            read_limited(stderr, MAX_STDERR_BYTES, "diagnostic output"),
        )?;
        let status = child
            .wait()
            .await
            .map_err(|error| format!("Cannot wait for git: {error}"))?;
        Ok::<_, String>((stdout, stderr, status))
    };
    let result = timeout(PROCESS_TIMEOUT, operation).await;
    let (stdout, stderr, status) = match result {
        Ok(Ok(result)) => result,
        other => {
            process.terminate().await;
            return Err(match other {
                Ok(Err(error)) => error,
                Err(_) => {
                    "Git exceeded the 15 second process limit; no partial review was opened.".into()
                }
                Ok(Ok(_)) => unreachable!(),
            });
        }
    };
    // Clean up even a helper that closed its output pipes before Git exited.
    process.kill_group();
    process.child = None;
    if !status.success() && !(exit_one_is_success && status.code() == Some(1)) {
        let stderr = String::from_utf8_lossy(&stderr);
        let diagnostic = sanitize_terminal_text(&stderr);
        return Err(format!("Git failed ({status}): {}", diagnostic.trim()));
    }
    Ok(stdout)
}

// Own the group independently of Child::id(), which disappears after wait().
// Drop must also cover cancellation of the enclosing loader future.
struct GitProcess {
    child: Option<Child>,
    #[cfg(unix)]
    group: Option<i32>,
}

impl GitProcess {
    fn spawn(command: &mut Command) -> Result<Self, String> {
        if !cfg!(unix) {
            return Err("Read-only diff review requires Unix process-group cancellation.".into());
        }
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .map_err(|error| format!("Cannot start git: {error}"))?;
        #[cfg(unix)]
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or("Git process identifier is unavailable or too large.")?;
        Ok(Self {
            child: Some(child),
            #[cfg(unix)]
            group: Some(group),
        })
    }

    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take().and_then(rustix::process::Pid::from_raw) {
            // We created this isolated group; never signal the caller's group.
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }

    async fn terminate(&mut self) {
        self.kill_group();
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child = None;
    }
}

impl Drop for GitProcess {
    fn drop(&mut self) {
        self.kill_group();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            // Cancellation cannot await. Transfer only reaping to the runtime;
            // group termination above is synchronous, before Drop returns.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
            // Without a live runtime, Tokio's kill-on-drop reaper is the fallback.
        }
    }
}

async fn read_limited(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
    name: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("Cannot read git {name}: {error}"))?;
    if bytes.len() > limit {
        return Err(format!(
            "Git {name} exceeds the review output limit ({limit} bytes remaining); no partial review was opened."
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<OsString, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<OsString, String> {
    std::str::from_utf8(bytes).map(OsString::from).map_err(|_| {
        "Git returned a non-UTF-8 filesystem path unsupported on this platform.".into()
    })
}

fn append_patch(
    review: &mut DiffReview,
    source: &'static str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut starts = Vec::new();
    let mut offset = 0;
    for (number, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if number >= MAX_PATCH_LINES {
            return Err(format!(
                "Diff exceeds {MAX_PATCH_LINES} patch lines; no partial review was opened."
            ));
        }
        if line.starts_with(b"diff --cc ") || line.starts_with(b"diff --combined ") {
            return Err("Unresolved merge conflicts cannot be reviewed as ordinary hunks; no partial review was opened.".into());
        }
        if line.starts_with(b"diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    if starts.first() != Some(&0) {
        return Err(
            "Git returned an unsupported patch format; no partial review was opened.".into(),
        );
    }
    if review.files.len() + starts.len() > MAX_FILES {
        return Err(format!(
            "Diff review exceeds {MAX_FILES} file changes; no partial review was opened."
        ));
    }
    starts.push(bytes.len());
    let patch = review.patches.len();
    for boundaries in starts.windows(2) {
        let section = boundaries[0]..boundaries[1];
        let mut hunks = Vec::new();
        let mut path = None;
        let mut old_path = None;
        let mut offset = section.start;
        for line in bytes[section.clone()].split_inclusive(|byte| *byte == b'\n') {
            if line.starts_with(b"@@ ") {
                hunks.push(offset);
            } else if hunks.is_empty() {
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                if let Some(value) = line.strip_prefix(b"+++ ") {
                    if value != b"/dev/null" {
                        path = Some(path_label(value, true));
                    }
                } else if let Some(value) = line.strip_prefix(b"--- ") {
                    old_path = Some(path_label(value, true));
                } else if let Some(value) = line
                    .strip_prefix(b"rename to ")
                    .or_else(|| line.strip_prefix(b"copy to "))
                {
                    path = Some(path_label(value, false));
                }
            }
            offset += line.len();
        }
        let header_end = hunks.first().copied().unwrap_or(section.end);
        let path = path.or(old_path).unwrap_or_else(|| {
            let first = bytes[section.clone()]
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            String::from_utf8_lossy(first.strip_prefix(b"diff --git ").unwrap_or(first))
                .into_owned()
        });
        let file = review.files.len();
        review.files.push(FileDiff {
            patch,
            header: section.start..header_end,
            path,
        });
        if hunks.is_empty() {
            review.entries.push(ReviewEntry { file, hunk: None });
        } else {
            hunks.push(section.end);
            for pair in hunks.windows(2) {
                let hunk = pair[0]..pair[1];
                if hunk.len() > MAX_HUNK_BYTES {
                    return Err(format!(
                        "A diff hunk exceeds the {} KiB limit; it was not truncated.",
                        MAX_HUNK_BYTES / 1024
                    ));
                }
                validate_hunk(&bytes[hunk.clone()])?;
                review.entries.push(ReviewEntry {
                    file,
                    hunk: Some(hunk),
                });
            }
        }
        if review.entries.len() > MAX_ENTRIES {
            return Err(format!(
                "Diff exceeds {MAX_ENTRIES} review entries; no partial review was opened."
            ));
        }
    }
    review.patches.push(Patch { bytes, source });
    Ok(())
}

fn validate_hunk(bytes: &[u8]) -> Result<(), String> {
    let invalid =
        || "Git returned a malformed or unsupported hunk; no partial review was opened.".to_owned();
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
    let header = lines.next().ok_or_else(invalid)?;
    let header = header.strip_suffix(b"\n").unwrap_or(header);
    let mut parts = header.split(|byte| *byte == b' ');
    if parts.next() != Some(b"@@".as_slice()) {
        return Err(invalid());
    }
    let old_range =
        std::str::from_utf8(parts.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
    let new_range =
        std::str::from_utf8(parts.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
    let mut old = range_count(old_range, '-').ok_or_else(invalid)?;
    let mut new = range_count(new_range, '+').ok_or_else(invalid)?;
    if parts.next() != Some(b"@@".as_slice()) {
        return Err(invalid());
    }
    for line in lines {
        match line.first() {
            Some(b' ') => {
                old = old.checked_sub(1).ok_or_else(invalid)?;
                new = new.checked_sub(1).ok_or_else(invalid)?;
            }
            Some(b'-') => old = old.checked_sub(1).ok_or_else(invalid)?,
            Some(b'+') => new = new.checked_sub(1).ok_or_else(invalid)?,
            Some(b'\\')
                if line.strip_suffix(b"\n").unwrap_or(line) == b"\\ No newline at end of file" => {}
            _ => return Err(invalid()),
        }
    }
    if old != 0 || new != 0 {
        return Err(invalid());
    }
    Ok(())
}

fn range_count(value: &str, sign: char) -> Option<u64> {
    let value = value.strip_prefix(sign)?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    start.parse::<u64>().ok()?;
    count.parse().ok()
}

// Git C-quotes control bytes and non-ASCII names. Decode valid UTF-8 without
// changing its Unicode, retain reversible Git notation for non-UTF-8 names.
fn path_label(value: &[u8], strip_prefix: bool) -> String {
    let value = value.strip_suffix(b"\t").unwrap_or(value);
    let mut decoded = Vec::new();
    if value.starts_with(b"\"") && value.ends_with(b"\"") && value.len() >= 2 {
        let inner = &value[1..value.len() - 1];
        let mut cursor = 0;
        while cursor < inner.len() {
            let byte = inner[cursor];
            cursor += 1;
            if byte != b'\\' || cursor == inner.len() {
                decoded.push(byte);
                continue;
            }
            let escape = inner[cursor];
            cursor += 1;
            let byte = match escape {
                b'a' => 7,
                b'b' => 8,
                b't' => b'\t',
                b'n' => b'\n',
                b'v' => 11,
                b'f' => 12,
                b'r' => b'\r',
                b'0'..=b'7' => {
                    let mut octal = u16::from(escape - b'0');
                    for _ in 0..2 {
                        if cursor < inner.len() && (b'0'..=b'7').contains(&inner[cursor]) {
                            octal = octal * 8 + u16::from(inner[cursor] - b'0');
                            cursor += 1;
                        } else {
                            break;
                        }
                    }
                    if octal > 255 {
                        return String::from_utf8_lossy(value).into_owned();
                    }
                    octal as u8
                }
                other => other,
            };
            decoded.push(byte);
        }
    } else {
        decoded.extend_from_slice(value);
    }
    let decoded = if strip_prefix {
        decoded
            .strip_prefix(b"a/")
            .or_else(|| decoded.strip_prefix(b"b/"))
            .unwrap_or(&decoded)
    } else {
        &decoded
    };
    match std::str::from_utf8(decoded) {
        Ok(path) if path.chars().any(char::is_control) => format!("{path:?}"),
        Ok(path) => path.to_owned(),
        Err(_) => format!("{} (Git byte-escaped path)", String::from_utf8_lossy(value)),
    }
}

pub(crate) fn render_diff(frame: &mut Frame<'_>, review: &DiffReview, area: Rect) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let inner = if area.width >= 6 && area.height >= 5 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };
    if inner.is_empty() {
        return;
    }
    let width = usize::from(inner.width);
    let title_rows = u16::from(inner.height >= 2);
    let path_rows = u16::from(inner.height >= 4);
    let range_rows = u16::from(inner.height >= 6);
    let footer_rows = u16::from(inner.height >= 3);
    let notice_rows = u16::from(inner.height >= 5 && review.notice.is_some());
    let body = Rect::new(
        inner.x,
        inner.y + title_rows + path_rows + range_rows,
        inner.width,
        inner
            .height
            .saturating_sub(title_rows + path_rows + range_rows + footer_rows + notice_rows),
    );
    review.viewport_height.set(usize::from(body.height));
    if title_rows != 0 {
        let title = if let Some(entry) = review.entries.get(review.selected) {
            let file = &review.files[entry.file];
            format!(
                "Diff review · {} · change {}/{} · {}",
                review.patches[file.patch].source,
                review.selected + 1,
                review.entries.len(),
                if entry.hunk.is_some() {
                    "text hunk"
                } else {
                    "metadata"
                }
            )
        } else {
            "Diff review · clean".to_owned()
        };
        draw_line(
            frame,
            inner.x,
            inner.y,
            inner.width,
            &title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        );
    }
    if path_rows != 0
        && let Some(entry) = review.entries.get(review.selected)
    {
        draw_line(
            frame,
            inner.x,
            inner.y + title_rows,
            inner.width,
            &review.files[entry.file].path,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        );
    }
    if range_rows != 0
        && let Some(entry) = review.entries.get(review.selected)
    {
        let file = &review.files[entry.file];
        let range = entry.hunk.as_ref().map(|hunk| {
            String::from_utf8_lossy(
                review.patches[file.patch].bytes[hunk.clone()]
                    .split(|byte| *byte == b'\n')
                    .next()
                    .unwrap_or_default(),
            )
        });
        draw_line(
            frame,
            inner.x,
            inner.y + title_rows + path_rows,
            inner.width,
            range.as_deref().unwrap_or("No text hunks"),
            Style::default().fg(ACCENT),
        );
    }
    let rows = match review.measured.get() {
        Some((selected, measured_width, rows))
            if selected == review.selected && measured_width == width =>
        {
            rows
        }
        _ => {
            let mut rows = 0_usize;
            review.visit_lines(|text, _| for_each_wrapped_line(text, width, |_| rows += 1));
            review.measured.set(Some((review.selected, width, rows)));
            rows
        }
    };
    let scroll = review
        .scroll
        .get()
        .min(rows.saturating_sub(usize::from(body.height)));
    review.scroll.set(scroll);
    let mut row = 0_usize;
    review.visit_lines(|text, color| {
        for_each_wrapped_line(text, width, |line| {
            if row >= scroll && row - scroll < usize::from(body.height) {
                frame.render_widget(
                    Line::from(Span::styled(line, Style::default().fg(color))),
                    Rect::new(body.x, body.y + (row - scroll) as u16, body.width, 1),
                );
            }
            row += 1;
        });
    });
    if notice_rows != 0
        && let Some(notice) = review.notice.as_ref()
    {
        draw_line(
            frame,
            inner.x,
            inner.bottom() - footer_rows - notice_rows,
            inner.width,
            notice,
            Style::default().fg(AMBER),
        );
    }
    if footer_rows != 0 {
        let controls = if width >= 76 {
            "n/p or Tab hunk · ↑↓/PgUp/PgDn scroll · Home/End · Enter feedback · Esc close"
        } else if width >= 48 {
            "n/p hunk · ↑↓ scroll · Enter feedback · Esc close"
        } else {
            "n/p · ↑↓ · Enter feedback · Esc"
        };
        draw_line(
            frame,
            inner.x,
            inner.bottom() - 1,
            inner.width,
            controls,
            Style::default().fg(DIM),
        );
    }
}

fn draw_line(frame: &mut Frame<'_>, x: u16, y: u16, width: u16, text: &str, style: Style) {
    frame.render_widget(
        Line::from(Span::styled(
            truncate_display(&sanitize_terminal_text(text), usize::from(width)),
            style,
        )),
        Rect::new(x, y, width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(bytes: &[u8]) -> DiffReview {
        let mut review = DiffReview::new();
        append_patch(&mut review, "unstaged", bytes.to_vec()).unwrap();
        review
    }

    #[test]
    fn feedback_contains_only_selected_hunk_and_preserves_raw_content() {
        let bytes = b"diff --git a/a.txt b/a.txt\nindex 111..222 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\r\n+\x1b[31mnew\r\n@@ -20 +20 @@\n-second\n+replacement\n\\ No newline at end of file\n";
        let mut review = review(bytes);
        let first = review.feedback().unwrap();
        assert!(first.contains("-old\r\n+\x1b[31mnew\r\n"));
        assert!(!first.contains("replacement"));
        assert!(first.ends_with("Feedback: "));
        review.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let second = review.feedback().unwrap();
        assert!(
            second.contains("@@ -20 +20 @@\n-second\n+replacement\n\\ No newline at end of file\n")
        );
        assert!(!second.contains("-old"));
        assert!(second.contains("a.txt"));
    }

    #[test]
    fn metadata_is_reviewable_but_never_fabricates_a_hunk() {
        for bytes in [
            b"diff --git a/image b/image\nBinary files a/image and b/image differ\n".as_slice(),
            b"diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n",
            b"diff --git a/empty b/empty\nnew file mode 100644\nindex 0000000..e69de29\n",
        ] {
            let mut review = review(bytes);
            assert!(review.entries[0].hunk.is_none());
            assert_eq!(
                review.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                DiffAction::None
            );
            assert!(review.notice.as_deref().unwrap().contains("No text hunk"));
        }
    }

    #[test]
    fn malformed_hunk_counts_are_not_forwarded_as_exact_feedback() {
        for hunk in [
            b"@@ -1,2 +1 @@\n-old\n+new\n".as_slice(),
            b"@@ -1 +1 @@\n-old\n+new\n+extra\n",
            b"@@ -1 +1 @@\nunprefixed\n",
        ] {
            assert!(validate_hunk(hunk).is_err());
        }
        validate_hunk(b"@@ -0,0 +1 @@\n+new\n").unwrap();
        validate_hunk(b"@@ -1 +0,0 @@\n-old\n\\ No newline at end of file\n").unwrap();
    }

    #[test]
    fn non_utf8_patch_is_retained_but_cannot_become_lossy_feedback() {
        let bytes =
            b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@ function\xff\n-old\n+\xff\n";
        let review = review(bytes);
        assert_eq!(review.patches[0].bytes, bytes);
        assert!(review.feedback().unwrap_err().contains("not UTF-8"));
        let mut warning = false;
        review.visit_lines(|line, _| warning |= line.contains("Non-UTF-8"));
        assert!(warning);
    }

    #[test]
    fn git_quoted_paths_preserve_unicode_and_escape_terminal_controls() {
        assert_eq!(path_label(br#""b/caf\303\251.txt""#, true), "café.txt");
        assert_eq!(
            path_label(br#""b/tab\tand\nline""#, true),
            r#""tab\tand\nline""#
        );
        assert_eq!(path_label(b"b/with spaces.txt\t", true), "with spaces.txt");
        assert!(path_label(br#""b/nonutf\377""#, true).contains("Git byte-escaped path"));
        let deleted = review(
            b"diff --git a/deleted b/deleted\n--- a/deleted\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
        );
        assert_eq!(deleted.files[0].path, "deleted");
    }

    #[test]
    fn feedback_limit_refuses_instead_of_silently_truncating() {
        let mut review = review(b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n");
        review.files[0].path = "a".repeat(MAX_FEEDBACK_BYTES);
        assert!(review.feedback().unwrap_err().contains("not truncated"));
    }

    #[tokio::test]
    async fn output_limit_accepts_exact_boundary_and_refuses_overflow() {
        assert_eq!(
            read_limited(b"abc".as_slice(), 3, "patch").await.unwrap(),
            b"abc"
        );
        assert!(read_limited(b"abcd".as_slice(), 3, "patch").await.is_err());
    }

    #[cfg(unix)]
    mod filesystem_safety {
        use super::*;
        use std::{
            fs,
            os::unix::{
                ffi::OsStrExt,
                fs::{PermissionsExt, symlink},
            },
        };

        async fn git(root: &Path, args: &[&str]) -> Vec<u8> {
            let output = timeout(
                Duration::from_secs(5),
                Command::new("git")
                    .current_dir(root)
                    .args([
                        "-c",
                        "user.name=Diff Test",
                        "-c",
                        "user.email=diff@example.invalid",
                        "-c",
                        "commit.gpgSign=false",
                        "-c",
                        "core.hooksPath=/dev/null",
                    ])
                    .args(args)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn executable(path: &Path, script: &str) {
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }

        async fn commit_all(root: &Path) {
            git(root, &["add", "--all"]).await;
            git(root, &["commit", "-qm", "fixture"]).await;
        }

        #[tokio::test]
        async fn untracked_symlinks_never_read_targets_and_apply_with_exact_bytes() {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let applied = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-q"]).await;
            // The old no-index operand handling reads this instead of the link.
            fs::write(
                outside.path().join("null"),
                b"outside secret must not enter review\n",
            )
            .unwrap();
            let fifo = outside.path().join("fifo");
            assert!(
                Command::new("mkfifo")
                    .args(["-m", "600"])
                    .arg(&fifo)
                    .status()
                    .await
                    .unwrap()
                    .success()
            );
            let links = [
                (
                    OsString::from("directory\t\"é\\"),
                    outside.path().to_path_buf(),
                ),
                (OsString::from("fifo"), fifo),
                // APFS requires UTF-8 filenames; Linux exercises raw name bytes.
                #[cfg(target_os = "linux")]
                (
                    path_from_bytes(b"nonutf\xff").unwrap(),
                    PathBuf::from(path_from_bytes(b"missing\nraw\xff\n").unwrap()),
                ),
            ];
            for (name, target) in &links {
                symlink(target, root.path().join(name)).unwrap();
            }
            let review = load_diff(root.path().to_owned()).await.unwrap();
            assert_eq!(review.entries.len(), links.len());
            let mut patch = Vec::new();
            for part in &review.patches {
                patch.extend_from_slice(&part.bytes);
            }
            assert!(
                !patch
                    .windows(b"outside secret".len())
                    .any(|bytes| bytes == b"outside secret")
            );
            let patch_file = outside.path().join("addition.patch");
            fs::write(&patch_file, patch).unwrap();
            git(applied.path(), &["apply", patch_file.to_str().unwrap()]).await;
            for (name, target) in &links {
                let actual = applied.path().join(name);
                assert!(
                    fs::symlink_metadata(&actual)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(fs::read_link(actual).unwrap(), *target);
            }
            let (name, _) = &links[0];
            let path = root.path().join(name);
            let exact = symlink_patch(&path, name.as_bytes(), MAX_PATCH_BYTES)
                .await
                .unwrap();
            assert_eq!(
                symlink_patch(&path, name.as_bytes(), exact.len())
                    .await
                    .unwrap(),
                exact
            );
            assert!(
                symlink_patch(&path, name.as_bytes(), exact.len() - 1)
                    .await
                    .is_err()
            );
        }

        #[tokio::test]
        async fn configured_clean_process_and_textconv_helpers_never_run() {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-q"]).await;
            fs::write(
                root.path().join(".gitattributes"),
                "*.clean filter=unsafe.clean diff=unsafe\n*.process filter=unsafe.process\n",
            )
            .unwrap();
            fs::write(root.path().join("tracked.clean"), "original\n").unwrap();
            commit_all(root.path()).await;
            git(
                root.path(),
                &[
                    "config",
                    "filter.unsafe.clean.clean",
                    "printf invoked > clean-ran; cat",
                ],
            )
            .await;
            git(
                root.path(),
                &["config", "filter.unsafe.clean.required", "true"],
            )
            .await;
            git(
                root.path(),
                &[
                    "config",
                    "filter.unsafe.process.process",
                    "printf invoked > process-ran; exit 1",
                ],
            )
            .await;
            git(
                root.path(),
                &["config", "filter.unsafe.process.required", "true"],
            )
            .await;
            git(
                root.path(),
                &[
                    "config",
                    "diff.unsafe.textconv",
                    "printf invoked > textconv-ran; cat",
                ],
            )
            .await;
            git(
                root.path(),
                &[
                    "config",
                    "diff.external",
                    "printf invoked > external-ran; exit 1",
                ],
            )
            .await;
            fs::write(
                root.path().join("tracked.clean"),
                "raw tracked replacement\n",
            )
            .unwrap();
            fs::write(root.path().join("new.clean"), "raw clean content\n").unwrap();
            fs::write(root.path().join("new.process"), "raw process content\n").unwrap();
            let review = load_diff(root.path().to_owned()).await.unwrap();
            let patch = review
                .patches
                .iter()
                .map(|patch| String::from_utf8_lossy(&patch.bytes))
                .collect::<String>();
            assert!(patch.contains("-original\n+raw tracked replacement\n"));
            assert!(patch.contains("+raw clean content\n"));
            assert!(patch.contains("+raw process content\n"));
            for marker in ["clean-ran", "process-ran", "textconv-ran", "external-ran"] {
                assert!(
                    !root.path().join(marker).exists(),
                    "helper executed: {marker}"
                );
            }
        }

        #[tokio::test]
        async fn nested_submodule_filters_are_disabled_without_hiding_dirty_gitlinks() {
            let root = tempfile::tempdir().unwrap();
            let submodule = root.path().join("submodule");
            let nested = submodule.join("nested");
            fs::create_dir_all(&nested).unwrap();
            for repository in [root.path(), submodule.as_path(), nested.as_path()] {
                git(repository, &["init", "-q"]).await;
            }
            fs::write(nested.join(".gitattributes"), "*.txt filter=nested\n").unwrap();
            fs::write(nested.join("tracked.txt"), "original\n").unwrap();
            commit_all(&nested).await;
            commit_all(&submodule).await;
            commit_all(root.path()).await;
            git(
                &nested,
                &[
                    "config",
                    "filter.nested.clean",
                    "printf invoked > filter-ran; cat",
                ],
            )
            .await;
            git(&nested, &["config", "filter.nested.required", "true"]).await;
            fs::write(nested.join("tracked.txt"), "changed without a helper\n").unwrap();
            let review = load_diff(root.path().to_owned()).await.unwrap();
            let patch = review
                .patches
                .iter()
                .map(|patch| String::from_utf8_lossy(&patch.bytes))
                .collect::<String>();
            assert!(
                patch.contains("-dirty"),
                "nested worktree changes disappeared: {patch}"
            );
            assert!(!nested.join("filter-ran").exists());
        }

        #[tokio::test]
        async fn stat_only_changes_do_not_refresh_index_or_run_hooks_and_fsmonitor() {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-q"]).await;
            let path = root.path().join("tracked");
            fs::write(&path, "unchanged\n").unwrap();
            commit_all(root.path()).await;
            let hooks = root.path().join(".git/hooks");
            executable(
                &hooks.join("post-index-change"),
                "#!/bin/sh\nprintf invoked > hook-ran\n",
            );
            let monitor = root.path().join(".git/monitor");
            executable(&monitor, "#!/bin/sh\nprintf invoked > fsmonitor-ran\n");
            git(
                root.path(),
                &["config", "core.fsmonitor", monitor.to_str().unwrap()],
            )
            .await;
            git(root.path(), &["config", "diff.autoRefreshIndex", "true"]).await;
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1)),
                )
                .unwrap();
            let index = fs::read(root.path().join(".git/index")).unwrap();
            let review = load_diff(root.path().to_owned()).await.unwrap();
            assert!(review.entries.is_empty());
            assert_eq!(fs::read(root.path().join(".git/index")).unwrap(), index);
            for marker in [".git/index.lock", "hook-ran", "fsmonitor-ran"] {
                assert!(
                    !root.path().join(marker).exists(),
                    "read-only review created {marker}"
                );
            }
        }

        #[tokio::test]
        async fn missing_objects_fail_without_invoking_a_remote_helper() {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-q"]).await;
            fs::write(root.path().join("tracked"), "original\n").unwrap();
            commit_all(root.path()).await;
            let blob = git(root.path(), &["rev-parse", "HEAD:tracked"]).await;
            let blob = std::str::from_utf8(&blob).unwrap().trim();
            fs::remove_file(
                root.path()
                    .join(".git/objects")
                    .join(&blob[..2])
                    .join(&blob[2..]),
            )
            .unwrap();
            let helper = root.path().join(".git/remote-helper");
            executable(&helper, "#!/bin/sh\nprintf invoked > remote-ran\nexit 1\n");
            git(
                root.path(),
                &[
                    "config",
                    "remote.origin.url",
                    &format!("ext::{}", helper.display()),
                ],
            )
            .await;
            git(root.path(), &["config", "remote.origin.promisor", "true"]).await;
            git(root.path(), &["config", "protocol.ext.allow", "always"]).await;
            fs::write(root.path().join("tracked"), "modified\n").unwrap();
            assert!(load_diff(root.path().to_owned()).await.is_err());
            assert!(!root.path().join("remote-ran").exists());
        }

        async fn stopped(pid: i32) -> bool {
            for _ in 0..100 {
                if rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                    == Err(rustix::io::Errno::SRCH)
                {
                    return true;
                }
                // Grandchildren are reaped by the OS, not by Child::wait. A
                // zombie has already exited even on CI hosts with a slow init.
                let status = Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .kill_on_drop(true)
                    .output()
                    .await
                    .unwrap();
                if status
                    .stdout
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    == Some(&b'Z')
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }

        async fn helper_teardown(cancel: bool) {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-q"]).await;
            executable(
                &root.path().join("helper"),
                "#!/bin/sh\nsleep 60 &\nprintf '%s %s\\n' \"$$\" \"$!\" > pids\nwhile [ ! -f release ]; do sleep 0.01; done\nprintf overflow\nwait\n",
            );
            let path = root.path().to_owned();
            let mut task = tokio::spawn(async move {
                run_git(
                    &path,
                    &[
                        "-c".into(),
                        "alias.review-fixture=!./helper".into(),
                        "review-fixture".into(),
                    ],
                    &[],
                    3,
                    false,
                )
                .await
            });
            let ready = timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = tokio::fs::read_to_string(root.path().join("pids")).await {
                        let pids = text
                            .split_whitespace()
                            .map(str::parse::<i32>)
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap();
                        if pids.len() == 2 {
                            break pids;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            let pids = match ready {
                Ok(pids) => pids,
                Err(error) => {
                    task.abort();
                    let _ = task.await;
                    panic!("Git helper did not become ready: {error}");
                }
            };
            let group = rustix::process::getpgid(rustix::process::Pid::from_raw(pids[0])).ok();
            if group.is_none_or(|group| {
                group == rustix::process::getpgrp() || group == rustix::process::Pid::INIT
            }) {
                task.abort();
                let _ = task.await;
                for &pid in &pids {
                    let _ = rustix::process::kill_process(
                        rustix::process::Pid::from_raw(pid).unwrap(),
                        rustix::process::Signal::KILL,
                    );
                }
                panic!("Git helper did not receive an isolated process group");
            }
            struct Cleanup(rustix::process::Pid);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ =
                        rustix::process::kill_process_group(self.0, rustix::process::Signal::KILL);
                }
            }
            let cleanup = Cleanup(group.expect("isolated process group"));
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                fs::write(root.path().join("release"), "").unwrap();
                let error = timeout(Duration::from_secs(5), &mut task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert!(error.contains("output limit"), "{error}");
            }
            let helper_stopped = stopped(pids[0]).await;
            let descendant_stopped = stopped(pids[1]).await;
            // Clean up even when the regression is present, before asserting.
            drop(cleanup);
            assert!(helper_stopped, "Git's helper survived teardown");
            assert!(
                descendant_stopped,
                "Git's sleeping descendant survived teardown"
            );
        }

        #[tokio::test]
        async fn cancelling_diff_kills_helper_descendants() {
            helper_teardown(true).await;
        }

        #[tokio::test]
        async fn output_limit_kills_helper_descendants() {
            helper_teardown(false).await;
        }
    }
}
