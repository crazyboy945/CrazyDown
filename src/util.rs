use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use percent_encoding::percent_decode_str;
use url::Url;

pub fn absolutize(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    normalize(&joined)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn filename_from_url(url: &Url) -> Option<String> {
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))?;
    let decoded = percent_decode_str(segment).decode_utf8_lossy().to_string();
    sanitize_filename(&decoded)
}

pub fn filename_from_content_disposition(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_header_params(header).into_iter().skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "filename" => plain = Some(unquote(value)),
            "filename*" => extended = decode_rfc5987(value),
            _ => {}
        }
    }
    let raw = extended.or(plain)?;
    sanitize_filename(&raw)
}

fn split_header_params(header: &str) -> Vec<&str> {
    let bytes = header.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, byte) in bytes.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b';' if !in_quotes => {
                parts.push(&header[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&header[start..]);
    parts
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        trimmed.to_string()
    }
}

fn decode_rfc5987(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let (charset, rest) = trimmed.split_once('\'')?;
    let (_language, encoded) = rest.split_once('\'')?;
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" | "" => String::from_utf8(bytes).ok(),
        "iso-8859-1" => Some(bytes.into_iter().map(|b| b as char).collect()),
        _ => None,
    }
}

pub fn sanitize_filename(name: &str) -> Option<String> {
    let base = name
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if base.is_empty() {
        return None;
    }

    let mut cleaned: String = base
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect();

    while cleaned.ends_with(' ') || cleaned.ends_with('.') {
        cleaned.pop();
    }
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }

    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        cleaned.insert(0, '_');
    }

    const MAX_CHARS: usize = 180;
    if cleaned.chars().count() > MAX_CHARS {
        let ext = Path::new(&cleaned)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .filter(|e| e.chars().count() <= 20)
            .unwrap_or_default();
        let keep = MAX_CHARS.saturating_sub(ext.chars().count());
        let stem: String = cleaned.chars().take(keep).collect();
        cleaned = format!("{stem}{ext}");
    }

    Some(cleaned)
}

pub fn unique_filename(name: &str, index: usize) -> String {
    if index == 0 {
        return name.to_string();
    }
    let path = Path::new(name);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let stem = name
        .strip_suffix(&ext)
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    format!("{stem} ({index}){ext}")
}

pub fn source_label(source: &str) -> String {
    if source.starts_with("magnet:") {
        return "BT 磁力任务".to_string();
    }
    let path = Path::new(source);
    if path.exists() && path.is_file() {
        return path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("种子任务")
            .to_string();
    }
    if let Ok(url) = Url::parse(source) {
        if let Some(name) = filename_from_url(&url) {
            return name;
        }
        if let Some(host) = url.host_str() {
            return host.to_string();
        }
    }
    source.to_string()
}

pub fn display_source(source: &str) -> String {
    if source.starts_with("magnet:") {
        return truncate_chars(source, 72);
    }
    if let Ok(mut url) = Url::parse(source) {
        if !matches!(url.scheme(), "http" | "https" | "ftp") {
            return truncate_chars(source, 100);
        }
        if url.password().is_some() {
            let _ = url.set_password(Some("***"));
        }
        url.set_query(None);
        url.set_fragment(None);
        return url.to_string();
    }
    truncate_chars(source, 100)
}

pub fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = value.chars().take(keep).collect();
    out.push_str("...");
    out
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 1.0 {
        return format!("{:.0} 毫秒", secs * 1000.0);
    }
    if secs < 60.0 {
        return format!("{secs:.1} 秒");
    }
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours} 小时 {minutes} 分 {seconds} 秒")
    } else {
        format!("{minutes} 分 {seconds} 秒")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_content_disposition() {
        let name = filename_from_content_disposition("attachment; filename=\"报告 2026.zip\"");
        assert_eq!(name.as_deref(), Some("报告 2026.zip"));
    }

    #[test]
    fn prefers_rfc5987_filename() {
        let name = filename_from_content_disposition(
            "attachment; filename=\"fallback.zip\"; filename*=UTF-8''%E6%B5%8B%E8%AF%95.zip",
        );
        assert_eq!(name.as_deref(), Some("测试.zip"));
    }

    #[test]
    fn sanitizes_windows_invalid_characters() {
        let name = sanitize_filename("a<b>:c*d?.zip").unwrap();
        assert_eq!(name, "a_b__c_d_.zip");
    }

    #[test]
    fn rejects_path_traversal_names() {
        assert_eq!(sanitize_filename("../.."), None);
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("dir/"), None);
    }

    #[test]
    fn creates_unique_names() {
        assert_eq!(unique_filename("file.zip", 2), "file (2).zip");
        assert_eq!(unique_filename("archive.tar.gz", 1), "archive.tar (1).gz");
    }

    #[test]
    fn hides_credentials_and_query_in_display_source() {
        let shown = display_source("ftp://alice:secret@example.com/file.zip?token=abc");
        assert_eq!(shown, "ftp://alice:***@example.com/file.zip");
    }
}
