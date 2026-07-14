/// Hard-blocked patterns: irreversible / catastrophic host damage.
///
/// Intentionally does **not** block `curl`/`wget` — coding agents and skills
/// (e.g. Agent Skills web search helpers) need network commands. Prefer
/// confirmation for risky ops instead.
const BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "mkfs.",
    ":(){ :|:& };:",
    "> /dev/sd",
    "dd if=/dev/zero of=/dev/",
    "dd if=/dev/random of=/dev/",
];

/// Requires `--yes` / `ONE_AUTO_APPROVE=1` when auto_approve is false.
const CONFIRM_PATTERNS: &[&str] = &[
    "sudo ",
    "rm -rf",
    "git push",
    "git reset --hard",
    "chmod ",
    "chown ",
    "kill -9",
    "> /etc/",
    "mkfs",
    "dd if=",
];

pub fn is_command_blocked(command: &str) -> Option<&'static str> {
    let lower = command.to_lowercase();
    for pattern in BLOCKED_PATTERNS {
        if lower.contains(pattern) {
            return Some(pattern);
        }
    }
    None
}

pub fn requires_confirmation(command: &str) -> Option<&'static str> {
    let lower = command.to_lowercase();
    for pattern in CONFIRM_PATTERNS {
        if lower.contains(pattern) {
            return Some(pattern);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_curl_and_wget() {
        assert!(is_command_blocked("curl https://example.com").is_none());
        assert!(is_command_blocked("wget https://example.com").is_none());
    }

    #[test]
    fn blocks_rm_root() {
        assert!(is_command_blocked("rm -rf /").is_some());
    }
}
