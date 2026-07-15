//! Plan-mode tools: restricted write/edit for the plan file + exit signal.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

/// Shared flag set when the model calls `exit_plan_mode`.
#[derive(Debug, Clone)]
pub struct PlanExitState {
    pub requested: bool,
    pub plan_path: PathBuf,
}

impl PlanExitState {
    pub fn new(plan_path: PathBuf) -> Self {
        Self {
            requested: false,
            plan_path,
        }
    }

    pub fn clear(&mut self) {
        self.requested = false;
    }
}

/// Resolve a user/tool path against cwd.
fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Compare two paths, tolerating missing files (no canonicalize required).
fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => {
            // Fallback: normalize with components (no FS).
            let na: PathBuf = a.components().collect();
            let nb: PathBuf = b.components().collect();
            na == nb
        }
    }
}

fn is_allowed_plan_path(cwd: &Path, allowed: &Path, path: &str) -> bool {
    if path == allowed.to_string_lossy().as_ref() {
        return true;
    }
    paths_equal(&resolve_path(cwd, path), allowed)
}

/// Write only the plan file (create/overwrite).
pub struct PlanWriteTool {
    cwd: PathBuf,
    plan_path: PathBuf,
}

impl PlanWriteTool {
    pub fn new(cwd: PathBuf, plan_path: PathBuf) -> Self {
        Self { cwd, plan_path }
    }
}

#[async_trait]
impl Tool for PlanWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: format!(
                "Create or overwrite the plan file only. Allowed path: {}. \
                 Do not write any other files while in plan mode.",
                self.plan_path.display()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("write", "missing `path`"))?;
        let content = call
            .arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("write", "missing `content`"))?;

        if !is_allowed_plan_path(&self.cwd, &self.plan_path, path) {
            return Err(tool_error(
                "write",
                format!(
                    "plan mode: only the plan file may be written ({}). Got: {path}",
                    self.plan_path.display()
                ),
            ));
        }

        if let Some(parent) = self.plan_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| tool_error("write", err.to_string()))?;
        }

        tokio::fs::write(&self.plan_path, content)
            .await
            .map_err(|err| tool_error("write", err.to_string()))?;

        Ok(ToolOutput::text_with_details(
            format!(
                "Wrote {} bytes to plan file {}",
                content.len(),
                self.plan_path.display()
            ),
            json!({
                "path": self.plan_path.to_string_lossy(),
                "bytes": content.len(),
                "plan": true,
            }),
        ))
    }
}

/// Edit only the plan file (exact string replace).
pub struct PlanEditTool {
    cwd: PathBuf,
    plan_path: PathBuf,
}

impl PlanEditTool {
    pub fn new(cwd: PathBuf, plan_path: PathBuf) -> Self {
        Self { cwd, plan_path }
    }
}

#[async_trait]
impl Tool for PlanEditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_string(),
            description: format!(
                "Replace an exact string in the plan file only. Allowed path: {}.",
                self.plan_path.display()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("edit", "missing `path`"))?;
        let old_string = call
            .arguments
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("edit", "missing `old_string`"))?;
        let new_string = call
            .arguments
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("edit", "missing `new_string`"))?;

        if !is_allowed_plan_path(&self.cwd, &self.plan_path, path) {
            return Err(tool_error(
                "edit",
                format!(
                    "plan mode: only the plan file may be edited ({}). Got: {path}",
                    self.plan_path.display()
                ),
            ));
        }

        let content = tokio::fs::read_to_string(&self.plan_path)
            .await
            .map_err(|err| tool_error("edit", err.to_string()))?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Err(tool_error("edit", "old_string not found"));
        }
        if count > 1 {
            return Err(tool_error(
                "edit",
                format!("old_string matched {count} times; must be unique"),
            ));
        }

        let updated = content.replacen(old_string, new_string, 1);
        tokio::fs::write(&self.plan_path, &updated)
            .await
            .map_err(|err| tool_error("edit", err.to_string()))?;

        Ok(ToolOutput::text_with_details(
            format!("Edited plan file {}", self.plan_path.display()),
            json!({
                "path": self.plan_path.to_string_lossy(),
                "plan": true,
            }),
        ))
    }
}

/// Signal that planning is done; client reads the plan file for approval.
pub struct ExitPlanModeTool {
    state: Arc<Mutex<PlanExitState>>,
}

impl ExitPlanModeTool {
    pub fn new(state: Arc<Mutex<PlanExitState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exit_plan_mode".to_string(),
            description: "\
Call this when you have finished writing the plan file and are ready for user approval. \
This tool does NOT take the plan content — it reads the plan from the plan file path given \
in the system prompt. Only use after a clear, unambiguous implementation plan is on disk. \
Do NOT use for pure research/Q&A that needs no code changes.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Optional one-line summary of the plan for the user."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let summary = call
            .arguments
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let plan_path = {
            let state = self.state.lock().expect("plan exit state");
            state.plan_path.clone()
        };

        let content = match tokio::fs::read_to_string(&plan_path).await {
            Ok(c) => c,
            Err(err) => {
                return Err(tool_error(
                    "exit_plan_mode",
                    format!(
                        "plan file missing or unreadable ({}): {err}. Write the plan first.",
                        plan_path.display()
                    ),
                ));
            }
        };

        if content.trim().is_empty() {
            return Err(tool_error(
                "exit_plan_mode",
                format!(
                    "plan file is empty ({}). Write a non-empty plan before exiting plan mode.",
                    plan_path.display()
                ),
            ));
        }

        {
            let mut state = self.state.lock().expect("plan exit state");
            state.requested = true;
        }

        let mut msg = format!(
            "Plan submitted for user approval.\n\
             Path: {}\n\
             The user will review the plan and approve with /act (or keep iterating in plan mode).\n\
             Do NOT implement code changes until the user explicitly approves.",
            plan_path.display()
        );
        if !summary.is_empty() {
            msg.push_str("\n\nSummary: ");
            msg.push_str(&summary);
        }

        Ok(ToolOutput::text_with_details(
            msg,
            json!({
                "plan_path": plan_path.to_string_lossy(),
                "pending_approval": true,
                "bytes": content.len(),
                "summary": summary,
            }),
        ))
    }
}

