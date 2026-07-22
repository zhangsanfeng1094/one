//! Image helpers shared by tools, TUI paste, and providers.
//!
//! ## Storage model (Codex-style)
//! - **Local / session**: file path only (`~/.one/agent/media/…` or workspace)
//! - **API**: providers read path → base64 / data-URL at request time

use base64::Engine;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard cap for raw image bytes accepted into agent context.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// `~/.one/agent/media` — durable store for pasted / clipboard images.
pub fn media_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".one").join("agent").join("media")
}

/// Extension for a supported image MIME type.
pub fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => "bin",
    }
}

/// Write raw image bytes into the media store. Returns `(absolute_path, mime)`.
///
/// `mime_hint` is ignored when magic bytes sniff succeeds (always required).
pub fn store_image_bytes(
    bytes: &[u8],
    _mime_hint: Option<&str>,
) -> Result<(PathBuf, String), String> {
    if bytes.is_empty() {
        return Err("image is empty".into());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large ({} bytes > {} max)",
            bytes.len(),
            MAX_IMAGE_BYTES
        ));
    }
    // Always require a real image signature.
    let mime = mime_from_bytes(bytes)
        .ok_or_else(|| "not a supported image (png/jpeg/gif/webp/bmp)".to_string())?
        .to_string();

    let dir = media_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create media dir: {e}"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("{nanos:x}_{:x}.{}", std::process::id(), ext_for_mime(&mime));
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(|e| format!("write media: {e}"))?;
    Ok((path, mime))
}

/// Decode base64 and store into the media dir.
pub fn store_image_base64(
    data_b64: &str,
    mime_hint: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let bytes = decode_base64(data_b64).map_err(|e| format!("base64: {e}"))?;
    store_image_bytes(&bytes, mime_hint)
}

/// Copy an existing image file into the media store (stable path for sessions).
pub fn import_image_file(src: &Path) -> Result<(PathBuf, String), String> {
    let bytes = std::fs::read(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    let hint = mime_from_path(src);
    store_image_bytes(&bytes, hint)
}

/// Prompt placeholder for image id 1 (deleting this token detaches the image).
pub const IMAGE_TOKEN: &str = "[图片.img]";

/// Placeholder for image `id` (1-based). Id 1 uses [`IMAGE_TOKEN`]; others use `[图片.N.img]`.
pub fn image_token(id: u32) -> String {
    if id <= 1 {
        IMAGE_TOKEN.to_string()
    } else {
        format!("[图片.{id}.img]")
    }
}

/// Scan `text` for `[图片.img]` / `[图片.N.img]` tokens; return ids in left-to-right order.
pub fn image_token_ids_in(text: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if let Some((id, len)) = parse_image_token_at(rest) {
            ids.push(id);
            i += len;
            continue;
        }
        // Advance one Unicode scalar to stay on char boundaries.
        match rest.chars().next() {
            Some(ch) => i += ch.len_utf8(),
            None => break,
        }
    }
    ids
}

/// If `s` **ends** with an image token, return `(id, token_byte_len)`.
pub fn ends_with_image_token(s: &str) -> Option<(u32, usize)> {
    let pos = s.rfind('[')?;
    let (id, len) = parse_image_token_at(&s[pos..])?;
    if pos + len == s.len() {
        Some((id, len))
    } else {
        None
    }
}

/// Bytes to remove from the end of `s` for one atomic Backspace on an image token.
///
/// Treats optional single trailing space after the token as part of the unit, and
/// also drops one leading space before the token so `hello [图片.img] ` → `hello`.
pub fn image_token_backspace_len(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let (core, trail) = if let Some(core) = s.strip_suffix(' ') {
        (core, 1usize)
    } else {
        (s, 0usize)
    };
    let (_id, tok_len) = ends_with_image_token(core)?;
    let mut remove = tok_len + trail;
    let before = core.len() - tok_len;
    if before > 0 && core.as_bytes()[before - 1] == b' ' {
        remove += 1;
    }
    Some(remove)
}

