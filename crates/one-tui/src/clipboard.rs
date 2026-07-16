//! System clipboard helpers.
//!
//! ## Copy (text → host clipboard)
//! - **WSL → Windows**: `clip.exe` with **UTF-16LE + BOM**
//!   (raw UTF-8 into `clip.exe` is the classic 中文乱码 bug)
//! - **OSC 52**: UTF-8 base64 (SSH / native terminals)
//! - **Unix**: `wl-copy` / `xclip` / `xsel` / `pbcopy` (UTF-8)
//!
//! ## Paste (host clipboard → image bytes)
//! Terminals only deliver **text** via bracketed paste. Screenshot / browser
//! copy leaves a **bitmap** on the host clipboard; Codex-style recovery:
//! - **WSL**: PowerShell `Get-Clipboard -Format Image` → temp PNG → WSL path
//! - **Wayland**: `wl-paste --type image/png` (and jpeg/bmp)
//! - **X11**: `xclip -selection clipboard -t image/png -o`
//! - **macOS**: `pngpaste -` / `osascript` TIFF→PNG via `sips`

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use one_core::image::{mime_from_bytes, store_image_bytes, MAX_IMAGE_BYTES};

/// Copy `text` to the system clipboard. Returns `Ok` if any backend succeeds.
pub fn copy_text(w: &mut impl Write, text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("empty".into());
    }

    let mut errors = Vec::new();
    let mut ok = false;

    // 1) WSL → Windows clipboard (what Ctrl+V uses). UTF-16LE required.
    if is_wsl() {
        match copy_windows_clip(text) {
            Ok(()) => ok = true,
            Err(e) => errors.push(e),
        }
    }

    // 2) OSC 52 (UTF-8). Skip on WSL if clip.exe already won — dual writes race.
    if !ok || !is_wsl() {
        match write_osc52(w, text) {
            Ok(()) => ok = true,
            Err(e) => errors.push(format!("osc52: {e}")),
        }
    }

    // 3) Unix tools (UTF-8).
    if !ok {
        match copy_via_unix_command(text) {
            Ok(()) => ok = true,
            Err(e) => errors.push(e),
        }
    }

    if ok {
        Ok(())
    } else {
        Err(if errors.is_empty() {
            "no clipboard backend".into()
        } else {
            errors.join("; ")
        })
    }
}

/// OSC 52 (UTF-8 payload, base64-encoded).
pub fn write_osc52(w: &mut impl Write, text: &str) -> io::Result<()> {
    let b64 = base64_std(text.as_bytes());
    if std::env::var_os("TMUX").is_some() {
        write!(w, "\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\")?;
    } else {
        write!(w, "\x1b]52;c;{b64}\x07")?;
    }
    w.flush()
}

/// Windows `clip.exe` needs UTF-16LE (+ BOM). UTF-8 → 乱码.
fn copy_windows_clip(text: &str) -> Result<(), String> {
    let bytes = utf16le_bom(text);
    if pipe_bytes(&["clip.exe"], &bytes).is_ok() {
        return Ok(());
    }
    copy_windows_powershell(text)
}

fn utf16le_bom(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.push(0xFF);
    out.push(0xFE);
    for unit in text.encode_utf16() {
        out.push((unit & 0xFF) as u8);
        out.push((unit >> 8) as u8);
    }
    out
}

fn copy_windows_powershell(text: &str) -> Result<(), String> {
    // stdin UTF-8 → Set-Clipboard (Unicode-safe).
    let script = r#"
$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::UTF8
$in = [Console]::In.ReadToEnd()
Set-Clipboard -Value $in
"#;
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("powershell.exe: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("powershell write: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("powershell wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("powershell exit {}", status.code().unwrap_or(-1)))
    }
}

fn copy_via_unix_command(text: &str) -> Result<(), String> {
    if pipe_bytes(&["wl-copy"], text.as_bytes()).is_ok() {
        return Ok(());
    }
    if pipe_bytes(&["xclip", "-selection", "clipboard"], text.as_bytes()).is_ok() {
        return Ok(());
    }
    if pipe_bytes(&["xsel", "--clipboard", "--input"], text.as_bytes()).is_ok() {
        return Ok(());
    }
    if pipe_bytes(&["pbcopy"], text.as_bytes()).is_ok() {
        return Ok(());
    }
    if std::env::var_os("TMUX").is_some()
        && pipe_bytes(&["tmux", "load-buffer", "-"], text.as_bytes()).is_ok()
    {
        return Ok(());
    }
    Err("no host clipboard command".into())
}

pub(crate) fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let l = v.to_ascii_lowercase();
            l.contains("microsoft") || l.contains("wsl")
        })
        .unwrap_or(false)
}

// ── Image paste (bitmap from host clipboard) ────────────────────────────────

/// Result of a successful clipboard image paste: `(mime, media_path, name)`.
pub type PastedImage = (String, PathBuf, String);