/// Build the tool set for plan mode (read-only exploration + plan file + exit).
pub fn plan_mode_tools(
    cwd: PathBuf,
    plan_path: PathBuf,
    exit_state: Arc<Mutex<PlanExitState>>,
) -> Vec<Arc<dyn Tool>> {
    plan_mode_tools_with_policy(
        crate::path_policy::PathPolicy::workspace(cwd),
        plan_path,
        exit_state,
    )
}

/// Plan mode tools with an explicit path policy (workspace boundary + plan file allow).
pub fn plan_mode_tools_with_policy(
    policy: crate::path_policy::PathPolicy,
    plan_path: PathBuf,
    exit_state: Arc<Mutex<PlanExitState>>,
) -> Vec<Arc<dyn Tool>> {
    let cwd = policy.cwd().to_path_buf();
    // Allow reading/writing the plan file even when it lives under ~/.one/agent/plans.
    let policy = policy.with_allowed_file(plan_path.clone());
    #[allow(unused_mut)] // mut when `network` feature pushes extra tools
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::read::ReadTool::with_policy(policy.clone())),
        Arc::new(crate::grep::GrepTool::with_policy(policy.clone())),
        Arc::new(crate::find::FindTool::with_policy(policy.clone())),
        Arc::new(crate::ls::LsTool::with_policy(policy)),
        Arc::new(PlanWriteTool::new(cwd.clone(), plan_path.clone())),
        Arc::new(PlanEditTool::new(cwd, plan_path)),
        Arc::new(ExitPlanModeTool::new(exit_state)),
    ];
    #[cfg(feature = "network")]
    {
        tools.push(Arc::new(crate::web_search::WebSearchTool::new()));
        tools.push(Arc::new(crate::web_fetch::WebFetchTool::new()));
    }
    tools
}

/// System-prompt overlay injected while plan mode is active.
pub fn plan_mode_system_overlay(plan_path: &Path) -> String {
    format!(
        r#"

## Plan mode is active

The user indicated they do not want you to execute yet. You MUST NOT make any edits \
to application code, run shell commands, change configs, or make commits. This supersedes \
other instructions about implementing changes.

You MAY:
- Read files, search the codebase (grep/find/ls), and use web tools when needed
- Ask the user clarifying questions
- Write and edit ONLY the plan file at: `{plan}`
- Call `exit_plan_mode` when the plan is ready for approval

### Workflow
1. **Understand** — explore relevant code and clarify ambiguous requirements with the user
2. **Design** — pick one recommended approach (not a laundry list of alternatives)
3. **Write plan** — write a concise markdown plan to the plan file (overview, critical files, numbered steps)
4. **Exit** — call `exit_plan_mode` when the plan is clear enough to implement

Keep the plan scannable: short bullets, concrete file paths, ordered steps. Do not implement until approved.
"#,
        plan = plan_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "one-plan-test-{}-{}-{:?}",
            std::process::id(),
            n,
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn plan_write_rejects_other_paths() {
        let dir = temp_dir();
        let plan = dir.join("plan.md");
        let tool = PlanWriteTool::new(dir.clone(), plan);
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: json!({"path": "src/main.rs", "content": "nope"}),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("plan mode"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plan_write_allows_plan_path() {
        let dir = temp_dir();
        let plan = dir.join("plan.md");
        let tool = PlanWriteTool::new(dir.clone(), plan.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: json!({
                    "path": plan.to_string_lossy(),
                    "content": "# Plan\n1. do thing\n"
                }),
            })
            .await
            .unwrap();
        assert!(out.as_text().contains("Wrote"));
        assert_eq!(
            tokio::fs::read_to_string(&plan).await.unwrap().trim(),
            "# Plan\n1. do thing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exit_requires_nonempty_plan() {
        let dir = temp_dir();
        let plan = dir.join("plan.md");
        let state = Arc::new(Mutex::new(PlanExitState::new(plan.clone())));
        let tool = ExitPlanModeTool::new(state.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "exit_plan_mode".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing") || err.to_string().contains("unreadable"));

        tokio::fs::write(&plan, "# ok\n").await.unwrap();
        let out = tool
            .execute(&ToolCall {
                id: "2".into(),
                name: "exit_plan_mode".into(),
                arguments: json!({"summary": "ready"}),
            })
            .await
            .unwrap();
        assert!(out.as_text().contains("approval"));
        assert!(state.lock().unwrap().requested);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_tools_exclude_bash() {
        let dir = temp_dir();
        let plan = dir.join("p.md");
        let state = Arc::new(Mutex::new(PlanExitState::new(plan.clone())));
        let tools = plan_mode_tools(dir.clone(), plan, state);
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"write".to_string()));
        assert!(names.contains(&"exit_plan_mode".to_string()));
        assert!(!names.iter().any(|n| n == "bash"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
