//! System clipboard helpers.
//!
//! - **WSL → Windows**: `clip.exe` with **UTF-16LE + BOM**  
//!   (raw UTF-8 into `clip.exe` is the classic 中文乱码 bug)
//! - **OSC 52**: UTF-8 base64 (SSH / native terminals)
//! - **Unix**: `wl-copy` / `xclip` / `xsel` / `pbcopy` (UTF-8)

use std::io::{self, Write};
use std::process::{Command, Stdio};

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
    let status = child
        .wait()
        .map_err(|e| format!("powershell wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "powershell exit {}",
            status.code().unwrap_or(-1)
        ))
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

fn is_wsl() -> bool {
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
}
