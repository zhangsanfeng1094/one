//! Bash command hard-blocks and confirmation heuristics.
//!
//! Matching is intentionally stricter than raw `contains` on the whole line:
//! - collapse whitespace
//! - treat common shell separators (`;`, `&&`, `||`, `|`, newlines) as splits
//! - match high-risk *command shapes* rather than free-text comments when possible
//!
//! Two confirmation tiers:
//! - **Strict** ([`requires_strict_confirmation`]): data-loss / destructive shapes
//!   (git checkout/restore/reset/clean, force-push, recursive rm, …). Always Ask —
//!   even with `auto_approve` / `-y` / session Always.
//! - **Soft** ([`requires_confirmation`]): host-impact shapes (`sudo`, normal
//!   `git push`, …). Skipped when auto_approve is on.
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

/// Soft confirm: skipped when `auto_approve` / `--yes` is on.
///
/// Patterns are matched against **normalized command segments** (not arbitrary
/// substrings mid-token), reducing false positives from comments/strings.
const SOFT_CONFIRM_PREFIXES: &[&str] = &[
    "sudo ",
    "sudo\t",
    "doas ",
    "git push",
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

/// Reason prefix for strict (always-confirm) Ask verdicts.
///
/// [`crate::permissions`] and the CLI gate key off this string so auto_approve
/// cannot silently allow destructive commands.
pub const DESTRUCTIVE_REASON_PREFIX: &str = "destructive command";

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

/// True when an Ask reason is a strict destructive prompt (must not skip with auto).
pub fn is_destructive_ask_reason(reason: &str) -> bool {
    reason.starts_with(DESTRUCTIVE_REASON_PREFIX)
}

/// Data-loss / irreversible worktree or host shapes — **always** need confirmation.
///
/// Covers git checkout/restore/reset/clean, force-push, branch delete, recursive
/// rm, etc. Does not include soft risks like plain `git push` or `sudo`.
pub fn requires_strict_confirmation(command: &str) -> Option<&'static str> {
    if is_command_blocked(command).is_some() {
        // Blocked commands never reach confirm; keep API simple.
        return None;
    }
    let normalized = normalize_command(command);
    for segment in shell_segments(&normalized) {
        let seg = segment.trim();
        if seg.is_empty() || seg.starts_with('#') {
            continue;
        }
        if let Some(pat) = destructive_git_shape(seg) {
            return Some(pat);
        }
        if let Some(pat) = destructive_rm_shape(seg) {
            return Some(pat);
        }
    }
    None
}

/// Soft high-risk (sudo, normal git push, …). Callers skip this when auto_approve.
///
/// Also returns strict matches so a single call still catches everything when
/// auto_approve is false (strict is a subset that always applies).
pub fn requires_confirmation(command: &str) -> Option<&'static str> {
    if let Some(pat) = requires_strict_confirmation(command) {
        return Some(pat);
    }
    if is_command_blocked(command).is_some() {
        return None;
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
        if seg.starts_with('#') {
            continue;
        }
        // Force-push is strict; plain push is soft — avoid double soft match.
        if destructive_git_shape(seg).is_some() {
            continue;
        }
        for pattern in SOFT_CONFIRM_PREFIXES {
            if segment_matches_risk(seg, pattern) {
                return Some(pattern.trim());
            }
        }
    }
    None
}

/// Format the Ask reason for a strict destructive match.
pub fn destructive_ask_reason(pattern: &str) -> String {
    format!("{DESTRUCTIVE_REASON_PREFIX} `{pattern}` (always confirm)")
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
///
/// Operators are ASCII-only. Indices must stay on UTF-8 char boundaries so
/// multi-byte text in comments/strings (e.g. en-dash `–`) cannot panic on
/// `&str` slicing — a previous byte-step loop did.
fn shell_segments(normalized: &str) -> Vec<&str> {
    // Split on ; && || | and newlines (already spaces from normalize for \n).
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = normalized.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Compare bytes, not `&str[i..i+2]` — the latter panics mid multi-byte char.
        if i + 1 < bytes.len()
            && ((bytes[i] == b'&' && bytes[i + 1] == b'&')
                || (bytes[i] == b'|' && bytes[i + 1] == b'|'))
        {
            parts.push(&normalized[start..i]);
            i += 2;
            start = i;
            continue;
        }
        let b = bytes[i];
        if b == b';' || b == b'|' || b == b'\n' {
            parts.push(&normalized[start..i]);
            i += 1;
            start = i;
            continue;
        }
        // Advance one full UTF-8 character so `i` stays on a char boundary.
        i += utf8_char_width(b);
    }
    parts.push(&normalized[start..]);
    parts
}

