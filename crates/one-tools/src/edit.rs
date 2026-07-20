use std::path::PathBuf;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::{bool_arg, path_arg, path_properties};

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
            "Existing file path (Claude Code alias: `file_path`)",
        );
        if let Some(obj) = properties.as_object_mut() {
            obj.insert(
                "old_string".into(),
                json!({
                    "type": "string",
                    "description": "Exact text to find. Must match uniquely unless replace_all is true."
                }),
            );
            obj.insert(
                "new_string".into(),
                json!({
                    "type": "string",
                    "description": "Replacement text (must differ from old_string)"
                }),
            );
            obj.insert(
                "replace_all".into(),
                json!({
                    "type": "boolean",
                    "description": "If true, replace every occurrence of old_string (Claude Code). Default false — then old_string must match exactly once."
                }),
            );
        }
        ToolDefinition {
            name: "edit".to_string(),
            description: format!(
                "Surgical in-place edit (Claude Code Edit-compatible): replace `old_string` with \
                 `new_string` in an existing file. Prefer this over `write` for bugfixes and \
                 localized changes. By default `old_string` must match uniquely (fails if 0 or \
                 >1 matches); set `replace_all=true` to change every occurrence (e.g. rename an \
                 identifier). Include enough surrounding context when not using replace_all. \
                 Allowed: {scope}."
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
            invalid_args("edit", "missing `path` or `file_path`")
        })?;
        let old_string = call
            .arguments
            .get("old_string")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("edit", "missing `old_string`"))?;
        let new_string = call
            .arguments
            .get("new_string")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("edit", "missing `new_string`"))?;
        let replace_all = bool_arg(&call.arguments, "replace_all", None).unwrap_or(false);

        if old_string == new_string {
            return Err(tool_error(
                "edit",
                "old_string and new_string must be different",
            ));
        }

        let resolved = self
            .policy
            .resolve(path, AccessKind::Write)
            .map_err(|err| tool_error("edit", err))?;
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| tool_error("edit", err.to_string()))?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Err(tool_error("edit", "old_string not found"));
        }
        if !replace_all && count > 1 {
            return Err(tool_error(
                "edit",
                format!(
                    "old_string matched {count} times; must be unique, or set replace_all=true"
                ),
            ));
        }

        let replacements = if replace_all { count } else { 1 };
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        tokio::fs::write(&resolved, updated)
            .await
            .map_err(|err| tool_error("edit", err.to_string()))?;

        let diff = unified_edit_diff(path, old_string, new_string, replacements);
        let old_lines = old_string.lines().count().max(1);
        let new_lines = new_string.lines().count().max(1);
        Ok(ToolOutput::text_with_details(
            diff,
            json!({
                "path": path,
                "replacements": replacements,
                "replace_all": replace_all,
                "old_lines": old_lines,
                "new_lines": new_lines,
            }),
        ))
    }
}

/// Compact unified-style preview for the TUI and the model.
fn unified_edit_diff(path: &str, old: &str, new: &str, replacements: usize) -> String {
    let mut out = String::new();
    if replacements > 1 {
        out.push_str(&format!(
            "Updated {path} ({replacements} replacements)\n"
        ));
    } else {
        out.push_str(&format!("Updated {path}\n"));
    }
    out.push_str("--- a/");
    out.push_str(path);
    out.push('\n');
    out.push_str("+++ b/");
    out.push_str(path);
    out.push('\n');
    // Line-oriented hunk (not a real LCS diff — good enough for unique replace).
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.lines().collect()
    };
    let new_lines: Vec<&str> = if new.is_empty() {
        Vec::new()
    } else {
        new.lines().collect()
    };
    out.push_str(&format!(
        "@@ -{} +{} @@\n",
        old_lines.len().max(1),
        new_lines.len().max(1)
    ));
    for line in &old_lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    // Trim trailing newline for cleaner tool_result storage.
    while out.ends_with('\n') {
        out.pop();
    }
    out
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

    #[test]
    fn diff_marks_old_and_new() {
        let d = unified_edit_diff("src/a.rs", "fn a() {}", "fn a() {\n    ok\n}", 1);
        assert!(d.contains("--- a/src/a.rs"));
        assert!(d.contains("+++ b/src/a.rs"));
        assert!(d.contains("-fn a() {}"));
        assert!(d.contains("+fn a() {"));
        assert!(d.contains("+    ok"));
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
        assert!(msg.contains("matched 2 times") || msg.contains("replace_all"), "{msg}");
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
}
