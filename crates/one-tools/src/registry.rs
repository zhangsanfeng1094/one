//! Tool name → factory registry.
//!
//! **AgentSpec / ToolsSpec only select names.** New builtins register here once;
//! harness and main session both materialize via [`ToolRegistry::materialize`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use one_core::tool::Tool;
use one_core::tool_gate::ToolGate;

use crate::ask_user::{AskUserHandler, AskUserTool, FailClosedAskUser};
use crate::bash::BashTool;
use crate::bash_kill::BashKillTool;
use crate::bash_output::BashOutputTool;
use crate::edit::EditTool;
use crate::find::FindTool;
use crate::grep::GrepTool;
use crate::ls::LsTool;
use crate::path_policy::PathPolicy;
use crate::read::ReadTool;
use crate::tasks::BackgroundTaskRegistry;
use crate::write::WriteTool;
use crate::ToolBuildOptions;

/// Runtime deps needed to construct stateful tools (path policy, bash registry, …).
#[derive(Clone)]
pub struct ToolBuildContext {
    pub policy: PathPolicy,
    pub auto_approve: bool,
    pub bg_registry: Arc<BackgroundTaskRegistry>,
    pub ask_user: Option<Arc<dyn AskUserHandler>>,
    pub tool_gate: Option<Arc<dyn ToolGate>>,
}

impl ToolBuildContext {
    pub fn from_options(opts: ToolBuildOptions) -> Self {
        Self {
            policy: opts.policy,
            auto_approve: opts.auto_approve,
            bg_registry: opts.registry,
            ask_user: opts.ask_user,
            tool_gate: opts.tool_gate,
        }
    }

    pub fn workspace(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            policy: PathPolicy::workspace(cwd.into()),
            auto_approve: true,
            bg_registry: Arc::new(BackgroundTaskRegistry::new()),
            ask_user: None,
            tool_gate: None,
        }
    }

    pub fn with_policy(mut self, policy: PathPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_auto_approve(mut self, auto_approve: bool) -> Self {
        self.auto_approve = auto_approve;
        self
    }

    pub fn with_bg_registry(mut self, registry: Arc<BackgroundTaskRegistry>) -> Self {
        self.bg_registry = registry;
        self
    }

    pub fn with_ask_user(mut self, handler: Arc<dyn AskUserHandler>) -> Self {
        self.ask_user = Some(handler);
        self
    }

    pub fn with_tool_gate(mut self, gate: Arc<dyn ToolGate>) -> Self {
        self.tool_gate = Some(gate);
        self
    }

    fn ask_handler(&self) -> Arc<dyn AskUserHandler> {
        self.ask_user
            .clone()
            .unwrap_or_else(|| Arc::new(FailClosedAskUser) as Arc<dyn AskUserHandler>)
    }
}

/// Built-in profile catalogs (names only). Source of truth for ToolsSpec profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinToolProfile {
    Coding,
    ReadOnly,
    /// Explore-style hard whitelist: no ask_user, no writes.
    Explore,
    Plan,
    /// Empty base; only explicit allow/extra matter.
    None,
}

impl BuiltinToolProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::ReadOnly => "read_only",
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::None => "none",
        }
    }

    /// Default tool names for this profile (may include names only present with features).
    pub fn tool_names(self) -> Vec<String> {
        match self {
            Self::None => vec![],
            Self::Explore => {
                #[allow(unused_mut)]
                let mut v = vec!["read".into(), "grep".into(), "find".into(), "ls".into()];
                #[cfg(feature = "network")]
                {
                    v.push("web_search".into());
                    v.push("web_fetch".into());
                }
                v
            }
            Self::ReadOnly => {
                #[allow(unused_mut)]
                let mut v = vec![
                    "read".into(),
                    "grep".into(),
                    "find".into(),
                    "ls".into(),
                    "ask_user".into(),
                ];
                #[cfg(feature = "network")]
                {
                    v.push("web_search".into());
                    v.push("web_fetch".into());
                }
                v
            }
            Self::Plan => {
                let mut v = BuiltinToolProfile::ReadOnly.tool_names();
                // plan / exit_plan_mode are runtime-injected (need plan path state).
                v.push("plan".into());
                v.push("exit_plan_mode".into());
                v
            }
            Self::Coding => {
                #[allow(unused_mut)]
                let mut v = vec![
                    "read".into(),
                    "write".into(),
                    "edit".into(),
                    "bash".into(),
                    "bash_output".into(),
                    "bash_kill".into(),
                    "grep".into(),
                    "find".into(),
                    "ls".into(),
                    "ask_user".into(),
                ];
                #[cfg(feature = "network")]
                {
                    v.push("web_search".into());
                    v.push("web_fetch".into());
                }
                v
            }
        }
    }
}

