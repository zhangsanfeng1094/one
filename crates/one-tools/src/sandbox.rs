//! Bash command hard-blocks and confirmation heuristics.
//!
//! Matching is intentionally stricter than raw `contains` on the whole line:
//! - collapse whitespace
//! - treat common shell separators (`;`, `&&`, `||`, `|`, newlines) as splits
//! - match high-risk *command shapes* rather than free-text comments when possible
//!
//! This is still not a shell parser. Prefer OS sandbox (bwrap) for real isolation.

/// Hard-blocked patterns: irreversible / catastrophic host damage.
///
/// Intentionally does **not** block `curl`/`wget` — coding agents and skills
/// (e.g. Agent Skills web search helpers) need network commands. Prefer
/// confirmation for risky ops instead.
const BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -rf -- /",
    "rm -rf --no-preserve-root /",
    "mkfs.",
    ":(){ :|:& };:",
    "> /dev/sd",
    "dd if=/dev/zero of=/dev/",
    "dd if=/dev/random of=/dev/",
    "dd if=/dev/urandom of=/dev/",
];

/// Requires `--yes` / `ONE_AUTO_APPROVE=1` when auto_approve is false.
///
/// Patterns are matched against **normalized command segments** (not arbitrary
/// substrings mid-token), reducing false positives from comments/strings.
const CONFIRM_COMMAND_PREFIXES: &[&str] = &[
    "sudo ",
    "sudo\t",
    "doas ",
    "rm -rf",
    "rm -fr",
    "rm -r ",
    "rm -r\t",
    "git push",
    "git reset --hard",
    "git clean -fd",
    "git clean -f",
    "chmod ",
    "chown ",
    "kill -9",
    "kill -kill",
    "mkfs",
    "dd if=",
    "shutdown",
    "reboot",
    "userdel ",
    "passwd ",
];

const CONFIRM_REDIRECT_PATTERNS: &[&str] = &["> /etc/", ">/etc/", "> /dev/sd", ">/dev/sd"];

pub fn is_command_blocked(command: &str) -> Option<&'static str> {
    let normalized = normalize_command(command);
    for pattern in BLOCKED_PATTERNS {
        if normalized.contains(pattern) {
            return Some(pattern);
        }
    }
    // Variants like `rm -rf/*` or `rm  -rf  /` after normalize.
    if looks_like_rm_root(&normalized) {
        return Some("rm -rf /");
    }
    None
}

pub fn requires_confirmation(command: &str) -> Option<&'static str> {
    if is_command_blocked(command).is_some() {
        // Blocked commands never reach confirm; keep API simple.
    }
    let normalized = normalize_command(command);
    for pattern in CONFIRM_REDIRECT_PATTERNS {
        if normalized.contains(pattern) {
            return Some(pattern);
        }
    }
    for segment in shell_segments(&normalized) {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        // Skip pure comments in a segment.
        if seg.starts_with('#') {
            continue;
        }
        for pattern in CONFIRM_COMMAND_PREFIXES {
            if segment_matches_risk(seg, pattern) {
                return Some(pattern.trim());
            }
        }
    }
    None
}

/// Lowercase + collapse whitespace so spacing tricks do not dodge checks.
fn normalize_command(command: &str) -> String {
    let lower = command.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_space = false;
    for ch in lower.chars() {
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Split on common shell list / pipe operators for per-segment matching.
fn shell_segments(normalized: &str) -> Vec<&str> {
    // Split on ; && || | and newlines (already spaces from normalize for \n).
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = normalized.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let two = if i + 1 < bytes.len() {
            &normalized[i..i + 2]
        } else {
            ""
        };
        if two == "&&" || two == "||" {
            parts.push(&normalized[start..i]);
            i += 2;
            start = i;
            continue;
        }
        let c = bytes[i] as char;
        if c == ';' || c == '|' || c == '\n' {
            parts.push(&normalized[start..i]);
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(&normalized[start..]);
    parts
}

fn segment_matches_risk(segment: &str, pattern: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    // Command at start of segment, or after env assignments (`FOO=1 sudo …`).
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    // Skip leading VAR=value.
    let mut idx = 0;
    while idx < tokens.len() && tokens[idx].contains('=') && !tokens[idx].starts_with('-') {
        idx += 1;
    }
    if idx >= tokens.len() {
        return false;
    }
    let rest = tokens[idx..].join(" ");
    if rest.starts_with(pat) {
        return true;
    }
    // Also allow pattern as whole-token prefix of first real command (e.g. `mkfs.ext4`).
    if !pat.ends_with(' ') && tokens[idx].starts_with(pat) {
        return true;
    }
    false
}

fn looks_like_rm_root(normalized: &str) -> bool {
    // rm … -r/-rf … / or /*
    for segment in shell_segments(normalized) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if tokens.is_empty() || tokens[0] != "rm" {
            continue;
        }
        let mut recursive = false;
        let mut target_root = false;
        for t in &tokens[1..] {
            if *t == "-rf" || *t == "-fr" || *t == "-r" || *t == "--recursive" {
                recursive = true;
            }
            if *t == "/" || *t == "/*" || *t == "--no-preserve-root" {
                target_root = true;
            }
            // Combined flags: -rf already handled; -rR etc.
            if t.starts_with('-') && !t.starts_with("--") {
                if t.contains('r') {
                    recursive = true;
                }
            }
        }
        if recursive && target_root {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_curl_and_wget() {
        assert!(is_command_blocked("curl https://example.com").is_none());
        assert!(is_command_blocked("wget https://example.com").is_none());
        assert!(requires_confirmation("curl https://example.com").is_none());
    }

    #[test]
    fn blocks_rm_root_variants() {
        assert!(is_command_blocked("rm -rf /").is_some());
        assert!(is_command_blocked("rm -rf /*").is_some());
        assert!(is_command_blocked("rm  -rf  /").is_some());
        assert!(is_command_blocked("rm -rf --no-preserve-root /").is_some());
    }

    #[test]
    fn confirm_git_push_not_in_echo_string() {
        // Still may match if `git push` is a real segment; echo alone is fine.
        assert!(requires_confirmation("echo 'do not git push yet'").is_none());
        assert!(requires_confirmation("git push origin main").is_some());
        assert!(requires_confirmation("FOO=1 git push").is_some());
        assert!(requires_confirmation("cd /tmp && git push").is_some());
    }

    #[test]
    fn confirm_sudo_and_skip_commentish() {
        assert!(requires_confirmation("sudo apt install x").is_some());
        assert!(requires_confirmation("# sudo apt install x").is_none());
    }

    #[test]
    fn allows_rm_project_path_without_root_block() {
        // Not hard-blocked (only /), but still needs confirm as rm -rf.
        assert!(is_command_blocked("rm -rf ./build").is_none());
        assert!(requires_confirmation("rm -rf ./build").is_some());
    }
}
