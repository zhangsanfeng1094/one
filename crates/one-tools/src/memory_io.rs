//! Memory path helpers for tool I/O (M2 write soft-check, M3 age + lookup budget).
//!
//! Path layout matches `docs/memory.md` / `one-resources::memory`:
//! `…/memory/_global/…` and `…/memory/projects/<slug>/…`.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default max memory `read`/`grep` ops counted per user turn.
pub const DEFAULT_MAX_LOOKUPS_PER_TURN: usize = 6;

/// Session-shared counter for memory body lookups (reset each user turn).
#[derive(Debug)]
pub struct MemoryLookupBudget {
    max_per_turn: AtomicUsize,
    used: AtomicUsize,
}

impl MemoryLookupBudget {
    pub fn new(max_per_turn: usize) -> Arc<Self> {
        Arc::new(Self {
            max_per_turn: AtomicUsize::new(max_per_turn.max(1)),
            used: AtomicUsize::new(0),
        })
    }

    pub fn unlimited() -> Arc<Self> {
        // Effectively off: max is huge; still tracks used for tests/metrics.
        Self::new(10_000)
    }

    pub fn set_max(&self, max_per_turn: usize) {
        self.max_per_turn
            .store(max_per_turn.max(1), Ordering::Relaxed);
    }

    pub fn max(&self) -> usize {
        self.max_per_turn.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Reset at the start of each user turn.
    pub fn reset_turn(&self) {
        self.used.store(0, Ordering::Relaxed);
    }

    /// Consume one lookup if under budget. Returns Err with user-facing message when exceeded.
    pub fn try_consume(&self) -> Result<usize, String> {
        let max = self.max();
        // CAS loop so concurrent tools in one turn share the cap.
        loop {
            let used = self.used.load(Ordering::Relaxed);
            if used >= max {
                return Err(format!(
                    "Memory lookup budget exceeded ({used}/{max} this turn). \
                     Stop scanning memory and continue the task with what you have, \
                     or wait for the next user turn."
                ));
            }
            if self
                .used
                .compare_exchange_weak(used, used + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(used + 1);
            }
        }
    }
}

/// True if path sits under a One memory tree (`memory/_global` or `memory/projects/…`).
pub fn is_memory_path(path: &Path) -> bool {
    let comps: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    for i in 0..comps.len() {
        if comps[i] != "memory" {
            continue;
        }
        let Some(next) = comps.get(i + 1) else {
            continue;
        };
        if *next == "_global" || *next == "projects" {
            return true;
        }
    }
    false
}

/// MEMORY.md index file under a memory tree.
pub fn is_memory_index_path(path: &Path) -> bool {
    is_memory_path(path)
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("MEMORY.md"))
            .unwrap_or(false)
}

/// Soft validation after writing memory content. `None` = looks fine.
pub fn soft_check_memory_write(path: &Path, content: &str) -> Option<String> {
    if !is_memory_path(path) {
        return None;
    }
    if is_memory_index_path(path) {
        return soft_check_index(content);
    }
    soft_check_body(path, content)
}

fn soft_check_index(content: &str) -> Option<String> {
    let has_entry = content.lines().any(|l| {
        let t = l.trim();
        (t.starts_with("- ") || t.starts_with("* ")) && t.contains('[')
    });
    if has_entry {
        return None;
    }
    if content.trim().is_empty() {
        return Some(
            "MEMORY.md is empty. Add lines like:\n\
             `- [id] type=project scope=project tags=a,b — one-line description`\n\
             and a sibling `{id}.md` body. Index changes apply on next session / `/reload`."
                .into(),
        );
    }
    Some(
        "MEMORY.md has no list entries matching `- [id] type=… — description`. \
         Index lines are required for L2 catalog discovery."
            .into(),
    )
}

fn soft_check_body(path: &Path, content: &str) -> Option<String> {
    let mut hints = Vec::new();
    if !content.trim_start().starts_with("---") {
        hints.push(
            "Body has no YAML frontmatter. Prefer:\n\
             ---\n\
             name: …\n\
             type: project|feedback|user|reference\n\
             scope: global|project\n\
             tags: [a, b]\n\
             updated: YYYY-MM-DD\n\
             ---\n\
             …body…"
                .to_string(),
        );
    } else if parse_frontmatter_updated(content).is_none() {
        hints.push("Frontmatter missing `updated: YYYY-MM-DD` (helps age reminders on read).".into());
    }
    // Suggest updating MEMORY.md when writing a new body.
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        if !stem.eq_ignore_ascii_case("MEMORY") {
            hints.push(format!(
                "If this is a new memory, ensure MEMORY.md in the same directory has an entry for `[{stem}]` \
                 (L2 catalog is session-frozen; new index lines appear after `/reload` or a new session)."
            ));
        }
    }
    if hints.is_empty() {
        None
    } else {
        Some(hints.join("\n\n"))
    }
}