/// Read a bitmap from the system/host clipboard (not terminal bracketed paste).
///
/// Mirrors Codex `clipboard_paste::paste_image_*`: dump to a file, then import
/// into `~/.one/agent/media` so session resume does not depend on `/tmp`.
pub fn paste_image() -> Result<PastedImage, String> {
    let mut errors = Vec::new();

    // 1) WSL → Windows clipboard (screenshots, browser copy-as-image).
    if is_wsl() {
        match paste_image_wsl_powershell() {
            Ok(v) => return Ok(v),
            Err(e) => errors.push(e),
        }
    }

    // 2) Wayland / X11 / macOS host tools.
    match paste_image_via_unix_tools() {
        Ok(v) => return Ok(v),
        Err(e) => errors.push(e),
    }

    Err(if errors.is_empty() {
        "no image on clipboard".into()
    } else {
        errors.join("; ")
    })
}

/// Convert `C:\Users\…\a.png` (or `C:/…`) to `/mnt/c/Users/…/a.png` under WSL.
pub fn windows_path_to_wsl(input: &str) -> Option<PathBuf> {
    let input = input.trim().trim_matches(|c| c == '"' || c == '\'');
    if input.starts_with(r"\\") {
        return None; // UNC not mapped
    }
    let mut chars = input.chars();
    let drive = chars.next()?.to_ascii_lowercase();
    if !drive.is_ascii_lowercase() {
        return None;
    }
    if chars.next() != Some(':') {
        return None;
    }
    let rest = input.get(2..)?;
    if !rest.starts_with(['\\', '/']) {
        // Allow `C:foo` rarely; still require separator for safety.
        if rest.is_empty() {
            return Some(PathBuf::from(format!("/mnt/{drive}")));
        }
        return None;
    }
    let mut result = PathBuf::from(format!("/mnt/{drive}"));
    for component in rest
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/'])
        .filter(|c| !c.is_empty())
    {
        result.push(component);
    }
    Some(result)
}

/// Normalize a pasted path: strip quotes, `file://`, Windows→WSL under WSL.
pub fn normalize_pasted_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();
    let unquoted = pasted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            pasted
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(pasted)
        .trim();
    if unquoted.is_empty() || unquoted.contains('\n') {
        return None;
    }

    if let Some(rest) = unquoted.strip_prefix("file://") {
        // file:///tmp/x.png or file://localhost/tmp/x.png
        let path = rest
            .strip_prefix("localhost")
            .unwrap_or(rest);
        let path = if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            PathBuf::from(format!("/{path}"))
        };
        return Some(path);
    }

    if is_wsl() {
        if let Some(wsl) = windows_path_to_wsl(unquoted) {
            return Some(wsl);
        }
    }

    Some(PathBuf::from(unquoted))
}

fn paste_image_wsl_powershell() -> Result<PastedImage, String> {
    // Codex: dump Windows clipboard image to a temp PNG, print the Windows path.
    let script = r#"[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $img = Get-Clipboard -Format Image; if ($img -ne $null) { $p=[System.IO.Path]::GetTempFileName(); $p = [System.IO.Path]::ChangeExtension($p,'png'); $img.Save($p,[System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p } else { exit 1 }"#;

    let mut last_err = "powershell: no image".to_string();
    for cmd in ["powershell.exe", "pwsh.exe", "pwsh", "powershell"] {
        let output = match Command::new(cmd)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                last_err = format!("{cmd}: {e}");
                continue;
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            last_err = format!(
                "{cmd} exit {} {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            );
            continue;
        }
        let win_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if win_path.is_empty() {
            last_err = format!("{cmd}: empty path");
            continue;
        }
        let path = windows_path_to_wsl(&win_path)
            .ok_or_else(|| format!("cannot map Windows path: {win_path}"))?;
        return load_pasted_image_file(&path, "clipboard.png");
    }
    Err(last_err)
}

fn paste_image_via_unix_tools() -> Result<PastedImage, String> {
    // Prefer PNG; fall back to other raster MIME types common on clipboards.
    const TARGETS: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/bmp",
        "image/gif",
        "image/webp",
    ];

    for mime in TARGETS {
        // wl-paste --type image/png
        if let Ok(bytes) = capture_stdout(&["wl-paste", "--type", mime], &[]) {
            if let Ok(v) = image_bytes_to_pasted(&bytes, "clipboard") {
                return Ok(v);
            }
        }
        if let Ok(bytes) = capture_stdout(&["wl-paste", "-t", mime], &[]) {
            if let Ok(v) = image_bytes_to_pasted(&bytes, "clipboard") {
                return Ok(v);
            }
        }
        // xclip -selection clipboard -t image/png -o
        if let Ok(bytes) = capture_stdout(
            &["xclip", "-selection", "clipboard", "-t", mime, "-o"],
            &[],
        ) {
            if let Ok(v) = image_bytes_to_pasted(&bytes, "clipboard") {
                return Ok(v);
            }
        }
    }

    // macOS: pngpaste writes PNG to stdout when given `-`
    if let Ok(bytes) = capture_stdout(&["pngpaste", "-"], &[]) {
        if let Ok(v) = image_bytes_to_pasted(&bytes, "clipboard.png") {
            return Ok(v);
        }
    }

    Err("no host image clipboard tool (wl-paste/xclip/pngpaste)".into())
}

