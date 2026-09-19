use std::{
    borrow::Cow,
    collections::{BinaryHeap, VecDeque},
    ffi::OsString,
    io::{self, Read},
    path::{Path, PathBuf},
};

use kurama_protocol::traits::CancelSignal;

const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    ".next",
    ".nuxt",
    ".turbo",
    ".capy",
    ".kurama",
    "__pycache__",
    "target",
    "node_modules",
    "dist",
    "build",
    "cache",
    "coverage",
    "venv",
];
const MAX_FILES: usize = 400;
const MAX_DEPTH: usize = 6;
const MAX_DIRECTORIES: usize = 400;
const MAX_ROWS: usize = 8;
pub const MAX_PASTE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const IMAGE_TOO_LARGE: &str = "Pasted image exceeds the 8 MiB limit";

pub fn mention_at_cursor(composer: &str, cursor: usize) -> Option<(usize, &str)> {
    let mut cursor = cursor.min(composer.len());
    while !composer.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &composer[..cursor];
    let start = before.rfind('@')?;
    if start > 0 {
        let previous = before[..start].chars().next_back()?;
        if !previous.is_whitespace() {
            return None;
        }
    }
    let query = &before[start + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((start, query))
}

pub fn collect_files(root: &Path, cancel: &dyn CancelSignal) -> Vec<String> {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0_usize)]);
    // A total admission budget bounds both the queue and empty-directory work.
    let mut remaining_dirs = MAX_DIRECTORIES - 1;
    while let Some((dir, depth)) = queue.pop_front() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        if files.len() >= MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names = BinaryHeap::new();
        let mut dirs = BinaryHeap::new();
        for entry in entries {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            let Ok(entry) = entry else {
                continue;
            };
            let name = entry.file_name();
            let Some(text) = name.to_str() else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.contains(&text) {
                    retain_smallest(&mut dirs, name, remaining_dirs);
                }
            } else if file_type.is_file() {
                retain_smallest(&mut names, name, MAX_FILES - files.len());
            }
        }
        for name in names.into_sorted_vec() {
            let path = dir.join(name);
            if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
        remaining_dirs -= dirs.len();
        for name in dirs.into_sorted_vec() {
            queue.push_back((dir.join(name), depth + 1));
        }
    }
    files.sort();
    files
}

fn retain_smallest(names: &mut BinaryHeap<OsString>, name: OsString, limit: usize) {
    // Scan every entry, retaining only the lexicographically smallest candidates.
    // Truncating read_dir itself would make admission depend on filesystem order.
    if names.len() < limit {
        names.push(name);
    } else if let Some(mut largest) = names.peek_mut()
        && name < *largest
    {
        *largest = name;
    }
}

pub fn filter_files<'a>(files: &'a [String], query: &str) -> Vec<&'a str> {
    let query = query.to_lowercase();
    let mut best = Vec::with_capacity(MAX_ROWS + 1);
    for path in files {
        // ASCII paths need no per-row allocation; Unicode paths use the same
        // lowercase matching as the query, including expanding lowercase forms.
        let normalized = if path.is_ascii() || query.is_empty() {
            Cow::Borrowed(path.as_str())
        } else {
            Cow::Owned(path.to_lowercase())
        };
        if !subsequence(&normalized, &query) {
            continue;
        }
        let score = if query.is_empty() {
            path.len()
        } else if normalized
            .rsplit('/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(&query))
        {
            0
        } else if normalized
            .as_bytes()
            .windows(query.len())
            .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
        {
            1
        } else {
            2
        };
        let candidate = (score, path.len(), path.as_str());
        let index = best.partition_point(|existing| existing < &candidate);
        if index < MAX_ROWS {
            best.insert(index, candidate);
            best.truncate(MAX_ROWS);
        }
    }
    best.into_iter().map(|(_, _, path)| path).collect()
}

fn subsequence(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|got| got.eq_ignore_ascii_case(&wanted)))
}

pub fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