/// Parse optional `updated:` / `date:` from simple YAML frontmatter.
pub fn parse_frontmatter_updated(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after = trimmed.strip_prefix("---")?;
    let end = after.find("\n---")?;
    let fm = &after[..end];
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line
            .strip_prefix("updated:")
            .or_else(|| line.strip_prefix("date:"))
        {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Build age reminder text for a memory body path (mtime + optional frontmatter).
pub fn memory_age_reminder(path: &Path, content: &str) -> String {
    let mut parts = Vec::new();
    if let Some(updated) = parse_frontmatter_updated(content) {
        parts.push(format!("frontmatter updated={updated}"));
        if let Some(days) = days_since_ymd(&updated) {
            parts.push(format!("~{days} day(s) since updated field"));
        }
    }
    if let Some(days) = days_since_mtime(path) {
        parts.push(format!("file mtime ~{days} day(s) ago"));
    }
    let age = if parts.is_empty() {
        "age unknown".to_string()
    } else {
        parts.join("; ")
    };
    format!(
        "Memory body (`{}`): {age}. Point-in-time observation — \
         **verify against current code/config** before asserting as fact.",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("memory")
    )
}

/// Prepend age system-reminder to presented memory body text.
pub fn wrap_memory_read(path: &Path, content: &str, presented: &str) -> String {
    let reminder = one_core::system_reminder(memory_age_reminder(path, content));
    if presented.trim().is_empty() {
        reminder
    } else {
        format!("{reminder}\n\n{presented}")
    }
}

fn days_since_mtime(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let now = SystemTime::now();
    let dur = now.duration_since(modified).ok()?;
    Some(dur.as_secs() / 86_400)
}

/// Parse `YYYY-MM-DD` roughly and return whole days since that date (UTC-ish).
fn days_since_ymd(s: &str) -> Option<u64> {
    let s = s.trim();
    let mut parts = s.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    // Days since Unix epoch (approximate civil calendar via time crate free algorithm).
    let epoch_days = civil_days_from_ymd(y, m, d)?;
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        / 86_400;
    let today = today as i64;
    if today >= epoch_days {
        Some((today - epoch_days) as u64)
    } else {
        Some(0)
    }
}

/// Days from Unix epoch for a civil date (Howard Hinnant algorithm).
fn civil_days_from_ymd(y: i64, m: u32, d: u32) -> Option<i64> {
    if m == 0 || m > 12 || d == 0 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe as i64 - 719468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_memory_layout() {
        assert!(is_memory_path(Path::new(
            "/home/u/.one/agent/memory/_global/foo.md"
        )));
        assert!(is_memory_path(Path::new(
            "/home/u/.one/agent/memory/projects/x-deadbeef/MEMORY.md"
        )));
        assert!(!is_memory_path(Path::new("/home/u/project/src/main.rs")));
        assert!(is_memory_index_path(Path::new(
            "/x/memory/_global/MEMORY.md"
        )));
        assert!(!is_memory_index_path(Path::new(
            "/x/memory/_global/tip.md"
        )));
    }

    #[test]
    fn soft_check_index_and_body() {
        assert!(soft_check_memory_write(
            Path::new("/m/memory/_global/MEMORY.md"),
            "- [a] type=user scope=global tags=x — hi\n"
        )
        .is_none());
        assert!(soft_check_memory_write(
            Path::new("/m/memory/_global/MEMORY.md"),
            "no bullets here\n"
        )
        .is_some());
        let body_hint = soft_check_memory_write(
            Path::new("/m/memory/_global/tip.md"),
            "just text without fm\n",
        );
        assert!(body_hint.unwrap().contains("frontmatter"));
    }

    #[test]
    fn frontmatter_updated() {
        let c = "---\nname: X\nupdated: 2026-01-01\n---\nbody\n";
        assert_eq!(
            parse_frontmatter_updated(c).as_deref(),
            Some("2026-01-01")
        );
    }

    #[test]
    fn budget_caps() {
        let b = MemoryLookupBudget::new(2);
        assert!(b.try_consume().is_ok());
        assert!(b.try_consume().is_ok());
        assert!(b.try_consume().is_err());
        b.reset_turn();
        assert!(b.try_consume().is_ok());
    }

    #[test]
    fn wrap_includes_reminder() {
        let path = PathBuf::from("/tmp/memory/_global/x.md");
        let out = wrap_memory_read(&path, "body", "1|body");
        assert!(out.contains("system-reminder"));
        assert!(out.contains("1|body"));
    }
}
