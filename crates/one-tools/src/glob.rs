//! Built-in file discovery by glob pattern, gitignore-aware.
//!
//! Fast file pattern matching tool that works with any codebase size.
//! Same matching semantics as the `grep` tool's `glob` filter: patterns like
//! `*.rs` match at any depth, `**/*.rs` matches recursively, and hidden /
//! gitignored paths are skipped (Claude Code Glob-compatible). Implemented
//! with the same `ignore`-crate walk as `grep` — no host `find`/`rg` binary
//! required.

use std::path::PathBuf;

use async_trait::async_trait;
use ignore::overrides::OverrideBuilder;
use ignore::{Match, WalkState};
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::glob_util::{expand_brace_glob, walk_builder, MAX_WALK_ENTRIES};
use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::path_arg_or;

pub struct GlobTool {
    policy: PathPolicy,
}

pub type FindTool = GlobTool;

impl GlobTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".to_string(),
            description: "Fast file pattern matching tool that works with any codebase size. \
                 Matches file paths using glob patterns (e.g. `**/*.rs`, `src/**/*.ts`, `*test*`). \
                 Skips .gitignore'd and hidden files by default. Returns a list of matching file paths. \
                 Prefer this over bash `find` or `ls -R`."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern like `**/*.rs` or `src/**/*.ts`"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search under (default: workspace root)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let pattern = call
            .arguments
            .get("pattern")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("glob", "missing or empty `pattern`"))?
            .to_string();
        let path = path_arg_or(&call.arguments, ".").map_err(|msg| invalid_args("glob", msg))?;

        let root = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("glob", err))?;

        // A file root can never contain matches (same as the old glob walk).
        if root.is_file() {
            return Ok(ToolOutput::text(String::new()));
        }

        let policy = self.policy.clone();
        let matches = tokio::task::spawn_blocking(move || collect_matches(root, pattern, policy))
            .await
            .map_err(|err| tool_error("glob", format!("glob task failed: {err}")))?;
        let matches = matches?;

        let joined = matches.join("\n");
        let presented = crate::truncate::present_tool_output(
            &joined,
            "glob",
            self.policy.cwd(),
            crate::truncate::PreviewStyle::Head,
        );
        Ok(ToolOutput::text(presented.text))
    }
}

pub fn collect_matches(root: PathBuf, pattern: String, policy: PathPolicy) -> Result<Vec<String>> {
    let walker = walk_builder(&root);

    // Build a standalone matcher instead of `walker.overrides(...)`: a
    // whitelist override bypasses .gitignore / hidden filtering entirely
    // (same "re-include" behavior as `rg -g`), which would defeat the
    // gitignore-aware walk. Filtering per-entry keeps both filters active.
    let mut ob = OverrideBuilder::new(&root);
    for part in expand_brace_glob(&pattern) {
        ob.add(&part)
            .map_err(|err| tool_error("glob", format!("invalid glob `{part}`: {err}")))?;
    }
    let overrides = ob
        .build()
        .map_err(|err| tool_error("glob", format!("invalid glob: {err}")))?;

    let matches = std::sync::Mutex::new(Vec::new());
    let err_slot = std::sync::Mutex::new(None::<String>);

    walker.build_parallel().run(|| {
        let matches = &matches;
        let err_slot = &err_slot;
        let policy = &policy;
        let overrides = &overrides;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue, // non-fatal walk error (broken symlink etc.)
            };
            // Files and directories both count as matches (like the old glob
            // walk); symlinks are neither followed nor reported.
            let Some(ft) = entry.file_type() else {
                return WalkState::Continue;
            };
            if !(ft.is_file() || ft.is_dir()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            if policy.check(path, AccessKind::Read).is_err() {
                return WalkState::Continue;
            }
            // Glob filter on top of the walker's gitignore/hidden filtering.
            if !matches!(overrides.matched(path, ft.is_dir()), Match::Whitelist(_)) {
                return WalkState::Continue;
            }
            if let Ok(mut guard) = matches.lock() {
                guard.push(path.display().to_string());
            }
            // Soft cap: avoid walking unbounded trees for agent use.
            if let Ok(guard) = matches.lock() {
                if guard.len() >= MAX_WALK_ENTRIES {
                    if let Ok(mut e) = err_slot.lock() {
                        *e = Some(format!(
                            "glob capped at {MAX_WALK_ENTRIES} entries; narrow pattern/path"
                        ));
                    }
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });

    if let Ok(slot) = err_slot.lock() {
        if let Some(msg) = slot.as_ref() {
            // Soft warning only if we got zero entries; otherwise proceed partial.
            let empty = matches.lock().map(|g| g.is_empty()).unwrap_or(true);
            if empty {
                return Err(tool_error("glob", msg.clone()));
            }
        }
    }

    let mut matches = matches.into_inner().unwrap_or_default();
    matches.sort();
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-glob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn beta() {}\n").unwrap();
        std::fs::write(dir.join("readme.md"), "# docs\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\nsecret.txt\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "nope\n").unwrap();
        std::fs::write(dir.join("secret.txt"), "nope\n").unwrap();
        std::fs::write(dir.join(".hidden.txt"), "nope\n").unwrap();
        dir
    }

    async fn run_glob(dir: &std::path::Path, pattern: &str) -> String {
        let tool = GlobTool::new(dir.to_path_buf());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "glob".into(),
                arguments: json!({ "pattern": pattern }),
            })
            .await
            .unwrap();
        out.as_text().to_string()
    }

    #[tokio::test]
    async fn glob_matches_at_any_depth() {
        let dir = temp_workspace();
        let text = run_glob(&dir, "*.rs").await;
        assert!(text.contains("src/a.rs"), "got: {text}");
        assert!(text.contains("b.rs"), "got: {text}");
        assert!(!text.contains("readme.md"), "got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn double_star_recursive() {
        let dir = temp_workspace();
        let text = run_glob(&dir, "**/*.rs").await;
        assert!(text.contains("src/a.rs"), "got: {text}");
        assert!(text.contains("b.rs"), "got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_gitignored_and_hidden() {
        let dir = temp_workspace();
        let text = run_glob(&dir, "**").await;
        assert!(text.contains("src/a.rs"), "got: {text}");
        assert!(!text.contains("ignored.txt"), "gitignored leaked: {text}");
        assert!(!text.contains("secret.txt"), "gitignored leaked: {text}");
        assert!(!text.contains(".hidden.txt"), "hidden leaked: {text}");
        assert!(!text.contains(".gitignore"), ".gitignore is hidden: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn matches_directories_too() {
        let dir = temp_workspace();
        let text = run_glob(&dir, "src").await;
        assert!(
            text.contains("src") && !text.contains("a.rs"),
            "expected just the `src` dir, got: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn brace_glob_expands_in_glob() {
        let dir = temp_workspace();
        std::fs::write(dir.join("c.ts"), "type C = 1;\n").unwrap();
        let text = run_glob(&dir, "*.{rs,ts}").await;
        assert!(text.contains("b.rs"), "got: {text}");
        assert!(text.contains("c.ts"), "got: {text}");
        assert!(!text.contains("readme.md"), "got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_pattern_is_rejected() {
        let dir = temp_workspace();
        let tool = GlobTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "glob".into(),
                arguments: json!({ "pattern": "  " }),
            })
            .await;
        assert!(out.is_err(), "empty pattern should error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