/// Byte length of the UTF-8 character that starts at `first`.
fn utf8_char_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        // Invalid lead / continuation — skip one byte to avoid hanging.
        _ => 1,
    }
}

fn segment_tokens(segment: &str) -> Vec<&str> {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    if tokens.is_empty() {
        return tokens;
    }
    // Skip leading VAR=value.
    let mut idx = 0;
    while idx < tokens.len() && tokens[idx].contains('=') && !tokens[idx].starts_with('-') {
        idx += 1;
    }
    tokens[idx..].to_vec()
}

fn segment_matches_risk(segment: &str, pattern: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    let tokens = segment_tokens(segment);
    if tokens.is_empty() {
        return false;
    }
    let rest = tokens.join(" ");
    if rest.starts_with(pat) {
        return true;
    }
    // Also allow pattern as whole-token prefix of first real command (e.g. `mkfs.ext4`).
    if !pat.ends_with(' ') && tokens[0].starts_with(pat) {
        return true;
    }
    false
}

/// Skip `git -C path` / `git --git-dir=…` style global options; return subcommand index.
fn git_subcommand_index(tokens: &[&str]) -> Option<usize> {
    if tokens.first().copied() != Some("git") {
        return None;
    }
    let mut i = 1;
    while i < tokens.len() {
        let t = tokens[i];
        match t {
            "-C" | "-c" => {
                i += 2;
                continue;
            }
            // long opts with optional =value
            t if t.starts_with("--git-dir")
                || t.starts_with("--work-tree")
                || t.starts_with("--namespace")
                || t.starts_with("--config-env") =>
            {
                if !t.contains('=') {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            t if t.starts_with('-') => {
                i += 1;
                continue;
            }
            _ => return Some(i),
        }
    }
    None
}

fn has_flag(tokens: &[&str], from: usize, names: &[&str]) -> bool {
    tokens[from..].iter().any(|t| {
        names.iter().any(|n| {
            *t == *n
                || t.starts_with(&format!("{n}="))
                // combined short flags: -fD etc. — only for single-letter names
                || (n.len() == 2
                    && n.starts_with('-')
                    && !n.starts_with("--")
                    && t.starts_with('-')
                    && !t.starts_with("--")
                    && t.contains(n.chars().nth(1).unwrap()))
        })
    })
}

/// Destructive git shapes that discard worktree/index or rewrite remote history.
fn destructive_git_shape(segment: &str) -> Option<&'static str> {
    let tokens = segment_tokens(segment);
    let sub_i = git_subcommand_index(&tokens)?;
    let sub = tokens[sub_i];
    let rest = &tokens[sub_i + 1..];

    match sub {
        "reset" => Some("git reset"),
        "restore" => Some("git restore"),
        "clean" => Some("git clean"),
        // checkout / switch can discard local changes (-f) or restore paths (`-- .`).
        // Agents routinely use these to wipe WIP; always confirm.
        "checkout" | "switch" => Some("git checkout"),
        "push" => {
            if has_flag(
                rest,
                0,
                &["--force", "-f", "--force-with-lease", "--force-if-includes"],
            ) {
                Some("git push --force")
            } else {
                None
            }
        }
        "branch" => {
            if has_flag(rest, 0, &["-D", "-d", "--delete"]) {
                Some("git branch -D")
            } else {
                None
            }
        }
        "stash" => match rest.first().copied() {
            Some("drop" | "clear" | "pop") => Some("git stash drop"),
            _ => None,
        },
        "worktree" => match rest.first().copied() {
            Some("remove" | "prune") => Some("git worktree remove"),
            _ => None,
        },
        "filter-branch" | "filter-repo" => Some("git filter-branch"),
        _ => None,
    }
}

/// Recursive rm (project paths) — always confirm; root wipe is hard-blocked separately.
fn destructive_rm_shape(segment: &str) -> Option<&'static str> {
    let tokens = segment_tokens(segment);
    if tokens.first().copied() != Some("rm") {
        return None;
    }
    let mut recursive = false;
    for t in &tokens[1..] {
        if *t == "-rf" || *t == "-fr" || *t == "-r" || *t == "--recursive" {
            recursive = true;
        }
        if t.starts_with('-') && !t.starts_with("--") && t.contains('r') {
            recursive = true;
        }
    }
    if recursive {
        Some("rm -r")
    } else {
        None
    }
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
        assert!(requires_strict_confirmation("curl https://example.com").is_none());
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
        // Soft only — not strict.
        assert!(requires_strict_confirmation("git push origin main").is_none());
    }

    #[test]
    fn strict_git_destructive() {
        for cmd in [
            "git checkout -- .",
            "git checkout HEAD -- src/main.rs",
            "git restore .",
            "git restore --staged --worktree .",
            "git reset --hard",
            "git reset HEAD~1",
            "git reset",
            "git clean -fd",
            "git clean -f",
            "git push --force origin main",
            "git push -f",
            "git push --force-with-lease",
            "git branch -D feature",
            "git branch -d old",
            "git stash drop",
            "git stash clear",
            "git -C /tmp/repo checkout -- .",
            "cd /tmp && git restore .",
        ] {
            assert!(
                requires_strict_confirmation(cmd).is_some(),
                "expected strict confirm for: {cmd}"
            );
            assert!(
                requires_confirmation(cmd).is_some(),
                "expected confirm for: {cmd}"
            );
        }
        // Safe git should not be strict.
        for cmd in [
            "git status",
            "git diff",
            "git log -1",
            "git add -A",
            "git commit -m hi",
            "git branch",
            "git stash list",
            "git push origin main",
        ] {
            assert!(
                requires_strict_confirmation(cmd).is_none(),
                "unexpected strict for: {cmd}"
            );
        }
    }

    #[test]
    fn strict_rm_recursive() {
        assert_eq!(
            requires_strict_confirmation("rm -rf ./build"),
            Some("rm -r")
        );
        assert!(requires_strict_confirmation("rm file.txt").is_none());
    }

    #[test]
    fn confirm_sudo_and_skip_commentish() {
        assert!(requires_confirmation("sudo apt install x").is_some());
        assert!(requires_confirmation("# sudo apt install x").is_none());
        assert!(requires_strict_confirmation("sudo apt install x").is_none());
    }

    #[test]
    fn allows_rm_project_path_without_root_block() {
        // Not hard-blocked (only /), but still needs confirm as recursive rm.
        assert!(is_command_blocked("rm -rf ./build").is_none());
        assert!(requires_confirmation("rm -rf ./build").is_some());
        assert!(requires_strict_confirmation("rm -rf ./build").is_some());
    }

    #[test]
    fn destructive_reason_prefix() {
        let r = destructive_ask_reason("git checkout");
        assert!(is_destructive_ask_reason(&r));
        assert!(!is_destructive_ask_reason("high-risk bash pattern `sudo`"));
    }

    #[test]
    fn shell_segments_handles_multibyte_utf8() {
        // En-dash U+2013 is 3 UTF-8 bytes; old byte-step slice panicked here.
        let cmd = "echo hello – world && git push origin main";
        let normalized = normalize_command(cmd);
        let segs = shell_segments(&normalized);
        assert!(segs.iter().any(|s| s.contains("git push")));
        assert!(requires_confirmation(cmd).is_some());
        // Must not panic when scanning for rm-root / blocked shapes either.
        assert!(is_command_blocked("echo path – with dash; true").is_none());
        // Relative path so substring hard-block `rm -rf /` does not match.
        assert!(is_command_blocked("rm -rf ./tmp/foo – backup").is_none());
        // Long multi-byte prefix (same class of bug as panic at byte ~1563).
        let long = format!("{} && git status", "–".repeat(600));
        assert!(is_command_blocked(&long).is_none());
        assert!(requires_strict_confirmation(&long).is_none());
    }
}
