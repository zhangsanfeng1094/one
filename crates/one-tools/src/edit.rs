//! Surgical file edit tool (Claude / OpenCode / Pi compatible).
//!
//! Matching and diff logic live in [`crate::edit_diff`] (exact → fuzzy, CRLF, unified diff).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use crate::edit_diff::{
    apply_edit_lf, apply_line_ending, detect_line_ending, format_edit_success, normalize_to_lf,
};
use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::{
    bool_arg_names, new_string_arg, old_string_arg, path_arg, path_properties,
};

pub struct EditTool {
    policy: PathPolicy,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

/// Per-path mutation lock shared by all edit tool instances (Pi-style queue).
fn file_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let key = path.to_string_lossy().into_owned();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().expect("edit file lock map");
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        let scope = if self.policy.is_full_access() {
            "any path".to_string()
        } else {
            format!(
                "paths under workspace `{}` (and --add-dir roots)",
                self.policy.cwd().display()
            )
        };
        let mut properties = path_properties(
            "Existing file path (aliases: `file_path`, `filePath`)",
        );
        if let Some(obj) = properties.as_object_mut() {
            obj.insert(
                "old_string".into(),
                json!({
                    "type": "string",
                    "description": "Text to find (aliases: oldString, oldText). Prefer exact match; trailing whitespace / smart quotes are tolerated via fuzzy fallback. Must match uniquely unless replace_all is true. Do not include read-tool line-number prefixes."
                }),
            );
            obj.insert(
                "new_string".into(),
                json!({
                    "type": "string",
                    "description": "Replacement text (aliases: newString, newText). Must differ from old_string."
                }),
            );
            obj.insert(
                "replace_all".into(),
                json!({
                    "type": "boolean",
                    "description": "If true, replace every occurrence (alias: replaceAll). Default false — then the match must be unique."
                }),
            );
        }
        ToolDefinition {
            name: "edit".to_string(),
            description: format!(
                "Surgical in-place edit (Claude / OpenCode / Pi compatible): replace `old_string` \
                 with `new_string` in an existing file. Prefer this over `write` for bugfixes and \
                 localized changes. Matching: exact first, then fuzzy (trailing whitespace, smart \
                 quotes/dashes). By default the match must be unique (fails if 0 or >1); set \
                 `replace_all=true` to change every occurrence (e.g. rename). Include enough \
                 surrounding context when not using replace_all. Read the file before editing when \
                 you need current contents. Allowed: {scope}."
            ),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": ["old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = path_arg(&call.arguments).ok_or_else(|| {
            invalid_args(
                "edit",
                "missing `path` / `file_path` / `filePath`",
            )
        })?;
        let old_string = old_string_arg(&call.arguments).ok_or_else(|| {
            invalid_args(
                "edit",
                "missing `old_string` / `oldString` / `oldText`",
            )
        })?;
        let new_string = new_string_arg(&call.arguments).ok_or_else(|| {
            invalid_args(
                "edit",
                "missing `new_string` / `newString` / `newText`",
            )
        })?;
        let replace_all =
            bool_arg_names(&call.arguments, &["replace_all", "replaceAll"]).unwrap_or(false);

        let resolved = self
            .policy
            .resolve(path, AccessKind::Write)
            .map_err(|err| tool_error("edit", err))?;

        let lock = file_lock(&resolved);
        let _guard = lock.lock().await;

        let raw = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| tool_error("edit", err.to_string()))?;

        let ending = detect_line_ending(&raw);
        let content_lf = normalize_to_lf(&raw);
        let old_lf = normalize_to_lf(old_string);
        let new_lf = normalize_to_lf(new_string);

        let applied = apply_edit_lf(&content_lf, &old_lf, &new_lf, replace_all)
            .map_err(|e| tool_error("edit", e.user_message()))?;

        let to_write = apply_line_ending(&applied.content_lf, ending);
        tokio::fs::write(&resolved, to_write)
            .await
            .map_err(|err| tool_error("edit", err.to_string()))?;