type FactoryFn = dyn Fn(&ToolBuildContext) -> Arc<dyn Tool> + Send + Sync;

enum RegistryEntry {
    Factory(Arc<FactoryFn>),
    /// Pre-built instance (MCP, extensions, plan tools, meta-tools).
    Instance(Arc<dyn Tool>),
}

/// Maps tool names to factories or pre-built instances.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    entries: BTreeMap<String, Arc<RegistryEntry>>,
}

#[derive(Debug, Clone)]
pub struct UnknownToolError {
    pub name: String,
    pub known: Vec<String>,
}

impl fmt::Display for UnknownToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown tool `{}` (not in registry). known: {}",
            self.name,
            if self.known.is_empty() {
                "(none)".into()
            } else {
                self.known.join(", ")
            }
        )
    }
}

impl std::error::Error for UnknownToolError {}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// All built-in coding tools (factories). Does not include plan/task/MCP.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register_builtins();
        r
    }

    pub fn register_factory<F>(&mut self, name: impl Into<String>, factory: F) -> &mut Self
    where
        F: Fn(&ToolBuildContext) -> Arc<dyn Tool> + Send + Sync + 'static,
    {
        self.entries.insert(
            name.into(),
            Arc::new(RegistryEntry::Factory(Arc::new(factory))),
        );
        self
    }

    /// Register a ready-made tool (MCP, extension, plan, meta). Overwrites same name.
    pub fn register_instance(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.definition().name;
        self.entries
            .insert(name, Arc::new(RegistryEntry::Instance(tool)));
        self
    }

    pub fn register_instances(
        &mut self,
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> &mut Self {
        for t in tools {
            self.register_instance(t);
        }
        self
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn known_names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Materialize tools in the given order. Unknown names → error (fail-fast).
    pub fn materialize(
        &self,
        names: &[String],
        ctx: &ToolBuildContext,
    ) -> Result<Vec<Arc<dyn Tool>>, UnknownToolError> {
        let mut out = Vec::with_capacity(names.len());
        let mut seen = std::collections::HashSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            let entry = self.entries.get(name).ok_or_else(|| UnknownToolError {
                name: name.clone(),
                known: self.known_names(),
            })?;
            let tool = match entry.as_ref() {
                RegistryEntry::Factory(f) => f(ctx),
                RegistryEntry::Instance(t) => t.clone(),
            };
            out.push(tool);
        }
        Ok(out)
    }

    /// Like [`materialize`] but skips unknown names (for soft merge of extras).
    pub fn materialize_skip_unknown(
        &self,
        names: &[String],
        ctx: &ToolBuildContext,
    ) -> Vec<Arc<dyn Tool>> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(entry) = self.entries.get(name) else {
                continue;
            };
            let tool = match entry.as_ref() {
                RegistryEntry::Factory(f) => f(ctx),
                RegistryEntry::Instance(t) => t.clone(),
            };
            out.push(tool);
        }
        out
    }

    fn register_builtins(&mut self) {
        self.register_factory("read", |ctx| {
            Arc::new(ReadTool::with_policy(ctx.policy.clone())) as Arc<dyn Tool>
        });
        self.register_factory("write", |ctx| {
            Arc::new(WriteTool::with_policy(ctx.policy.clone())) as Arc<dyn Tool>
        });
        self.register_factory("edit", |ctx| {
            Arc::new(EditTool::with_policy(ctx.policy.clone())) as Arc<dyn Tool>
        });
        self.register_factory("bash", |ctx| {
            Arc::new(BashTool::with_policy_and_gate(
                ctx.policy.clone(),
                ctx.auto_approve,
                ctx.bg_registry.clone(),
                ctx.tool_gate.clone(),
            )) as Arc<dyn Tool>
        });
        self.register_factory("bash_output", |ctx| {
            Arc::new(BashOutputTool::new(ctx.bg_registry.clone())) as Arc<dyn Tool>
        });
        self.register_factory("bash_kill", |ctx| {
            Arc::new(BashKillTool::new(ctx.bg_registry.clone())) as Arc<dyn Tool>
        });
        self.register_factory("grep", |ctx| {
            Arc::new(GrepTool::with_policy(ctx.policy.clone())) as Arc<dyn Tool>
        });
        self.register_factory("find", |ctx| {
            Arc::new(FindTool::with_policy(ctx.policy.clone())) as Arc<dyn Tool>
        });
        self.register_factory("ls", |ctx| {
            Arc::new(LsTool::with_policy(ctx.policy.clone())) as Arc<dyn Tool>
        });
        self.register_factory("ask_user", |ctx| {
            Arc::new(AskUserTool::new(ctx.ask_handler())) as Arc<dyn Tool>
        });
        #[cfg(feature = "network")]
        {
            self.register_factory("web_search", |_ctx| {
                Arc::new(crate::WebSearchTool::new()) as Arc<dyn Tool>
            });
            self.register_factory("web_fetch", |_ctx| {
                Arc::new(crate::WebFetchTool::new()) as Arc<dyn Tool>
            });
        }
    }
}