/// If `s` starts with an image token, return `(id, matched_byte_len)`.
/// Prefer numbered form `[图片.N.img]` over bare `[图片.img]`.
pub fn parse_image_token_at(s: &str) -> Option<(u32, usize)> {
    let prefix = "[图片.";
    let suffix = ".img]";
    if let Some(after) = s.strip_prefix(prefix) {
        if let Some(end) = after.find(suffix) {
            let num = &after[..end];
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(id) = num.parse::<u32>() {
                    if id >= 1 {
                        let len = prefix.len() + num.len() + suffix.len();
                        return Some((id, len));
                    }
                }
            }
        }
    }
    // Bare `[图片.img]` → id 1
    if s.starts_with(IMAGE_TOKEN) {
        return Some((1, IMAGE_TOKEN.len()));
    }
    None
}

/// Remove all image tokens from text; collapse leftover whitespace runs (keep newlines).
pub fn strip_image_tokens(text: &str) -> String {
    strip_chips(text, ChipKind::Image)
}

// ── Long-text paste chips (`[文本.txt]` / `[文本.N.txt]`) ───────────────────

/// Prompt placeholder for long pasted text id 1.
pub const TEXT_TOKEN: &str = "[文本.txt]";

/// Collapse paste into a chip when it exceeds this many chars **or** lines.
pub const LONG_PASTE_CHAR_THRESHOLD: usize = 120;
pub const LONG_PASTE_LINE_THRESHOLD: usize = 4;

/// Placeholder for text paste `id` (1-based).
pub fn text_token(id: u32) -> String {
    if id <= 1 {
        TEXT_TOKEN.to_string()
    } else {
        format!("[文本.{id}.txt]")
    }
}

/// Whether paste body should become a `[文本.txt]` chip instead of raw input.
pub fn should_collapse_paste(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    // Image path / data-URI handled separately by callers.
    if parse_data_uri(t).is_some() || try_load_image_path_paste(t).is_some() {
        return false;
    }
    let chars = t.chars().count();
    let lines = t.lines().count();
    chars >= LONG_PASTE_CHAR_THRESHOLD
        || lines >= LONG_PASTE_LINE_THRESHOLD
        || (lines >= 2 && chars >= 80)
}

/// Short label for notices / tooltips, e.g. `12 lines · 3.4KB`.
pub fn text_blob_summary(body: &str) -> String {
    let lines = body
        .lines()
        .count()
        .max(if body.is_empty() { 0 } else { 1 });
    let bytes = body.len();
    let size = if bytes < 1024 {
        format!("{bytes}B")
    } else {
        format!("{}KB", (bytes + 512) / 1024)
    };
    format!("{lines} lines · {size}")
}

pub fn text_token_ids_in(text: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if let Some((id, len)) = parse_text_token_at(rest) {
            ids.push(id);
            i += len;
            continue;
        }
        match rest.chars().next() {
            Some(ch) => i += ch.len_utf8(),
            None => break,
        }
    }
    ids
}

pub fn ends_with_text_token(s: &str) -> Option<(u32, usize)> {
    let pos = s.rfind('[')?;
    let (id, len) = parse_text_token_at(&s[pos..])?;
    if pos + len == s.len() {
        Some((id, len))
    } else {
        None
    }
}

pub fn text_token_backspace_len(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let (core, trail) = if let Some(core) = s.strip_suffix(' ') {
        (core, 1usize)
    } else {
        (s, 0usize)
    };
    let (_id, tok_len) = ends_with_text_token(core)?;
    let mut remove = tok_len + trail;
    let before = core.len() - tok_len;
    if before > 0 && core.as_bytes()[before - 1] == b' ' {
        remove += 1;
    }
    Some(remove)
}

