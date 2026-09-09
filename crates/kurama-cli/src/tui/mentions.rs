use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "dist", ".capy", ".kurama"];
const MAX_FILES: usize = 400;
const MAX_DEPTH: usize = 6;
const MAX_ROWS: usize = 8;

pub fn mention_at_cursor(composer: &str, cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(composer.len());
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
    Some((start, query.to_owned()))
}

pub fn collect_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0_usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if files.len() >= MAX_FILES || depth > MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if SKIP_DIRS.contains(&name) || name.starts_with('.') {
                    continue;
                }
                dirs.push(path);
            } else if file_type.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
                if files.len() >= MAX_FILES {
                    break;
                }
            }
        }
        dirs.sort();
        for path in dirs {
            queue.push_back((path, depth + 1));
        }
    }
    files.sort();
    files
}

pub fn filter_files<'a>(files: &'a [String], query: &str) -> Vec<&'a str> {
    let query = query.to_ascii_lowercase();
    let mut scored = files
        .iter()
        .filter_map(|path| {
            let lowered = path.to_ascii_lowercase();
            if !subsequence(&lowered, &query) {
                return None;
            }
            let score = if query.is_empty() {
                path.len()
            } else if lowered.rsplit('/').next() == Some(query.as_str()) {
                0
            } else if lowered.contains(&query) {
                1
            } else {
                2
            };
            Some((score, path.len(), path.as_str()))
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, len, path)| (*score, *len, *path));
    scored
        .into_iter()
        .take(MAX_ROWS)
        .map(|(_, _, path)| path)
        .collect()
}

fn subsequence(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = haystack.chars();
    needle.chars().all(|wanted| chars.any(|got| got == wanted))
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

pub fn decode_pasted_image(paste: &str) -> Option<(Vec<u8>, &'static str)> {
    let trimmed = paste.trim();
    if trimmed.starts_with("file://") {
        let path = path_from_file_url(trimmed)?;
        return image_from_path(&path);
    }
    let path = Path::new(trimmed);
    if path.is_file() {
        return image_from_path(path);
    }
    let payload = trimmed
        .strip_prefix("data:image/png;base64,")
        .or_else(|| trimmed.strip_prefix("data:image/jpeg;base64,"))
        .or_else(|| trimmed.strip_prefix("data:image/gif;base64,"))
        .or_else(|| trimmed.strip_prefix("data:image/webp;base64,"));
    if let Some(payload) = payload {
        let bytes = decode_base64(payload)?;
        let ext = image_extension(&bytes)?;
        return Some((bytes, ext));
    }
    if trimmed.starts_with("iVBORw0KGgo")
        || trimmed.starts_with("/9j/")
        || trimmed.starts_with("R0lGOD")
        || trimmed.starts_with("UklGR")
    {
        let bytes = decode_base64(trimmed)?;
        let ext = image_extension(&bytes)?;
        return Some((bytes, ext));
    }
    None
}

fn image_from_path(path: &Path) -> Option<(Vec<u8>, &'static str)> {
    let bytes = std::fs::read(path).ok()?;
    let ext = image_extension(&bytes).or_else(|| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(known_ext)
    })?;
    Some((bytes, ext))
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

fn decode_base64(input: &str) -> Option<Vec<u8>> {
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
    let filtered = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
        .collect::<Vec<_>>();
    if filtered.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    for chunk in filtered.chunks(4) {
        let a = value(chunk[0])?;
        let b = value(*chunk.get(1)?)?;
        out.push((a << 2) | (b >> 4));
        if let Some(&c) = chunk.get(2) {
            let c = value(c)?;
            out.push((b << 4) | (c >> 2));
            if let Some(&d) = chunk.get(3) {
                let d = value(d)?;
                out.push((c << 6) | d);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{decode_pasted_image, filter_files, mention_at_cursor};

    #[test]
    fn mention_requires_a_boundary_at_sign() {
        assert_eq!(
            mention_at_cursor("see @src/lib", 12),
            Some((4, "src/lib".into()))
        );
        assert!(mention_at_cursor("email@x", 7).is_none());
        assert!(mention_at_cursor("@src foo", 8).is_none());
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
    fn png_file_path_paste_loads() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("shot.png");
        let bytes = b"\x89PNG\r\n\x1a\nrest";
        std::fs::write(&path, bytes).expect("write");
        let decoded = decode_pasted_image(path.to_str().expect("utf8")).expect("png");
        assert_eq!(decoded.1, "png");
        assert!(decoded.0.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn file_url_percent_decoding_loads_png() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("my shot.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nrest").expect("write");
        let encoded = path.to_str().expect("utf8").replace(' ', "%20");
        let decoded = decode_pasted_image(&format!("file://{encoded}")).expect("png");
        assert_eq!(decoded.1, "png");
    }

    #[test]
    fn webp_data_uri_decodes() {
        let bytes = b"RIFF\x18\0\0\0WEBP rest";
        let mut encoded = String::new();
        encode_base64(bytes, &mut encoded);
        let decoded =
            decode_pasted_image(&format!("data:image/webp;base64,{encoded}")).expect("webp");
        assert_eq!(decoded.1, "webp");
        assert!(decoded.0.starts_with(b"RIFF"));
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