        let text = format_edit_success(
            path,
            &content_lf,
            &applied.content_lf,
            applied.replacements,
            applied.strategy,
        );
        let old_lines = old_lf.lines().count().max(1);
        let new_lines = new_lf.lines().count().max(1);
        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "path": path,
                "replacements": applied.replacements,
                "replace_all": replace_all,
                "old_lines": old_lines,
                "new_lines": new_lines,
                "strategy": applied.strategy.as_str(),
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-edit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn replace_all_renames_every_occurrence() {
        let dir = temp_dir();
        let file = dir.join("lib.rs");
        std::fs::write(&file, "foo\nbar foo\nfoo end\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "path": "lib.rs",
                    "old_string": "foo",
                    "new_string": "baz",
                    "replace_all": true
                }),
            })
            .await
            .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, "baz\nbar baz\nbaz end\n");
        assert_eq!(out.details.as_ref().unwrap()["replacements"], 3);
        assert!(
            out.as_text().contains("3 replacements"),
            "{}",
            out.as_text()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_replace_all_requires_unique_match() {
        let dir = temp_dir();
        let file = dir.join("lib.rs");
        std::fs::write(&file, "foo\nfoo\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "file_path": "lib.rs",
                    "old_string": "foo",
                    "new_string": "bar"
                }),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("matched 2 times") || msg.contains("replace_all"),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn accepts_file_path_alias() {
        let dir = temp_dir();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello world\n").unwrap();

        let tool = EditTool::new(dir.clone());
        tool.execute(&ToolCall {
            id: "1".into(),
            name: "edit".into(),
            arguments: json!({
                "file_path": "a.txt",
                "old_string": "world",
                "new_string": "one"
            }),
        })
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello one\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn accepts_opencode_camel_case_args() {
        let dir = temp_dir();
        let file = dir.join("b.txt");
        std::fs::write(&file, "alpha beta\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "filePath": "b.txt",
                    "oldString": "beta",
                    "newString": "gamma",
                    "replaceAll": false
                }),
            })
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha gamma\n");
        assert_eq!(out.details.as_ref().unwrap()["strategy"], "exact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fuzzy_trailing_whitespace() {
        let dir = temp_dir();
        let file = dir.join("main.rs");
        // trailing spaces on first line
        std::fs::write(&file, "fn main() {  \n    let x = 1;\n}\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "path": "main.rs",
                    "old_string": "fn main() {\n    let x = 1;\n}",
                    "new_string": "fn main() {\n    let x = 2;\n}"
                }),
            })
            .await
            .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("let x = 2"), "{text}");
        assert_eq!(out.details.as_ref().unwrap()["strategy"], "fuzzy");
        assert!(out.as_text().contains("fuzzy") || out.as_text().contains("Updated"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preserves_crlf() {
        let dir = temp_dir();
        let file = dir.join("win.txt");
        std::fs::write(&file, "line1\r\nline2\r\n").unwrap();

        let tool = EditTool::new(dir.clone());
        tool.execute(&ToolCall {
            id: "1".into(),
            name: "edit".into(),
            arguments: json!({
                "path": "win.txt",
                "old_string": "line2",
                "new_string": "LINE2"
            }),
        })
        .await
        .unwrap();

        let bytes = std::fs::read(&file).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\r\n"), "expected CRLF preserved: {text:?}");
        assert!(text.contains("LINE2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn not_found_is_actionable() {
        let dir = temp_dir();
        let file = dir.join("a.txt");
        std::fs::write(&file, "only this\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "path": "a.txt",
                    "old_string": "missing block",
                    "new_string": "x"
                }),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("re-read") || msg.contains("not find"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn result_contains_unified_diff_markers() {
        let dir = temp_dir();
        let file = dir.join("d.txt");
        std::fs::write(&file, "a\nb\nc\n").unwrap();

        let tool = EditTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "path": "d.txt",
                    "old_string": "b",
                    "new_string": "B"
                }),
            })
            .await
            .unwrap();
        let t = out.as_text();
        assert!(t.contains("--- a/d.txt"), "{t}");
        assert!(t.contains("+++ b/d.txt"), "{t}");
        assert!(t.contains("-b") || t.contains("- b"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