pub fn decode_pasted_image(paste: &str) -> Result<Option<(Vec<u8>, &'static str)>, &'static str> {
    let trimmed = paste.trim();
    if trimmed.starts_with("file://") {
        if trimmed.len() > MAX_PASTE_IMAGE_BYTES {
            return Err("Pasted image path is too large");
        }
        let Some(path) = path_from_file_url(trimmed) else {
            return Ok(None);
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            return Ok(None);
        };
        return image_from_path(&path, metadata);
    }
    let payload = trimmed
        .strip_prefix("data:image/png;base64,")
        .or_else(|| trimmed.strip_prefix("data:image/jpeg;base64,"))
        .or_else(|| trimmed.strip_prefix("data:image/gif;base64,"))
        .or_else(|| trimmed.strip_prefix("data:image/webp;base64,"));
    if let Some(payload) = payload {
        return Ok(decode_base64(payload)?
            .and_then(|bytes| image_extension(&bytes).map(|ext| (bytes, ext))));
    }
    let mut prefix = [0_u8; 10];
    let mut prefix_len = 0;
    for byte in trimmed
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .take(prefix.len())
    {
        prefix[prefix_len] = byte;
        prefix_len += 1;
    }
    let prefix = &prefix[..prefix_len];
    if (prefix.starts_with(b"iVBORw0KGg")
        || prefix.starts_with(b"/9j/")
        || prefix.starts_with(b"R0lGOD")
        || prefix.starts_with(b"UklGR"))
        && let Some(bytes) = decode_base64(trimmed)?
        && let Some(ext) = image_extension(&bytes)
    {
        return Ok(Some((bytes, ext)));
    }
    // A raw image prefix is only a guess: filenames can start with it too.
    // Large text cannot be a supported pathname; avoid making a native copy.
    if trimmed.len() > MAX_PASTE_IMAGE_BYTES {
        return Ok(None);
    }
    let path = Path::new(trimmed);
    match std::fs::metadata(path) {
        Ok(metadata) => image_from_path(path, metadata),
        Err(_) => Ok(None),
    }
}

fn image_from_path(
    path: &Path,
    metadata: std::fs::Metadata,
) -> Result<Option<(Vec<u8>, &'static str)>, &'static str> {
    if !metadata.is_file() {
        return Ok(None);
    }
    let path_ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(known_ext);
    if path_ext.is_some() && metadata.len() > MAX_PASTE_IMAGE_BYTES as u64 {
        return Err(IMAGE_TOO_LARGE);
    }
    // Metadata can race with replacement by a FIFO. Nonblocking open plus an
    // opened-handle check rejects special files without waiting for a writer.
    #[cfg(unix)]
    let file = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map(std::fs::File::from);
    #[cfg(not(unix))]
    let file = std::fs::File::open(path);
    let Ok(file) = file else {
        return Ok(None);
    };
    let Ok(metadata) = file.metadata() else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    read_image(file, metadata.len(), path_ext)
}

fn read_image(
    reader: impl Read,
    file_len: u64,
    path_ext: Option<&'static str>,
) -> Result<Option<(Vec<u8>, &'static str)>, &'static str> {
    if path_ext.is_some() && file_len > MAX_PASTE_IMAGE_BYTES as u64 {
        return Err(IMAGE_TOO_LARGE);
    }
    let mut reader = reader.take(MAX_PASTE_IMAGE_BYTES as u64 + 1);
    let mut bytes = Vec::with_capacity(12);
    if reader.by_ref().take(12).read_to_end(&mut bytes).is_err() {
        return Ok(None);
    }
    let Some(ext) = image_extension(&bytes).or(path_ext) else {
        return Ok(None);
    };
    if file_len > MAX_PASTE_IMAGE_BYTES as u64 {
        return Err(IMAGE_TOO_LARGE);
    }
    bytes.reserve_exact((file_len as usize).saturating_sub(bytes.len()));
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(None),
        };
        let required = bytes.len() + read;
        if required > MAX_PASTE_IMAGE_BYTES {
            return Err(IMAGE_TOO_LARGE);
        }
        if required > bytes.capacity() {
            // A growing file must not make Vec's geometric growth exceed the cap.
            let capacity = (bytes.capacity() * 2)
                .max(required)
                .min(MAX_PASTE_IMAGE_BYTES);
            bytes.reserve_exact(capacity - bytes.len());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(Some((bytes, ext)))
}

fn path_from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)?))
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = from_hex(bytes.get(index + 1).copied()?)?;
            let lo = from_hex(bytes.get(index + 2).copied()?)?;
            out.push((hi << 4) | lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn known_ext(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        _ => None,
    }
}

