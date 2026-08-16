//! Shared glob helpers for the built-in search tools (`grep`, `find`).
//!
//! One source of truth for two things:
//!
//! 1. Brace expansion of user globs (`*.{ts,tsx}` → `*.ts` + `*.tsx`), so the
//!    `ignore` crate's `Override` filter sees the same syntax ripgrep's `-g`
//!    flag supports.
//! 2. The gitignore-aware [`WalkBuilder`] configuration both tools use (skip
//!    hidden + gitignored paths, follow rg-ish defaults, cap worker threads).

use std::path::Path;

use ignore::WalkBuilder;

/// Soft cap on entries collected by a single walk, so agent tools stay light.
pub const MAX_WALK_ENTRIES: usize = 50_000;

/// Build a gitignore-aware walker rooted at `root`, with the same defaults as
/// ripgrep: respect `.gitignore` / `.ignore` / global git excludes, skip
/// hidden files, don't require a git repo, and cap worker threads for agent
/// use. Overrides / custom filters are layered on top by the caller.
pub fn walk_builder(root: &Path) -> WalkBuilder {
    let mut walker = WalkBuilder::new(root);
    walker.hidden(true);
    walker.parents(true);
    walker.git_ignore(true);
    walker.git_global(true);
    walker.git_exclude(true);
    walker.ignore(true);
    // Follow rg-ish defaults: skip ignored, don't require git.
    walker.require_git(false);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(1);
    walker.threads(threads);
    walker
}

/// Expand a single-level brace glob: `**/*.{ts,tsx}` → two globs.
pub fn expand_brace_glob(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_string()];
    };
    let Some(close) = glob[open..].find('}') else {
        return vec![glob.to_string()];
    };
    let close = open + close;
    let prefix = &glob[..open];
    let suffix = &glob[close + 1..];
    let inner = &glob[open + 1..close];
    if inner.is_empty() || inner.contains('{') {
        return vec![glob.to_string()];
    }
    inner
        .split(',')
        .map(|part| format!("{prefix}{}{suffix}", part.trim()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brace_glob_expands() {
        let parts = expand_brace_glob("**/*.{ts,tsx}");
        assert_eq!(parts, vec!["**/*.ts".to_string(), "**/*.tsx".to_string()]);
    }

    #[test]
    fn brace_glob_without_braces_passes_through() {
        assert_eq!(expand_brace_glob("*.rs"), vec!["*.rs".to_string()]);
        // Unterminated or nested braces are left untouched.
        assert_eq!(expand_brace_glob("*.{ts"), vec!["*.{ts".to_string()]);
        assert_eq!(
            expand_brace_glob("**/*.{ts,{js,jsx}}"),
            vec!["**/*.{ts,{js,jsx}}".to_string()]
        );
    }
}