pub fn parse_text_token_at(s: &str) -> Option<(u32, usize)> {
    let prefix = "[文本.";
    let suffix = ".txt]";
    if let Some(after) = s.strip_prefix(prefix) {
        if let Some(end) = after.find(suffix) {
            let num = &after[..end];
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(id) = num.parse::<u32>() {
                    if id >= 1 {
                        let len = prefix.len() + num.len() + suffix.len();
                        return Some((id, len));
                    }
                }
            }
        }
    }
    if s.starts_with(TEXT_TOKEN) {
        return Some((1, TEXT_TOKEN.len()));
    }
    None
}

pub fn strip_text_tokens(text: &str) -> String {
    strip_chips(text, ChipKind::Text)
}

/// Backspace length for either image or text paste chip at end of input.
pub fn paste_chip_backspace_len(s: &str) -> Option<usize> {
    // Prefer longer match if both somehow apply (they never share suffix).
    match (image_token_backspace_len(s), text_token_backspace_len(s)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[derive(Clone, Copy)]
enum ChipKind {
    Image,
    Text,
}

fn strip_chips(text: &str, kind: ChipKind) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let hit = match kind {
            ChipKind::Image => parse_image_token_at(&text[i..]),
            ChipKind::Text => parse_text_token_at(&text[i..]),
        };
        if let Some((_id, len)) = hit {
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    collapse_ws(&out)
}

fn collapse_ws(out: &str) -> String {
    let mut cleaned = String::with_capacity(out.len());
    let mut prev_space = false;
    for ch in out.chars() {
        if ch == '\n' {
            cleaned.push(ch);
            prev_space = false;
        } else if ch.is_whitespace() {
            if !prev_space {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(ch);
            prev_space = false;
        }
    }
    cleaned.trim().to_string()
}

/// Build agent-facing prompt text:
/// - `[文本.N.txt]` → full pasted body
/// - `[图片.N.img]` → removed (images travel as multimodal blocks)
/// - other text kept as-is
pub fn materialize_prompt_text(
    text: &str,
    text_bodies: &std::collections::HashMap<u32, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((id, len)) = parse_text_token_at(&text[i..]) {
            if let Some(body) = text_bodies.get(&id) {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
            }
            i += len;
            continue;
        }
        if let Some((_id, len)) = parse_image_token_at(&text[i..]) {
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out.trim().to_string()
}

/// Encode raw bytes as standard base64 (no data-URI prefix).
pub fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode standard base64 (strips optional whitespace).
pub fn decode_base64(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(cleaned)
}

/// Guess MIME from file extension (lowercase).
pub fn mime_from_path(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// Sniff common image magic bytes.
pub fn mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    None
}

/// True when path extension looks like a raster image we support.
pub fn is_image_path(path: &Path) -> bool {
    mime_from_path(path).is_some()
}

/// Approximate decoded byte length from base64 length.
pub fn approx_decoded_len(b64: &str) -> usize {
    b64.len().saturating_mul(3) / 4
}

/// Compact UI / transcript label from byte length, e.g. `[image · png · 12KB]`.
pub fn image_label_bytes(mime_type: &str, bytes: usize) -> String {
    let size = if bytes < 1024 {
        format!("{bytes}B")
    } else {
        format!("{}KB", (bytes + 512) / 1024)
    };
    let short = mime_type.strip_prefix("image/").unwrap_or(mime_type);
    format!("[image · {short} · {size}]")
}

/// Compact UI / transcript label from base64 payload length.
pub fn image_label(mime_type: &str, data_b64: &str) -> String {
    image_label_bytes(mime_type, approx_decoded_len(data_b64))
}

/// Label for a local image file (uses on-disk size when available).
pub fn image_label_path(mime_type: &str, path: &Path) -> String {
    let bytes = std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    image_label_bytes(mime_type, bytes)
}

/// Parse a `data:image/...;base64,...` URI into (mime, raw base64 payload).
pub fn parse_data_uri(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let rest = s.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.contains(";base64") {
        return None;
    }
    let mime = meta
        .split(';')
        .next()
        .unwrap_or("image/png")
        .trim()
        .to_string();
    if !mime.starts_with("image/") {
        return None;
    }
    let data = data.trim();
    if data.is_empty() {
        return None;
    }
    // Validate base64.
    decode_base64(data).ok()?;
    Some((mime, data.to_string()))
}

/// Load an image file into (mime, base64). Errors as string for tool/TUI use.
pub fn load_image_file(path: &Path) -> Result<(String, String), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read image: {e}"))?;
    if bytes.is_empty() {
        return Err("image file is empty".into());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large ({} bytes > {} max)",
            bytes.len(),
            MAX_IMAGE_BYTES
        ));
    }
    let mime = mime_from_bytes(&bytes)
        .or_else(|| mime_from_path(path))
        .ok_or_else(|| "not a supported image (png/jpeg/gif/webp/bmp)".to_string())?
        .to_string();
    Ok((mime, encode_base64(&bytes)))
}

/// If `text` is a single existing image path (quoted or bare), import into media store.
///
/// Returns `(mime, media_path, original_name)`.
pub fn try_load_image_path_paste(text: &str) -> Option<(String, PathBuf, String)> {
    let t = text.trim().trim_matches(|c| c == '"' || c == '\'');
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    // Reject obvious non-paths.
    if t.starts_with("http://") || t.starts_with("https://") {
        return None;
    }
    let path = Path::new(t);
    if !is_image_path(path) {
        return None;
    }
    if !path.is_file() {
        return None;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image")
        .to_string();
    let (media, mime) = import_image_file(path).ok()?;
    Some((mime, media, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1×1 PNG
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn sniff_png() {
        let bytes = decode_base64(TINY_PNG_B64).unwrap();
        assert_eq!(mime_from_bytes(&bytes), Some("image/png"));
    }

    #[test]
    fn data_uri_roundtrip() {
        let uri = format!("data:image/png;base64,{TINY_PNG_B64}");
        let (mime, data) = parse_data_uri(&uri).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(data, TINY_PNG_B64);
    }

    #[test]
    fn label_format() {
        let l = image_label("image/png", TINY_PNG_B64);
        assert!(l.contains("png"), "{l}");
        assert!(l.starts_with("[image"), "{l}");
    }

    #[test]
    fn image_token_parse_and_strip() {
        assert_eq!(image_token(1), "[图片.img]");
        assert_eq!(image_token(2), "[图片.2.img]");
        let text = "看 [图片.img] 和 [图片.2.img] 对比";
        assert_eq!(image_token_ids_in(text), vec![1, 2]);
        assert_eq!(strip_image_tokens(text), "看 和 对比");
    }

    #[test]
    fn image_token_backspace_is_atomic() {
        let s = "hello [图片.img] ";
        let n = image_token_backspace_len(s).unwrap();
        assert_eq!(&s[..s.len() - n], "hello");

        let s2 = "x[图片.2.img]";
        let n2 = image_token_backspace_len(s2).unwrap();
        assert_eq!(&s2[..s2.len() - n2], "x");

        assert!(image_token_backspace_len("hello").is_none());
    }

    #[test]
    fn long_paste_collapses_and_materializes() {
        let long = "a\n".repeat(20);
        assert!(should_collapse_paste(&long));
        assert!(!should_collapse_paste("short"));

        let mut bodies = std::collections::HashMap::new();
        bodies.insert(1, "LINE1\nLINE2".into());
        let prompt = materialize_prompt_text("see [文本.txt] please [图片.img]", &bodies);
        assert!(prompt.contains("LINE1"));
        assert!(prompt.contains("please"));
        assert!(!prompt.contains("文本"));
        assert!(!prompt.contains("图片"));
    }

    #[test]
    fn text_token_backspace_atomic() {
        let s = "hi [文本.txt] ";
        let n = text_token_backspace_len(s).unwrap();
        assert_eq!(&s[..s.len() - n], "hi");
        assert_eq!(paste_chip_backspace_len(s), Some(n));
    }
}