/// Resolve final name list from profile + allow/deny/extra (protocol ToolsSpec algorithm).
pub fn resolve_tool_names(
    profile: BuiltinToolProfile,
    allow: &[String],
    deny: &[String],
    extra: &[String],
) -> Vec<String> {
    let mut base = if !allow.is_empty() {
        allow.to_vec()
    } else {
        profile.tool_names()
    };
    base.retain(|n| !deny.iter().any(|d| d == n));
    for e in extra {
        if !base.iter().any(|b| b == e) {
            base.push(e.clone());
        }
    }
    // Drop network tools when feature off so profiles stay materializable.
    #[cfg(not(feature = "network"))]
    {
        base.retain(|n| n != "web_search" && n != "web_fetch");
    }
    base
}

/// Materialize a full coding set (compat with older helpers).
pub fn materialize_coding(ctx: &ToolBuildContext) -> Vec<Arc<dyn Tool>> {
    let reg = ToolRegistry::with_builtins();
    let names = BuiltinToolProfile::Coding.tool_names();
    reg.materialize(&names, ctx)
        .expect("builtin coding profile materializes")
}

/// Materialize read_only set (includes ask_user).
pub fn materialize_read_only(ctx: &ToolBuildContext) -> Vec<Arc<dyn Tool>> {
    let reg = ToolRegistry::with_builtins();
    let names = BuiltinToolProfile::ReadOnly.tool_names();
    reg.materialize(&names, ctx)
        .expect("builtin read_only profile materializes")
}

/// Explore hard whitelist (no ask_user).
pub fn materialize_explore(ctx: &ToolBuildContext) -> Vec<Arc<dyn Tool>> {
    let reg = ToolRegistry::with_builtins();
    let names = BuiltinToolProfile::Explore.tool_names();
    reg.materialize(&names, ctx)
        .expect("builtin explore profile materializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn coding_has_write_and_bash() {
        let ctx = ToolBuildContext::workspace(PathBuf::from("/tmp"));
        let tools = materialize_coding(&ctx);
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        assert!(names.contains(&"write".to_string()));
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"read".to_string()));
    }

    #[test]
    fn explore_excludes_write_and_ask() {
        let ctx = ToolBuildContext::workspace(PathBuf::from("/tmp"));
        let tools = materialize_explore(&ctx);
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        for forbidden in ["write", "edit", "bash", "ask_user", "task"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "explore must not include {forbidden}, got {names:?}"
            );
        }
    }

    #[test]
    fn allow_overrides_profile() {
        let names = resolve_tool_names(
            BuiltinToolProfile::Coding,
            &["read".into(), "grep".into()],
            &[],
            &[],
        );
        assert_eq!(names, vec!["read", "grep"]);
    }

    #[test]
    fn deny_strips_from_profile() {
        let names =
            resolve_tool_names(BuiltinToolProfile::ReadOnly, &[], &["ask_user".into()], &[]);
        assert!(!names.iter().any(|n| n == "ask_user"));
        assert!(names.iter().any(|n| n == "read"));
    }

    #[test]
    fn unknown_tool_errors() {
        let reg = ToolRegistry::with_builtins();
        let ctx = ToolBuildContext::workspace(PathBuf::from("/tmp"));
        match reg.materialize(&["nope".into()], &ctx) {
            Ok(_) => panic!("expected unknown tool error"),
            Err(err) => assert_eq!(err.name, "nope"),
        }
    }

    #[test]
    fn register_instance_overlay() {
        let mut reg = ToolRegistry::with_builtins();
        let ctx = ToolBuildContext::workspace(PathBuf::from("/tmp"));
        // Overlay: register a second read via instance still works as "read"
        let custom = Arc::new(ReadTool::with_policy(PathPolicy::workspace(PathBuf::from(
            "/tmp",
        )))) as Arc<dyn Tool>;
        reg.register_instance(custom);
        let tools = reg.materialize(&["read".into()], &ctx).unwrap();
        assert_eq!(tools.len(), 1);
    }
}