fn load_pasted_image_file(path: &Path, fallback_name: &str) -> Result<PastedImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback_name)
        .to_string();
    image_bytes_to_pasted(&bytes, &name)
}

fn image_bytes_to_pasted(bytes: &[u8], name: &str) -> Result<PastedImage, String> {
    if bytes.is_empty() {
        return Err("empty image".into());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large ({} bytes > {} max)",
            bytes.len(),
            MAX_IMAGE_BYTES
        ));
    }
    // Sniff first so we don't write garbage into the media store.
    let _ = mime_from_bytes(bytes)
        .ok_or_else(|| "clipboard data is not a supported image".to_string())?;
    let (path, mime) = store_image_bytes(bytes, None)?;
    let name = if Path::new(name).extension().is_some() {
        name.to_string()
    } else {
        let ext = mime.strip_prefix("image/").unwrap_or("png");
        format!("{name}.{ext}")
    };
    Ok((mime, path, name))
}

fn capture_stdout(cmd: &[&str], stdin_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (bin, args) = cmd.split_first().ok_or_else(|| "empty cmd".to_string())?;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(if stdin_bytes.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    if !stdin_bytes.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(stdin_bytes)
                .map_err(|e| format!("{bin} write: {e}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{bin} wait: {e}"))?;
    if !output.status.success() {
        return Err(format!("{bin} exit {}", output.status.code().unwrap_or(-1)));
    }
    if output.stdout.is_empty() {
        return Err(format!("{bin}: empty stdout"));
    }
    Ok(output.stdout)
}

fn pipe_bytes(cmd: &[&str], bytes: &[u8]) -> Result<(), String> {
    let (bin, args) = cmd.split_first().ok_or_else(|| "empty cmd".to_string())?;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(bytes)
            .map_err(|e| format!("{bin} write: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("{bin} wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{bin} exit {}", status.code().unwrap_or(-1)))
    }
}

fn base64_std(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    match input.len() - i {
        1 => {
            let n = (input[i] as u32) << 16;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_hello() {
        assert_eq!(base64_std(b"hello"), "aGVsbG8=");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
    }

    #[test]
    fn utf16le_bom_chinese() {
        let bytes = utf16le_bom("你好");
        assert_eq!(&bytes[0..2], &[0xFF, 0xFE]);
        // 你 U+4F60 → LE 60 4F
        assert_eq!(&bytes[2..4], &[0x60, 0x4F]);
        // 好 U+597D → LE 7D 59
        assert_eq!(&bytes[4..6], &[0x7D, 0x59]);
    }

    #[test]
    fn osc52_writes_sequence() {
        let mut buf = Vec::new();
        let old = std::env::var_os("TMUX");
        std::env::remove_var("TMUX");
        write_osc52(&mut buf, "hi").unwrap();
        if let Some(v) = old {
            std::env::set_var("TMUX", v);
        }
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("]52;c;"), "{s:?}");
        assert!(s.contains("aGk="), "{s:?}");
    }

    #[test]
    fn copy_empty_err() {
        let mut sink = Vec::new();
        assert!(copy_text(&mut sink, "").is_err());
    }

    #[test]
    fn windows_path_maps_to_mnt() {
        let p = windows_path_to_wsl(r"C:\Users\me\shot.png").unwrap();
        assert_eq!(p, PathBuf::from("/mnt/c/Users/me/shot.png"));
        let p2 = windows_path_to_wsl("D:/tmp/a.jpeg").unwrap();
        assert_eq!(p2, PathBuf::from("/mnt/d/tmp/a.jpeg"));
        assert!(windows_path_to_wsl("/home/me/a.png").is_none());
        assert!(windows_path_to_wsl(r"\\server\share\a.png").is_none());
    }

    #[test]
    fn normalize_file_url() {
        let p = normalize_pasted_path("file:///tmp/example.png").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/example.png"));
    }

    #[test]
    fn image_bytes_png_ok() {
        // 1×1 PNG
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let bytes = one_core::image::decode_base64(b64).unwrap();
        let (mime, path, name) = image_bytes_to_pasted(&bytes, "clipboard").unwrap();
        assert_eq!(mime, "image/png");
        assert!(path.is_file(), "{path:?}");
        assert_eq!(name, "clipboard.png");
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, bytes);
    }
}
