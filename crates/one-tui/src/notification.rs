//! Terminal desktop notifications & chime protocol handling (Grok Build style).
//!
//! Protocols:
//! - **OSC 9** (iTerm2, WezTerm, Warp): `\x1b]9;{message}\x07`
//! - **OSC 99** (Kitty): `\x1b]99;i=one;{message}\x1b\`
//! - **OSC 777** (Ghostty, VTE/GNOME Terminal): `\x1b]777;notify;{title};{body}\x1b\`
//! - **BEL** (Fallback): `\x07` (ASCII Bell chime)
//! - **None**: Notifications disabled

use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotificationProtocol {
    /// iTerm2, WezTerm, Warp: `\x1b]9;{message}\x07`
    Osc9,
    /// Kitty: `\x1b]99;i=one;{message}\x1b\`
    Osc99,
    /// Ghostty, VTE (GNOME Terminal, Tilix): `\x1b]777;notify;{title};{body}\x1b\`
    Osc777,
    /// Universal fallback: `\x07` (ASCII Bell chime)
    #[default]
    Bel,
    /// Notifications disabled
    None,
}

impl NotificationProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Osc9 => "osc9",
            Self::Osc99 => "osc99",
            Self::Osc777 => "osc777",
            Self::Bel => "bel",
            Self::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "osc9" | "iterm" | "wezterm" | "warp" => Some(Self::Osc9),
            "osc99" | "kitty" => Some(Self::Osc99),
            "osc777" | "ghostty" | "vte" => Some(Self::Osc777),
            "bel" | "bell" | "chime" => Some(Self::Bel),
            "none" | "off" | "disabled" | "0" | "false" => Some(Self::None),
            _ => None,
        }
    }
}

/// Detect the best notification protocol for the current terminal environment.
pub fn detect_notification_protocol() -> NotificationProtocol {
    // Check manual override environment variables first
    if let Ok(val) = std::env::var("ONE_NOTIFY_PROTOCOL") {
        if let Some(proto) = NotificationProtocol::parse(&val) {
            return proto;
        }
    }

    if let Ok(val) = std::env::var("ONE_NOTIFY") {
        if val == "0" || val.eq_ignore_ascii_case("false") || val.eq_ignore_ascii_case("off") {
            return NotificationProtocol::None;
        }
    }

    if let Ok(val) = std::env::var("ONE_BELL") {
        if val == "0" || val.eq_ignore_ascii_case("false") || val.eq_ignore_ascii_case("off") {
            return NotificationProtocol::None;
        }
    }

    // Check Kitty environment variables
    if std::env::var_os("KITTY_PID").is_some() || std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return NotificationProtocol::Osc99;
    }

    // Check Ghostty environment variables
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
        return NotificationProtocol::Osc777;
    }

    // Check VTE (GNOME Terminal, Tilix, etc.)
    if std::env::var_os("VTE_VERSION").is_some() {
        return NotificationProtocol::Osc777;
    }

    // Check TERM_PROGRAM
    if let Ok(term_prog) = std::env::var("TERM_PROGRAM") {
        let prog = term_prog.trim().to_ascii_lowercase();
        if prog.contains("iterm") || prog.contains("wezterm") || prog.contains("warp") {
            return NotificationProtocol::Osc9;
        }
        if prog.contains("ghostty") {
            return NotificationProtocol::Osc777;
        }
        if prog.contains("kitty") {
            return NotificationProtocol::Osc99;
        }
        if prog.contains("vscode") {
            return NotificationProtocol::Osc777;
        }
    }

    // Check TERM
    if let Ok(term) = std::env::var("TERM") {
        let t = term.trim().to_ascii_lowercase();
        if t.contains("xterm-kitty") {
            return NotificationProtocol::Osc99;
        }
        if t.contains("ghostty") {
            return NotificationProtocol::Osc777;
        }
        if t.contains("wezterm") || t.contains("iterm") {
            return NotificationProtocol::Osc9;
        }
    }

    // Default fallback: BEL chime
    NotificationProtocol::Bel
}

/// Format the terminal escape sequence for a notification.
pub fn format_notification_sequence(
    protocol: NotificationProtocol,
    title: &str,
    body: &str,
) -> Option<String> {
    let title_clean = sanitize(title);
    let body_clean = sanitize(body);

    match protocol {
        NotificationProtocol::Osc9 => {
            let msg = if title_clean.is_empty() {
                body_clean
            } else if body_clean.is_empty() {
                title_clean
            } else {
                format!("{title_clean}: {body_clean}")
            };
            Some(format!("\x1b]9;{}\x07", msg))
        }
        NotificationProtocol::Osc99 => {
            let msg = if title_clean.is_empty() {
                body_clean
            } else if body_clean.is_empty() {
                title_clean
            } else {
                format!("{title_clean}: {body_clean}")
            };
            Some(format!("\x1b]99;i=one;{}\x1b\\", msg))
        }
        NotificationProtocol::Osc777 => {
            Some(format!("\x1b]777;notify;{};{}\x1b\\", title_clean, body_clean))
        }
        NotificationProtocol::Bel => Some("\x07".to_string()),
        NotificationProtocol::None => None,
    }
}

/// Send a terminal notification directly to standard output.
pub fn send_notification(protocol: NotificationProtocol, title: &str, body: &str) -> io::Result<()> {
    if let Some(seq) = format_notification_sequence(protocol, title, body) {
        let mut stdout = io::stdout().lock();
        stdout.write_all(seq.as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
}

/// Ring the terminal bell (ASCII BEL).
pub fn ring_bell() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x07")?;
    stdout.flush()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && *c != ';' && *c != '\x1b')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_sequences() {
        assert_eq!(
            format_notification_sequence(NotificationProtocol::Osc9, "One", "Task complete"),
            Some("\x1b]9;One: Task complete\x07".into())
        );
        assert_eq!(
            format_notification_sequence(NotificationProtocol::Osc99, "One", "Task complete"),
            Some("\x1b]99;i=one;One: Task complete\x1b\\".into())
        );
        assert_eq!(
            format_notification_sequence(NotificationProtocol::Osc777, "One", "Task complete"),
            Some("\x1b]777;notify;One;Task complete\x1b\\".into())
        );
        assert_eq!(
            format_notification_sequence(NotificationProtocol::Bel, "One", "Task complete"),
            Some("\x07".into())
        );
        assert_eq!(
            format_notification_sequence(NotificationProtocol::None, "One", "Task complete"),
            None
        );
    }

    #[test]
    fn test_sanitization() {
        let raw = "Title;\x1b[31mwith\nnewlines;and;semicolons";
        let sanitized = sanitize(raw);
        assert!(!sanitized.contains(';'));
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\x1b'));
    }

    #[test]
    fn test_protocol_parse() {
        assert_eq!(NotificationProtocol::parse("osc9"), Some(NotificationProtocol::Osc9));
        assert_eq!(NotificationProtocol::parse("kitty"), Some(NotificationProtocol::Osc99));
        assert_eq!(NotificationProtocol::parse("ghostty"), Some(NotificationProtocol::Osc777));
        assert_eq!(NotificationProtocol::parse("bel"), Some(NotificationProtocol::Bel));
        assert_eq!(NotificationProtocol::parse("0"), Some(NotificationProtocol::None));
        assert_eq!(NotificationProtocol::parse("unknown_xyz"), None);
    }
}