fn decode_base64(input: &str) -> Result<Option<Vec<u8>>, &'static str> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    // Count and validate without allocating a filtered copy of the paste.
    let symbols = || {
        input
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
    };
    let mut encoded_len = 0;
    for byte in symbols() {
        if value(byte).is_none() {
            return Ok(None);
        }
        encoded_len += 1;
    }
    let decoded_len = encoded_len / 4 * 3 + encoded_len % 4 * 3 / 4;
    if decoded_len > MAX_PASTE_IMAGE_BYTES {
        return Err(IMAGE_TOO_LARGE);
    }
    if encoded_len % 4 == 1 {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(decoded_len);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in symbols() {
        buffer = (buffer << 6) | u32::from(value(byte).expect("validated base64"));
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::{
        MAX_DEPTH, MAX_DIRECTORIES, MAX_FILES, MAX_PASTE_IMAGE_BYTES, collect_files,
        decode_pasted_image, filter_files, mention_at_cursor, read_image,
    };

    #[test]
    fn mention_requires_a_boundary_at_sign() {
        assert_eq!(mention_at_cursor("see @src/lib", 12), Some((4, "src/lib")));
        assert!(mention_at_cursor("email@x", 7).is_none());
        assert!(mention_at_cursor("@src foo", 8).is_none());
        assert_eq!(mention_at_cursor("@界", 2), Some((0, "")));
        assert_eq!(mention_at_cursor("@界", usize::MAX), Some((0, "界")));
    }

    #[test]
    fn oversized_file_url_is_rejected_before_path_conversion() {
        let url = format!("file://{}.png", "a".repeat(MAX_PASTE_IMAGE_BYTES));
        assert!(decode_pasted_image(&url).is_err());
        assert!(
            decode_pasted_image(&"x".repeat(MAX_PASTE_IMAGE_BYTES + 1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn file_filter_ranks_basename_matches_first() {
        let files = [
            "crates/cli/src/lib.rs".into(),
            "src/lib.rs".into(),
            "README.md".into(),
        ];
        let matches = filter_files(&files, "lib");
        assert_eq!(matches[0], "src/lib.rs");
    }

    #[test]
    fn file_filter_preserves_unicode_case_matches_and_ranking() {
        let files = [
            "notes/CAFÉ.rs".into(),
            "CAF---É.rs".into(),
            "src/café.rs".into(),
            "CAFÉ.rs".into(),
            "unrelated.rs".into(),
        ];
        let expected = ["CAFÉ.rs", "src/café.rs", "notes/CAFÉ.rs", "CAF---É.rs"];
        assert_eq!(filter_files(&files, "café.rs"), expected);
        assert_eq!(filter_files(&files, "CAFÉ.RS"), expected);
        assert_eq!(filter_files(&["İ.rs".into()], "i\u{307}"), ["İ.rs"]);
        assert_eq!(
            filter_files(&["src/LIB.RS".into()], "lib.rs"),
            ["src/LIB.RS"]
        );
        assert_eq!(
            filter_files(&["界.txt".into(), "z".into(), "a".into()], ""),
            ["a", "z", "界.txt"]
        );
    }

    #[test]
    fn file_index_observes_cancellation_between_entries_and_directories() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use kurama_protocol::traits::{BoxFuture, CancelSignal};

        struct CancelAfter(AtomicUsize);

        impl CancelSignal for CancelAfter {
            fn is_cancelled(&self) -> bool {
                self.0
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                        left.checked_sub(1)
                    })
                    .is_err()
            }

            fn cancelled(&self) -> BoxFuture<'static, ()> {
                Box::pin(std::future::pending())
            }
        }

        let root = tempfile::tempdir().expect("temp");
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.path().join(name), b"").expect("write");
        }
        // Permit entering the directory and reading one entry, then cancel.
        // A directory-only checkpoint would incorrectly publish this index.
        let cancel = CancelAfter(AtomicUsize::new(2));
        assert!(collect_files(root.path(), &cancel).is_empty());

        let nested = tempfile::tempdir().expect("temp");
        std::fs::create_dir(nested.path().join("src")).expect("directory");
        std::fs::write(nested.path().join("src/visible.rs"), b"").expect("write");
        // Permit entering the root and admitting its child directory, then cancel.
        // Entry-only checks would still admit the child's sole file.
        let cancel = CancelAfter(AtomicUsize::new(2));
        assert!(collect_files(nested.path(), &cancel).is_empty());
    }

    #[test]
    fn file_admission_is_sorted_before_the_limit_for_any_creation_order() {
        let forward = tempfile::tempdir().expect("temp");
        let reverse = tempfile::tempdir().expect("temp");
        let count = MAX_FILES + 37;
        for index in 0..count {
            std::fs::write(forward.path().join(format!("file-{index:04}.rs")), b"").expect("write");
            std::fs::write(
                reverse
                    .path()
                    .join(format!("file-{:04}.rs", count - index - 1)),
                b"",
            )
            .expect("write");
        }
        let expected: Vec<_> = (0..MAX_FILES)
            .map(|index| format!("file-{index:04}.rs"))
            .collect();
        let cancel = kurama_core::cancel::CancelToken::new();
        assert_eq!(collect_files(forward.path(), &cancel), expected);
        assert_eq!(collect_files(reverse.path(), &cancel), expected);
    }

    #[test]
    fn directory_admission_is_deterministic_and_bounded() {
        let forward = tempfile::tempdir().expect("temp");
        let reverse = tempfile::tempdir().expect("temp");
        let count = MAX_DIRECTORIES + 17;
        for index in 0..count {
            for (root, number) in [(forward.path(), index), (reverse.path(), count - index - 1)] {
                let dir = root.join(format!("dir-{number:04}"));
                std::fs::create_dir(&dir).expect("directory");
                std::fs::write(dir.join("source.rs"), b"").expect("write");
            }
        }
        let expected: Vec<_> = (0..MAX_DIRECTORIES - 1)
            .map(|index| format!("dir-{index:04}/source.rs"))
            .collect();
        let cancel = kurama_core::cancel::CancelToken::new();
        assert_eq!(collect_files(forward.path(), &cancel), expected);
        assert_eq!(collect_files(reverse.path(), &cancel), expected);
    }

    #[test]
    fn useful_dot_directories_survive_while_generated_trees_and_deep_files_do_not() {
        let root = tempfile::tempdir().expect("temp");
        let visible = [
            ".github/workflows/check.yml",
            ".config/tool/config.toml",
            ".devcontainer/devcontainer.json",
            ".vscode/settings.json",
            ".gitignore",
        ];
        let hidden = [
            ".git/objects/object",
            ".cache/cached.rs",
            "cache/cached.rs",
            "target/debug/generated.rs",
            "node_modules/module/index.js",
            "src/__pycache__/compiled.pyc",
            ".venv/lib/module.py",
        ];
        for name in visible.iter().chain(&hidden) {
            let path = root.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
            std::fs::write(path, b"").expect("write");
        }
        let mut deepest = root.path().to_path_buf();
        for _ in 0..MAX_DEPTH {
            deepest.push("nested");
        }
        std::fs::create_dir_all(deepest.join("too-deep")).expect("directories");
        std::fs::write(deepest.join("included.rs"), b"").expect("write");
        std::fs::write(deepest.join("too-deep/excluded.rs"), b"").expect("write");
        let mut expected: Vec<_> = visible.into_iter().map(str::to_owned).collect();
        expected.push(format!("{}included.rs", "nested/".repeat(MAX_DEPTH)));
        expected.sort();
        assert_eq!(
            collect_files(root.path(), &kurama_core::cancel::CancelToken::new()),
            expected
        );
    }

    #[test]
    fn png_file_path_paste_loads() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("shot.png");
        let bytes = b"\x89PNG\r\n\x1a\nrest";
        std::fs::write(&path, bytes).expect("write");
        let decoded = decode_pasted_image(path.to_str().expect("utf8"))
            .expect("within limit")
            .expect("png");
        assert_eq!(decoded.1, "png");
        assert!(decoded.0.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn file_url_percent_decoding_loads_png() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("my shot.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nrest").expect("write");
        let encoded = path.to_str().expect("utf8").replace(' ', "%20");
        let decoded = decode_pasted_image(&format!("file://{encoded}"))
            .expect("within limit")
            .expect("png");
        assert_eq!(decoded.1, "png");
    }

    #[test]
    fn raw_base64_prefix_filenames_fall_back_to_relative_path_detection() {
        let bytes = b"\x89PNG\r\n\x1a\nrest";
        for prefix in ["R0lGOD-screenshot-", "UklGR"] {
            // Keep the test parallel-safe: unique relative names, no cwd mutation.
            let file = tempfile::Builder::new()
                .prefix(prefix)
                .suffix(".png")
                .tempfile_in(".")
                .expect("relative image");
            std::fs::write(file.path(), bytes).expect("write image");
            let relative = file.path().file_name().unwrap().to_str().unwrap();
            assert_eq!(
                decode_pasted_image(relative),
                Ok(Some((bytes.to_vec(), "png")))
            );
            file.as_file()
                .set_len(MAX_PASTE_IMAGE_BYTES as u64 + 1)
                .expect("sparse image");
            assert!(decode_pasted_image(relative).is_err());
        }
    }

    #[test]
    fn webp_data_uri_decodes() {
        let bytes = b"RIFF\x18\0\0\0WEBP rest";
        let mut encoded = String::new();
        encode_base64(bytes, &mut encoded);
        let decoded = decode_pasted_image(&format!("data:image/webp;base64,{encoded}"))
            .expect("within limit")
            .expect("webp");
        assert_eq!(decoded.1, "webp");
        assert!(decoded.0.starts_with(b"RIFF"));
    }

    #[test]
    fn oversized_sparse_images_report_errors_but_unrelated_files_remain_text() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("large.PNG");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("write header");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open image");
        file.set_len(MAX_PASTE_IMAGE_BYTES as u64 + 1)
            .expect("sparse image");
        assert!(decode_pasted_image(path.to_str().expect("utf8")).is_err());
        assert!(decode_pasted_image(&format!("file://{}", path.display())).is_err());
        let unknown = root.path().join("image-without-extension");
        std::fs::rename(path, &unknown).expect("rename");
        assert!(decode_pasted_image(unknown.to_str().expect("utf8")).is_err());
        let text = root.path().join("large.txt");
        std::fs::File::create(&text)
            .expect("create text")
            .set_len(MAX_PASTE_IMAGE_BYTES as u64 + 1)
            .expect("sparse text");
        assert_eq!(decode_pasted_image(text.to_str().expect("utf8")), Ok(None));
    }

    #[test]
    fn image_file_at_exact_byte_limit_is_accepted() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("exact.png");
        let header = b"\x89PNG\r\n\x1a\n";
        std::fs::write(&path, header).expect("write header");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open image")
            .set_len(MAX_PASTE_IMAGE_BYTES as u64)
            .expect("sparse image");
        let (bytes, ext) = decode_pasted_image(path.to_str().expect("utf8"))
            .expect("within limit")
            .expect("png");
        assert_eq!(bytes.len(), MAX_PASTE_IMAGE_BYTES);
        assert_eq!(&bytes[..header.len()], header);
        assert_eq!(bytes.last(), Some(&0));
        assert_eq!(ext, "png");
    }

    #[test]
    fn image_growth_is_rejected_after_reading_only_one_byte_past_the_limit() {
        let header = b"\x89PNG\r\n\x1a\n";
        let mut reader = io::Cursor::new(header)
            .chain(io::repeat(0))
            .take(MAX_PASTE_IMAGE_BYTES as u64 + 64);
        assert!(read_image(&mut reader, header.len() as u64, None).is_err());
        assert_eq!(reader.limit(), 63);
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_not_images_even_with_a_known_extension() {
        let root = tempfile::tempdir().expect("temp");
        let path = root.path().join("device.png");
        std::os::unix::fs::symlink("/dev/null", &path).expect("symlink");
        assert_eq!(decode_pasted_image(path.to_str().expect("utf8")), Ok(None));
        assert_eq!(
            decode_pasted_image(&format!("file://{}", path.display())),
            Ok(None)
        );
    }

    #[test]
    fn base64_exact_byte_limit_is_accepted_and_one_byte_more_is_an_error() {
        const PREFIX: &str = "data:image/png;base64,";
        let mut bytes = vec![0; MAX_PASTE_IMAGE_BYTES];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut encoded =
            String::with_capacity(PREFIX.len() + (MAX_PASTE_IMAGE_BYTES + 1).div_ceil(3) * 4);
        encoded.push_str(PREFIX);
        encode_base64(&bytes, &mut encoded);
        {
            let (decoded, ext) = decode_pasted_image(&encoded)
                .expect("within limit")
                .expect("png");
            assert_eq!(decoded, bytes);
            assert_eq!(ext, "png");
        }
        bytes.push(0);
        encoded.truncate(PREFIX.len());
        encode_base64(&bytes, &mut encoded);
        assert!(decode_pasted_image(&encoded).is_err());
        assert!(decode_pasted_image(&encoded[PREFIX.len()..]).is_err());
    }

    #[test]
    fn raw_base64_whitespace_and_text_fallback_are_preserved() {
        let bytes = b"\x89PNG\r\n\x1a\nrest";
        let mut encoded = String::new();
        encode_base64(bytes, &mut encoded);
        encoded.insert_str(12, " \n\t");
        assert_eq!(
            decode_pasted_image(&encoded),
            Ok(Some((bytes.to_vec(), "png")))
        );
        assert_eq!(decode_pasted_image("ordinary pasted text"), Ok(None));
        assert_eq!(
            decode_pasted_image("data:image/png;base64,not*base64"),
            Ok(None)
        );
        assert_eq!(decode_pasted_image("file:///%not-hex"), Ok(None));
    }

    fn encode_base64(bytes: &[u8], out: &mut String) {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for chunk in bytes.chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            out.push(TABLE[(a >> 2) as usize] as char);
            out.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(c & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
}
