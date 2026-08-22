//! Graph-based intent recognition and dynamic reminder injection engine.
//!
//! Provides a labeled property graph (LPG) representation of triggers, aliases,
//! entities, hierarchical intents, and reminders, supporting:
//! - Token-aware verb-object matching (CJK blocked compounds + ASCII word boundaries)
//! - Verb families so 查/搜/search share recall without flooding learned rules
//! - Multi-hop intent inheritance (`SubIntentOf`)
//! - Negative trigger and conflict resolution (`VetoedBy`, `ConflictsWith`)
//! - Cooldown, tool availability, and priority-ranked reminder injection

pub use crate::intent_match::{is_conceptual_clarification, strip_xml_and_meta};

use crate::intent_match::{
    cap_learned_objects, extract_latin_idents, extract_verbs_and_objects, has_crate_like_ident,
    phrase_in_text, stable_hash, token_in_text, TokenRole,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Summary of a learned rule for display and feedback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearnedRuleSummary {
    pub intent_id: String,
    pub intent_name: String,
    pub triggers: Vec<String>,
    pub reminder_id: String,
    pub reminder_title: String,
    pub reminder_level: ReminderLevel,
    pub reminder_content: String,
    pub suggested_tools: Vec<String>,
    pub source: String,
}

/// Severity / constraint level of a Reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderLevel {
    /// Informational tip or suggestion.
    Info,
    /// Recommended workflow or preferred tool pattern.
    Recommended,
    /// Mandatory constraint or safety guardrail.
    Mandatory,
}

impl Default for ReminderLevel {
    fn default() -> Self {
        Self::Recommended
    }
}

impl ReminderLevel {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Mandatory => "🔴 强制规范",
            Self::Recommended => "🟡 建议策略",
            Self::Info => "ℹ️ 提示参考",
        }
    }
}

/// Matching strategy for triggers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MatchMode {
    /// Case-insensitive substring search.
    Substring { phrase: String },
    /// Exact match after whitespace & case normalization.
    Exact { phrase: String },
    /// Verb + Object pairing (e.g. "查" / "找" + "资料" / "文档").
    VerbObject {
        verbs: Vec<String>,
        objects: Vec<String>,
        /// When true, a crate-like latin identifier (tokio, reqwest, …) counts as an object.
        #[serde(default)]
        allow_latin_ident: bool,
    },
}

/// Optional runtime context for [`IntentGraph::infer_with`].
#[derive(Debug, Clone, Default)]
pub struct InferOptions {
    pub entity_params: HashMap<String, String>,
    /// If non-empty, drop tool suggestions whose name is not in this list
    /// (builtin tool names + connected MCP server names).
    pub available_tools: Vec<String>,
    /// Monotonic user-turn index used with reminder `cooldown_turns`.
    pub turn_index: u32,
    /// reminder_id → last turn it was injected.
    pub reminder_last_turn: HashMap<String, u32>,
    /// Plan mode: only Mandatory reminders survive.
    pub mandatory_only: bool,
}

fn mode_matches(mode: &MatchMode, query: &str) -> bool {
    match mode {
        MatchMode::Substring { phrase } => phrase_in_text(query, phrase),
        MatchMode::Exact { phrase } => query.trim().eq_ignore_ascii_case(phrase.trim()),
        MatchMode::VerbObject {
            verbs,
            objects,
            allow_latin_ident,
        } => {
            let has_verb = verbs
                .iter()
                .any(|v| token_in_text(query, v, TokenRole::Verb));
            let has_obj = objects
                .iter()
                .any(|o| token_in_text(query, o, TokenRole::Object));
            let has_ident = *allow_latin_ident && has_crate_like_ident(query);
            has_verb && (has_obj || has_ident)
        }
    }
}

fn tool_is_available(name: &str, available: &[String]) -> bool {
    if available.is_empty() {
        return true;
    }
    let n = name.to_ascii_lowercase();
    available.iter().any(|a| {
        let a = a.to_ascii_lowercase();
        if a == n {
            return true;
        }
        if let Some((srv, rest)) = a.split_once("__") {
            if srv == n || rest == n {
                return true;
            }
        }
        n.len() >= 3 && a.contains(&n)
    })
}

/// A node in the intent property graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node_type", rename_all = "snake_case")]
pub enum GraphNode {
    /// Surface trigger phrase or matching rule.
    Trigger {
        id: String,
        mode: MatchMode,
        #[serde(default = "default_weight")]
        weight: f32,
        #[serde(default)]
        is_negative: bool,
    },
    /// Canonical task/action intent.
    Intent {
        id: String,
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default = "default_priority")]
        priority: u32,
        #[serde(default)]
        category: String,
    },
    /// Domain entity / tool / library / concept.
    Entity {
        id: String,
        name: String,
        #[serde(default)]
        category: String,
        #[serde(default)]
        synonyms: Vec<String>,
    },
    /// Dynamic reminder / constraint / guideline.
    Reminder {
        id: String,
        title: String,
        #[serde(default)]
        level: ReminderLevel,
        #[serde(default = "default_priority")]
        priority: u32,
        #[serde(default)]
        category: String,
        template: String,
        #[serde(default)]
        cooldown_turns: u32,
        #[serde(default)]
        scope: String,
    },
    /// Tool or MCP action recommendation.
    Tool {
        id: String,
        name: String,
        #[serde(default)]
        description: String,
    },
    /// Context condition (environment or state requirements).
    Context {
        id: String,
        key: String,
        value: String,
    },
}

fn default_weight() -> f32 {
    1.0
}

fn default_priority() -> u32 {
    50
}

impl GraphNode {
    pub fn id(&self) -> &str {
        match self {
            Self::Trigger { id, .. } => id,
            Self::Intent { id, .. } => id,
            Self::Entity { id, .. } => id,
            Self::Reminder { id, .. } => id,
            Self::Tool { id, .. } => id,
            Self::Context { id, .. } => id,
        }
    }
}

/// An edge connecting two nodes in the property graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "relation", rename_all = "snake_case")]
pub enum GraphEdge {
    /// Trigger -> Intent (Activates intent with weight).
    Triggers {
        #[serde(default = "default_weight")]
        weight: f32,
    },
    /// SubIntent -> ParentIntent (Hierarchical intent inheritance).
    SubIntentOf,
    /// Synonym/Alias -> Canonical Intent or Entity.
    SynonymOf {
        #[serde(default = "default_weight")]
        weight: f32,
    },
    /// Intent -> Context (Requires specific condition).
    RequiresContext,
    /// Intent -> Trigger or Context (Negative rule vetoes intent).
    VetoedBy,
    /// Intent <-> Intent (Mutual exclusion).
    ConflictsWith,
    /// Intent -> Tool (Recommends a tool).
    SuggestsTool {
        #[serde(default = "default_priority")]
        priority: u32,
    },
    /// Intent -> Reminder (Injects a reminder when intent is active).
    InjectsReminder {
        #[serde(default = "default_priority")]
        priority: u32,
        #[serde(default = "default_weight")]
        weight: f32,
        #[serde(default)]
        condition: Option<String>,
    },
}

/// A stored directed edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeEntry {
    pub from: String,
    pub to: String,
    #[serde(flatten)]
    pub edge: GraphEdge,
}

/// In-memory Property Graph for Intent Recognition & Reminder Injection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentGraph {
    pub nodes: HashMap<String, GraphNode>,
    pub edges: Vec<EdgeEntry>,
    #[serde(skip)]
    outgoing: HashMap<String, Vec<usize>>,
    #[serde(skip)]
    incoming: HashMap<String, Vec<usize>>,
}

/// Matched Intent item with activation confidence and evidence path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchedIntent {
    pub intent_id: String,
    pub intent_name: String,
    pub confidence: f32,
    pub evidence_path: Vec<String>,
}

/// Active Reminder ready to be formatted and injected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveReminder {
    pub reminder_id: String,
    pub title: String,
    pub level: ReminderLevel,
    pub priority: u32,
    pub rendered_content: String,
    pub source_intent_id: String,
    pub confidence: f32,
    pub evidence: String,
}

/// Suggested Tool item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestedTool {
    pub tool_id: String,
    pub tool_name: String,
    pub priority: u32,
    pub source_intent_id: String,
}

/// Complete inference result produced by the graph engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphInferenceResult {
    pub matched_intents: Vec<MatchedIntent>,
    pub active_reminders: Vec<ActiveReminder>,
    pub suggested_tools: Vec<SuggestedTool>,
    pub active_entities: Vec<String>,
}

impl GraphInferenceResult {
    /// Render active reminders and tool suggestions into a markdown `<system-reminder>` block.
    pub fn render_reminder_markdown(&self) -> Option<String> {
        if self.active_reminders.is_empty() && self.suggested_tools.is_empty() {
            return None;
        }

        let mut out =
            String::from("### Graph Intent Guidance & Reminders (图意图识别与规则指引)\n");

        if !self.active_reminders.is_empty() {
            out.push_str("\n**激活的策略与约束提醒：**\n");
            for rem in &self.active_reminders {
                out.push_str(&format!(
                    "- {} **[{}]** (置信度: {:.2}, 来源: `{}`) · {}\n",
                    rem.level.badge(),
                    rem.title,
                    rem.confidence,
                    rem.source_intent_id,
                    rem.evidence
                ));
                for line in rem.rendered_content.lines() {
                    out.push_str(&format!("  {}\n", line.trim()));
                }
            }
        }

        if !self.suggested_tools.is_empty() {
            out.push_str("\n**推荐工具与偏好建议：**\n");
            for tool in &self.suggested_tools {
                out.push_str(&format!(
                    "- 建议优先考虑工具 `{}` (意图: `{}`)\n",
                    tool.tool_name, tool.source_intent_id
                ));
            }
        }

        out.push_str("\n*注：以上为图推理给出的建议与约束，若与用户显式指令冲突以用户指令为准。*");
        Some(out)
    }
}

impl IntentGraph {
    /// Create an empty IntentGraph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild fast adjacency indices after modifying nodes or edges.
    pub fn rebuild_index(&mut self) {
        self.outgoing.clear();
        self.incoming.clear();
        for (idx, edge) in self.edges.iter().enumerate() {
            self.outgoing
                .entry(edge.from.clone())
                .or_default()
                .push(idx);
            self.incoming.entry(edge.to.clone()).or_default().push(idx);
        }
    }

    /// Add a node to the graph.
    pub fn add_node(&mut self, node: GraphNode) {
        self.nodes.insert(node.id().to_string(), node);
    }

    /// Add a directed edge to the graph.
    pub fn add_edge(&mut self, from: impl Into<String>, to: impl Into<String>, edge: GraphEdge) {
        let entry = EdgeEntry {
            from: from.into(),
            to: to.into(),
            edge,
        };
        self.edges.push(entry);
    }

    /// Load graph from JSON string.
    pub fn from_json_str(json_str: &str) -> Result<Self, serde_json::Error> {
        let mut graph: Self = serde_json::from_str(json_str)?;
        graph.rebuild_index();
        Ok(graph)
    }

    /// Load graph definition from a file.
    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut graph: Self = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        graph.rebuild_index();
        Ok(graph)
    }

    /// Create default built-in graph with common intents, aliases, and reminder rules.
    pub fn with_builtin_rules() -> Self {
        let mut g = Self::new();
        let investigate_verbs = vec![
            "查".into(),
            "找".into(),
            "搜".into(),
            "检索".into(),
            "查阅".into(),
            "看".into(),
            "调研".into(),
            "了解".into(),
            "怎么用".into(),
            "如何用".into(),
            "search".into(),
            "lookup".into(),
            "find".into(),
        ];

        g.add_node(GraphNode::Intent {
            id: "RetrieveInformation".into(),
            name: "检索信息".into(),
            description: "查询资料、查阅外部文档、查找技术说明或 API".into(),
            priority: 70,
            category: "research".into(),
        });

        g.add_node(GraphNode::Intent {
            id: "SearchExternalDocs".into(),
            name: "查阅外部库文档".into(),
            description: "查阅第三方开源库、外部 API、框架文档".into(),
            priority: 85,
            category: "research".into(),
        });
        g.add_edge(
            "SearchExternalDocs",
            "RetrieveInformation",
            GraphEdge::SubIntentOf,
        );

        // General retrieve: documents / notes. No reminder — too broad to inject tools.
        g.add_node(GraphNode::Trigger {
            id: "trig-search-docs".into(),
            mode: MatchMode::VerbObject {
                verbs: investigate_verbs.clone(),
                objects: vec![
                    "资料".into(),
                    "文档".into(),
                    "说明".into(),
                    "用法".into(),
                    "使用方式".into(),
                    "使用方法".into(),
                    "api".into(),
                    "docs".into(),
                    "reference".into(),
                    "库".into(),
                    "crate".into(),
                    "package".into(),
                    "sdk".into(),
                ],
                allow_latin_ident: false,
            },
            weight: 1.0,
            is_negative: false,
        });
        g.add_edge(
            "trig-search-docs",
            "RetrieveInformation",
            GraphEdge::Triggers { weight: 1.0 },
        );

        // External docs: usage/API + crate-like identifiers (tokio, reqwest, …).
        g.add_node(GraphNode::Trigger {
            id: "trig-external-usage".into(),
            mode: MatchMode::VerbObject {
                verbs: investigate_verbs,
                objects: vec![
                    "用法".into(),
                    "文档".into(),
                    "api".into(),
                    "docs".into(),
                    "crate".into(),
                    "package".into(),
                    "sdk".into(),
                    "开源库".into(),
                    "第三方库".into(),
                    "reference".into(),
                    "库".into(),
                ],
                allow_latin_ident: true,
            },
            weight: 1.2,
            is_negative: false,
        });
        g.add_edge(
            "trig-external-usage",
            "SearchExternalDocs",
            GraphEdge::Triggers { weight: 1.2 },
        );

        for (id, phrase, w) in [
            ("trig-external-lib", "开源库", 1.2),
            ("trig-third-party", "第三方库", 1.2),
        ] {
            g.add_node(GraphNode::Trigger {
                id: id.into(),
                mode: MatchMode::Substring {
                    phrase: phrase.into(),
                },
                weight: w,
                is_negative: false,
            });
            g.add_edge(id, "SearchExternalDocs", GraphEdge::Triggers { weight: w });
        }

        for (id, phrase) in [
            ("trig-local-source-only", "当前项目源码"),
            ("trig-no-external-docs", "不要查外部文档"),
        ] {
            g.add_node(GraphNode::Trigger {
                id: id.into(),
                mode: MatchMode::Substring {
                    phrase: phrase.into(),
                },
                weight: 1.0,
                is_negative: true,
            });
            g.add_edge("SearchExternalDocs", id, GraphEdge::VetoedBy);
        }

        g.add_node(GraphNode::Reminder {
            id: "rem-deepwiki-docs".into(),
            title: "外部文档查询指引".into(),
            level: ReminderLevel::Recommended,
            priority: 80,
            category: "tool_preference".into(),
            template: "涉及第三方开源库（{{library}}）的 API、用法或规范时，优先调用 `deepwiki` 查阅权威文档，不要盲目推测。若只是项目内模块，改用 `find`/`grep` 读本地源码。"
                .into(),
            cooldown_turns: 2,
            scope: "global".into(),
        });
        g.add_edge(
            "SearchExternalDocs",
            "rem-deepwiki-docs",
            GraphEdge::InjectsReminder {
                priority: 80,
                weight: 1.0,
                condition: None,
            },
        );

        g.add_node(GraphNode::Tool {
            id: "tool-deepwiki".into(),
            name: "deepwiki".into(),
            description: "DeepWiki GitHub 仓库与开源库文档检索".into(),
        });
        g.add_node(GraphNode::Tool {
            id: "tool-find".into(),
            name: "find".into(),
            description: "文件与符号检索工具".into(),
        });
        g.add_edge(
            "SearchExternalDocs",
            "tool-deepwiki",
            GraphEdge::SuggestsTool { priority: 85 },
        );
        g.add_edge(
            "SearchExternalDocs",
            "tool-find",
            GraphEdge::SuggestsTool { priority: 70 },
        );

        g.add_node(GraphNode::Intent {
            id: "WebFactSearch".into(),
            name: "实时事实与新闻检索".into(),
            description: "查询最新新闻、实时资讯、客观事实或外部动态".into(),
            priority: 75,
            category: "research".into(),
        });

        g.add_node(GraphNode::Trigger {
            id: "trig-web-facts".into(),
            mode: MatchMode::VerbObject {
                verbs: vec![
                    "查".into(),
                    "搜".into(),
                    "找".into(),
                    "看".into(),
                    "了解".into(),
                    "search".into(),
                ],
                objects: vec![
                    "新闻".into(),
                    "近况".into(),
                    "动态".into(),
                    "最新消息".into(),
                    "资讯".into(),
                    "headline".into(),
                ],
                allow_latin_ident: false,
            },
            weight: 1.1,
            is_negative: false,
        });
        g.add_edge(
            "trig-web-facts",
            "WebFactSearch",
            GraphEdge::Triggers { weight: 1.1 },
        );

        g.add_node(GraphNode::Tool {
            id: "tool-web_search".into(),
            name: "web_search".into(),
            description: "网络搜索工具".into(),
        });
        g.add_node(GraphNode::Tool {
            id: "tool-agy".into(),
            name: "agy".into(),
            description: "Agy 网络搜索与多模态工具".into(),
        });
        g.add_edge(
            "WebFactSearch",
            "tool-web_search",
            GraphEdge::SuggestsTool { priority: 85 },
        );
        g.add_edge(
            "WebFactSearch",
            "tool-agy",
            GraphEdge::SuggestsTool { priority: 80 },
        );

        g.add_node(GraphNode::Reminder {
            id: "rem-web-fact-check".into(),
            title: "实时事实检索指引".into(),
            level: ReminderLevel::Recommended,
            priority: 75,
            category: "tool_preference".into(),
            template: "查询实时新闻、近期动态或客观事实时，建议优先使用网络搜索工具（如 web_search 或 agy）核实最新资讯。"
                .into(),
            cooldown_turns: 2,
            scope: "global".into(),
        });
        g.add_edge(
            "WebFactSearch",
            "rem-web-fact-check",
            GraphEdge::InjectsReminder {
                priority: 75,
                weight: 1.0,
                condition: None,
            },
        );

        g.add_node(GraphNode::Intent {
            id: "GitDestructiveAction".into(),
            name: "破坏性版本控制操作".into(),
            description: "强制推送、硬重置、删除分支等不可逆 git 操作".into(),
            priority: 95,
            category: "safety".into(),
        });

        for (id, phrase) in [
            ("trig-git-push-f", "push -f"),
            ("trig-git-push-force", "push --force"),
            ("trig-git-force-with-lease", "--force-with-lease"),
            ("trig-git-force-push-en", "force push"),
            ("trig-git-force-push-hy", "force-push"),
            ("trig-git-force-push-zh", "强制推送"),
            ("trig-git-reset-hard", "reset --hard"),
            ("trig-git-hard-reset", "hard reset"),
            ("trig-git-hard-reset-zh", "硬重置"),
            ("trig-git-hard-revert-zh", "硬回退"),
            ("trig-git-delete-remote-zh", "删除远程分支"),
            ("trig-git-delete-remote-en", "delete remote branch"),
        ] {
            g.add_node(GraphNode::Trigger {
                id: id.into(),
                mode: MatchMode::Substring {
                    phrase: phrase.into(),
                },
                weight: 1.5,
                is_negative: false,
            });
            g.add_edge(
                id,
                "GitDestructiveAction",
                GraphEdge::Triggers { weight: 1.5 },
            );
        }

        g.add_node(GraphNode::Reminder {
            id: "rem-git-safety-guard".into(),
            title: "破坏性 Git 操作确认".into(),
            level: ReminderLevel::Mandatory,
            priority: 100,
            category: "safety".into(),
            template: "执行 `git push --force`、`git reset --hard`、删除远程分支等破坏性操作前，必须向用户明确说明数据丢失风险并获得确认。"
                .into(),
            cooldown_turns: 0,
            scope: "global".into(),
        });
        g.add_edge(
            "GitDestructiveAction",
            "rem-git-safety-guard",
            GraphEdge::InjectsReminder {
                priority: 100,
                weight: 1.5,
                condition: None,
            },
        );

        g.add_node(GraphNode::Intent {
            id: "RunTestsAndVerification".into(),
            name: "运行测试与验证".into(),
            description: "运行单元测试、集成测试或基准评测".into(),
            priority: 65,
            category: "testing".into(),
        });

        g.add_node(GraphNode::Trigger {
            id: "trig-test-verbs".into(),
            mode: MatchMode::VerbObject {
                verbs: vec![
                    "跑".into(),
                    "运行".into(),
                    "执行".into(),
                    "run".into(),
                    "test".into(),
                ],
                objects: vec![
                    "测试".into(),
                    "单测".into(),
                    "cargo test".into(),
                    "pytest".into(),
                    "unit test".into(),
                ],
                allow_latin_ident: false,
            },
            weight: 1.0,
            is_negative: false,
        });
        g.add_edge(
            "trig-test-verbs",
            "RunTestsAndVerification",
            GraphEdge::Triggers { weight: 1.0 },
        );

        g.add_node(GraphNode::Reminder {
            id: "rem-test-failure-diagnosis".into(),
            title: "测试失败诊断要求".into(),
            level: ReminderLevel::Recommended,
            priority: 70,
            category: "testing".into(),
            template: "运行测试若出现失败，先检查失败用例的断言与上下文输出，定位根本原因，避免在未查明原因前随意修改测试用例。"
                .into(),
            cooldown_turns: 1,
            scope: "global".into(),
        });
        g.add_edge(
            "RunTestsAndVerification",
            "rem-test-failure-diagnosis",
            GraphEdge::InjectsReminder {
                priority: 70,
                weight: 1.0,
                condition: None,
            },
        );

        g.rebuild_index();
        g
    }

    /// Match a user query against the graph (no cooldown / tool filter).
    pub fn infer(
        &self,
        query: &str,
        entity_params: &HashMap<String, String>,
    ) -> GraphInferenceResult {
        self.infer_with(
            query,
            &InferOptions {
                entity_params: entity_params.clone(),
                ..InferOptions::default()
            },
        )
    }

    /// Match a user query with runtime context (cooldown, available tools, plan mode).
    pub fn infer_with(&self, query: &str, opts: &InferOptions) -> GraphInferenceResult {
        let q = query.trim();
        if q.is_empty() {
            return GraphInferenceResult::default();
        }

        let mut entity_params = opts.entity_params.clone();
        let latin_idents = extract_latin_idents(q);
        if !entity_params.contains_key("library") {
            if let Some(lib) = latin_idents.first() {
                entity_params.insert("library".into(), lib.clone());
            }
        }

        let mut active_triggers: HashMap<String, f32> = HashMap::new();
        let mut veto_triggers: HashSet<String> = HashSet::new();

        for node in self.nodes.values() {
            if let GraphNode::Trigger {
                id,
                mode,
                weight,
                is_negative,
            } = node
            {
                if mode_matches(mode, q) {
                    if *is_negative {
                        veto_triggers.insert(id.clone());
                    } else {
                        active_triggers.insert(id.clone(), *weight);
                    }
                }
            }
        }

        let mut intent_scores: HashMap<String, (f32, Vec<String>)> = HashMap::new();

        for (trig_id, weight) in &active_triggers {
            if let Some(edge_indices) = self.outgoing.get(trig_id) {
                for &idx in edge_indices {
                    let edge = &self.edges[idx];
                    if let GraphEdge::Triggers { weight: edge_w } = edge.edge {
                        let score = *weight * edge_w;
                        let entry = intent_scores
                            .entry(edge.to.clone())
                            .or_insert((0.0, Vec::new()));
                        entry.0 += score;
                        entry.1.push(format!("trigger:{trig_id}"));
                    }
                }
            }
        }

        let initial_intents: Vec<(String, f32)> = intent_scores
            .iter()
            .map(|(k, v)| (k.clone(), v.0))
            .collect();

        for (intent_id, score) in initial_intents {
            let mut stack = vec![(intent_id, score)];
            let mut visited = HashSet::new();
            while let Some((current, cur_score)) = stack.pop() {
                if !visited.insert(current.clone()) {
                    continue;
                }
                let Some(edge_indices) = self.outgoing.get(&current) else {
                    continue;
                };
                for &idx in edge_indices {
                    let edge = &self.edges[idx];
                    if matches!(edge.edge, GraphEdge::SubIntentOf) {
                        let parent = edge.to.clone();
                        let parent_entry = intent_scores
                            .entry(parent.clone())
                            .or_insert((0.0, Vec::new()));
                        parent_entry.0 += cur_score * 0.7;
                        parent_entry.1.push(format!("sub_intent:{current}"));
                        stack.push((parent, cur_score * 0.7));
                    }
                }
            }
        }

        let conflict_losers = self.conflict_losers(&intent_scores);
        let mut final_intents: Vec<MatchedIntent> = Vec::new();

        for (intent_id, (score, evidence)) in intent_scores {
            if conflict_losers.contains(&intent_id) {
                continue;
            }
            if self.is_vetoed(&intent_id, &veto_triggers) {
                continue;
            }
            if !self.context_satisfied(&intent_id, &entity_params) {
                continue;
            }

            let intent_name = match self.nodes.get(&intent_id) {
                Some(GraphNode::Intent { name, .. }) => name.clone(),
                _ => intent_id.clone(),
            };
            let confidence = (score / (score + 1.0)).min(0.99);
            final_intents.push(MatchedIntent {
                intent_id,
                intent_name,
                confidence,
                evidence_path: evidence,
            });
        }

        final_intents.sort_by(|a, b| {
            self.intent_priority(&b.intent_id)
                .cmp(&self.intent_priority(&a.intent_id))
                .then_with(|| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        let is_clarification = is_conceptual_clarification(q);
        let has_explicit_search_verbs =
            ["查", "搜", "检索", "找", "定位", "search", "find", "lookup"]
                .iter()
                .any(|&v| token_in_text(q, v, TokenRole::Verb));
        let suppress_tool_reminders = is_clarification && !has_explicit_search_verbs;

        let mut active_reminders: Vec<ActiveReminder> = Vec::new();
        let mut suggested_tools: Vec<SuggestedTool> = Vec::new();
        let mut seen_reminders: HashSet<String> = HashSet::new();
        let mut seen_tools: HashSet<String> = HashSet::new();

        for intent in &final_intents {
            let Some(edge_indices) = self.outgoing.get(&intent.intent_id) else {
                continue;
            };
            for &idx in edge_indices {
                let edge = &self.edges[idx];
                match &edge.edge {
                    GraphEdge::InjectsReminder {
                        priority, weight, ..
                    } => {
                        let rem_id = &edge.to;
                        if !seen_reminders.insert(rem_id.clone()) {
                            continue;
                        }
                        if let Some(GraphNode::Reminder {
                            title,
                            level,
                            template,
                            priority: rem_pri,
                            cooldown_turns,
                            ..
                        }) = self.nodes.get(rem_id)
                        {
                            if opts.mandatory_only && *level != ReminderLevel::Mandatory {
                                continue;
                            }
                            if suppress_tool_reminders && *level != ReminderLevel::Mandatory {
                                continue;
                            }
                            if *cooldown_turns > 0 {
                                if let Some(&last) = opts.reminder_last_turn.get(rem_id) {
                                    if last > 0
                                        && opts.turn_index > last
                                        && opts.turn_index.saturating_sub(last) <= *cooldown_turns
                                    {
                                        continue;
                                    }
                                }
                            }

                            let mut rendered = template.clone();
                            for (k, v) in &entity_params {
                                rendered = rendered.replace(&format!("{{{{{k}}}}}"), v);
                            }
                            if !entity_params.contains_key("library") {
                                rendered = rendered.replace("{{library}}", "该库");
                            }

                            active_reminders.push(ActiveReminder {
                                reminder_id: rem_id.clone(),
                                title: title.clone(),
                                level: *level,
                                priority: (*rem_pri).max(*priority),
                                rendered_content: rendered,
                                source_intent_id: intent.intent_id.clone(),
                                confidence: intent.confidence * weight,
                                evidence: intent.evidence_path.join(" -> "),
                            });
                        }
                    }
                    GraphEdge::SuggestsTool { priority } => {
                        if suppress_tool_reminders || opts.mandatory_only {
                            continue;
                        }
                        let tool_id = &edge.to;
                        let tool_name = match self.nodes.get(tool_id) {
                            Some(GraphNode::Tool { name, .. }) => name.clone(),
                            _ => tool_id.clone(),
                        };
                        if !tool_is_available(&tool_name, &opts.available_tools) {
                            continue;
                        }
                        if !seen_tools.insert(tool_name.clone()) {
                            continue;
                        }
                        suggested_tools.push(SuggestedTool {
                            tool_id: tool_id.clone(),
                            tool_name,
                            priority: *priority,
                            source_intent_id: intent.intent_id.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }

        active_reminders.sort_by(|a, b| {
            b.level
                .cmp(&a.level)
                .then_with(|| b.priority.cmp(&a.priority))
                .then_with(|| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        suggested_tools.sort_by(|a, b| b.priority.cmp(&a.priority));

        GraphInferenceResult {
            matched_intents: final_intents,
            active_reminders,
            suggested_tools,
            active_entities: latin_idents,
        }
    }

    fn is_vetoed(&self, intent_id: &str, veto_triggers: &HashSet<String>) -> bool {
        let Some(edge_indices) = self.outgoing.get(intent_id) else {
            return false;
        };
        edge_indices.iter().any(|&idx| {
            let edge = &self.edges[idx];
            matches!(edge.edge, GraphEdge::VetoedBy) && veto_triggers.contains(&edge.to)
        })
    }

    fn context_satisfied(&self, intent_id: &str, params: &HashMap<String, String>) -> bool {
        let Some(edge_indices) = self.outgoing.get(intent_id) else {
            return true;
        };
        for &idx in edge_indices {
            let edge = &self.edges[idx];
            if !matches!(edge.edge, GraphEdge::RequiresContext) {
                continue;
            }
            if let Some(GraphNode::Context { key, value, .. }) = self.nodes.get(&edge.to) {
                match params.get(key) {
                    Some(v) if value.is_empty() || value == "*" || v == value => {}
                    Some(_) => return false,
                    None => {}
                }
            }
        }
        true
    }

    fn intent_priority(&self, id: &str) -> u32 {
        match self.nodes.get(id) {
            Some(GraphNode::Intent { priority, .. }) => *priority,
            _ => 0,
        }
    }

    fn conflict_losers(&self, scores: &HashMap<String, (f32, Vec<String>)>) -> HashSet<String> {
        let mut losers = HashSet::new();
        for edge in &self.edges {
            if !matches!(edge.edge, GraphEdge::ConflictsWith) {
                continue;
            }
            if !scores.contains_key(&edge.from) || !scores.contains_key(&edge.to) {
                continue;
            }
            if losers.contains(&edge.from) || losers.contains(&edge.to) {
                continue;
            }
            let (a_score, _) = &scores[&edge.from];
            let (b_score, _) = &scores[&edge.to];
            let loser = if (a_score - b_score).abs() > f32::EPSILON {
                if a_score < b_score {
                    edge.from.clone()
                } else {
                    edge.to.clone()
                }
            } else if self.intent_priority(&edge.from) < self.intent_priority(&edge.to) {
                edge.from.clone()
            } else {
                edge.to.clone()
            };
            losers.insert(loser);
        }
        losers
    }

    /// Merge another graph into this one, overwriting nodes and appending new edges.
    pub fn merge(&mut self, other: IntentGraph) {
        for (id, node) in other.nodes {
            self.nodes.insert(id, node);
        }
        for edge in other.edges {
            if !self
                .edges
                .iter()
                .any(|e| e.from == edge.from && e.to == edge.to && e.edge == edge.edge)
            {
                self.edges.push(edge);
            }
        }
        self.rebuild_index();
    }

    /// Extract custom / learned subgraph (nodes with custom IDs).
    pub fn extract_custom_subgraph(&self) -> Self {
        let mut custom = Self::new();
        let custom_node_ids: HashSet<String> = self
            .nodes
            .keys()
            .filter(|id| id.starts_with("custom-"))
            .cloned()
            .collect();

        for (id, node) in &self.nodes {
            if custom_node_ids.contains(id) {
                custom.nodes.insert(id.clone(), node.clone());
            }
        }

        for edge in &self.edges {
            if custom_node_ids.contains(&edge.from) || custom_node_ids.contains(&edge.to) {
                custom.edges.push(edge.clone());
            }
        }

        custom.rebuild_index();
        custom
    }

    /// Save full graph to JSON file.
    pub fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Save custom subgraph to JSON file.
    pub fn save_custom_to_file(&self, path: &Path) -> std::io::Result<()> {
        let custom = self.extract_custom_subgraph();
        custom.save_to_file(path)
    }

    /// Load graph with built-in rules and merge any user-global and project-local custom rules.
    pub fn load_merged(cwd: &Path, agent_dir: &Path) -> Self {
        let mut graph = Self::with_builtin_rules();

        // 1. User-global custom graph: ~/.one/agent/intent_graph/custom.json
        let global_custom = agent_dir.join("intent_graph").join("custom.json");
        if global_custom.exists() {
            if let Ok(custom) = Self::from_file(&global_custom) {
                graph.merge(custom);
            }
        }

        // 2. Project-level custom graph: .one/intent_graph/custom.json or .one/intent_graph.json
        let proj_custom_dir = cwd.join(".one").join("intent_graph").join("custom.json");
        let proj_custom_file = cwd.join(".one").join("intent_graph.json");
        if proj_custom_dir.exists() {
            if let Ok(custom) = Self::from_file(&proj_custom_dir) {
                graph.merge(custom);
            }
        } else if proj_custom_file.exists() {
            if let Ok(custom) = Self::from_file(&proj_custom_file) {
                graph.merge(custom);
            }
        }

        graph
    }

    /// List all custom rules currently in the graph.
    pub fn list_custom_rules(&self) -> Vec<LearnedRuleSummary> {
        let mut rules = Vec::new();
        for (id, node) in &self.nodes {
            if !id.starts_with("custom-intent-") && !id.starts_with("custom-") {
                continue;
            }
            if let GraphNode::Intent { name, .. } = node {
                // Find connected triggers
                let mut triggers = Vec::new();
                for (t_id, t_node) in &self.nodes {
                    if let GraphNode::Trigger { mode, .. } = t_node {
                        if let Some(edge_indices) = self.outgoing.get(t_id) {
                            for &idx in edge_indices {
                                if self.edges[idx].to == *id {
                                    match mode {
                                        MatchMode::Substring { phrase } => {
                                            triggers.push(phrase.clone())
                                        }
                                        MatchMode::Exact { phrase } => {
                                            triggers.push(format!("exact:{phrase}"))
                                        }
                                        MatchMode::VerbObject { verbs, objects, .. } => {
                                            triggers.push(format!(
                                                "[{}] + [{}]",
                                                verbs.join(","),
                                                objects.join(",")
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Find connected reminders
                let mut rem_id_opt = None;
                let mut rem_title = String::new();
                let mut rem_level = ReminderLevel::Recommended;
                let mut rem_content = String::new();

                if let Some(edge_indices) = self.outgoing.get(id) {
                    for &idx in edge_indices {
                        let edge = &self.edges[idx];
                        if matches!(edge.edge, GraphEdge::InjectsReminder { .. }) {
                            if let Some(GraphNode::Reminder {
                                title,
                                level,
                                template,
                                ..
                            }) = self.nodes.get(&edge.to)
                            {
                                rem_id_opt = Some(edge.to.clone());
                                rem_title = title.clone();
                                rem_level = *level;
                                rem_content = template.clone();
                                break;
                            }
                        }
                    }
                }

                // Find connected tools
                let mut tools = Vec::new();
                if let Some(edge_indices) = self.outgoing.get(id) {
                    for &idx in edge_indices {
                        let edge = &self.edges[idx];
                        if matches!(edge.edge, GraphEdge::SuggestsTool { .. }) {
                            if let Some(GraphNode::Tool { name, .. }) = self.nodes.get(&edge.to) {
                                tools.push(name.clone());
                            } else {
                                tools.push(edge.to.clone());
                            }
                        }
                    }
                }

                if let Some(rem_id) = rem_id_opt {
                    rules.push(LearnedRuleSummary {
                        intent_id: id.clone(),
                        intent_name: name.clone(),
                        triggers,
                        reminder_id: rem_id,
                        reminder_title: rem_title,
                        reminder_level: rem_level,
                        reminder_content: rem_content,
                        suggested_tools: tools,
                        source: "custom".into(),
                    });
                }
            }
        }
        rules
    }

    /// Clear all custom learned rules and reset back to built-ins.
    pub fn clear_custom_rules(&mut self) {
        let mut builtin = Self::with_builtin_rules();
        std::mem::swap(self, &mut builtin);
    }

    /// Learn and update intent graph from user instruction or structured text rule.
    pub fn learn_from_text(&mut self, text: &str) -> Result<LearnedRuleSummary, String> {
        let clean_t = strip_xml_and_meta(text);
        let t = clean_t.trim();
        if t.is_empty()
            || t.starts_with("### ")
            || t.contains("Learned Tool Intent")
            || t.contains("Graph Intent Guidance")
        {
            return Err("学习规则文本无效，不能包含系统元指令或为空".into());
        }

        // 1. Structured format parsing (e.g. 意图: xxx | 触发: xxx | 提醒: xxx | 级别: xxx | 工具: xxx)
        if t.contains("意图:")
            || t.contains("intent:")
            || t.contains("提醒:")
            || t.contains("reminder:")
        {
            return self.learn_from_structured_text(t);
        }

        // 2. Natural language rule parsing
        self.learn_from_natural_language(t)
    }

    fn learn_from_structured_text(&mut self, text: &str) -> Result<LearnedRuleSummary, String> {
        let mut intent_name = String::new();
        let mut trigger_raw = String::new();
        let mut reminder_text = String::new();
        let mut level = ReminderLevel::Recommended;
        let mut tools_list: Vec<String> = Vec::new();

        // Split by pipe or newline
        for seg in text.split(['|', '\n']) {
            let seg = seg.trim();
            if let Some((k, v)) = seg.split_once(':').or_else(|| seg.split_once('：')) {
                let k = k.trim().to_lowercase();
                let v = v.trim().to_string();
                if k == "意图" || k == "intent" || k == "name" {
                    intent_name = v;
                } else if k == "触发" || k == "触发词" || k == "trigger" || k == "triggers" {
                    trigger_raw = v;
                } else if k == "提醒" || k == "reminder" || k == "content" || k == "rule" {
                    reminder_text = v;
                } else if k == "级别" || k == "level" || k == "severity" {
                    let v_lower = v.to_lowercase();
                    if v_lower.contains("强制")
                        || v_lower.contains("mandatory")
                        || v_lower.contains("必须")
                    {
                        level = ReminderLevel::Mandatory;
                    } else if v_lower.contains("提示")
                        || v_lower.contains("info")
                        || v_lower.contains("参考")
                    {
                        level = ReminderLevel::Info;
                    } else {
                        level = ReminderLevel::Recommended;
                    }
                } else if k == "工具" || k == "tool" || k == "tools" {
                    tools_list = v
                        .split([',', '，', '、', ' '])
                        .map(|s| s.trim().trim_matches('`').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }

        if intent_name.is_empty() {
            intent_name = "自定义意图".into();
        }
        if reminder_text.is_empty() {
            reminder_text = text.to_string();
        }

        let hash = stable_hash(&format!("{intent_name}-{trigger_raw}-{reminder_text}"));
        let intent_id = format!("custom-intent-{hash}");
        let trig_id = format!("custom-trig-{hash}");
        let rem_id = format!("custom-rem-{hash}");

        // Create Intent Node
        self.add_node(GraphNode::Intent {
            id: intent_id.clone(),
            name: intent_name.clone(),
            description: format!("自定义学习意图: {intent_name}"),
            priority: if level == ReminderLevel::Mandatory {
                90
            } else {
                70
            },
            category: "custom".into(),
        });

        // Create Trigger Node
        let mut triggers_disp = Vec::new();
        if trigger_raw.contains('+') {
            let (v_part, o_part) = trigger_raw.split_once('+').unwrap();
            let verbs: Vec<String> = v_part
                .split([',', '，', '、', ' '])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let objects: Vec<String> = o_part
                .split([',', '，', '、', ' '])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            triggers_disp.push(format!("[{}] + [{}]", verbs.join(","), objects.join(",")));
            self.add_node(GraphNode::Trigger {
                id: trig_id.clone(),
                mode: MatchMode::VerbObject {
                    verbs,
                    objects,
                    allow_latin_ident: false,
                },
                weight: 1.2,
                is_negative: false,
            });
        } else if !trigger_raw.is_empty() {
            let phrases: Vec<String> = trigger_raw
                .split([',', '，', '、'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            for (idx, phrase) in phrases.iter().enumerate() {
                let sub_trig_id = if idx == 0 {
                    trig_id.clone()
                } else {
                    format!("{trig_id}-{idx}")
                };
                triggers_disp.push(phrase.clone());
                self.add_node(GraphNode::Trigger {
                    id: sub_trig_id.clone(),
                    mode: MatchMode::Substring {
                        phrase: phrase.clone(),
                    },
                    weight: 1.0,
                    is_negative: false,
                });
                self.add_edge(
                    sub_trig_id,
                    intent_id.clone(),
                    GraphEdge::Triggers { weight: 1.0 },
                );
            }
        } else {
            // Fallback substring trigger
            triggers_disp.push(intent_name.clone());
            self.add_node(GraphNode::Trigger {
                id: trig_id.clone(),
                mode: MatchMode::Substring {
                    phrase: intent_name.clone(),
                },
                weight: 1.0,
                is_negative: false,
            });
            self.add_edge(
                trig_id.clone(),
                intent_id.clone(),
                GraphEdge::Triggers { weight: 1.0 },
            );
        }

        if trigger_raw.contains('+') || trigger_raw.is_empty() {
            // '+' branch created the node but not the edge; empty fallback already added.
            if trigger_raw.contains('+') {
                self.add_edge(
                    trig_id,
                    intent_id.clone(),
                    GraphEdge::Triggers { weight: 1.0 },
                );
            }
        }

        // Create Reminder Node
        let rem_title = format!("{intent_name}指引");
        self.add_node(GraphNode::Reminder {
            id: rem_id.clone(),
            title: rem_title.clone(),
            level,
            priority: if level == ReminderLevel::Mandatory {
                95
            } else {
                75
            },
            category: "custom".into(),
            template: reminder_text.clone(),
            cooldown_turns: 1,
            scope: "global".into(),
        });
        self.add_edge(
            intent_id.clone(),
            rem_id.clone(),
            GraphEdge::InjectsReminder {
                priority: if level == ReminderLevel::Mandatory {
                    95
                } else {
                    75
                },
                weight: 1.0,
                condition: None,
            },
        );

        // Add Tool Suggestions
        for tool_name in &tools_list {
            let tool_node_id = format!("tool-{tool_name}");
            self.add_node(GraphNode::Tool {
                id: tool_node_id.clone(),
                name: tool_name.clone(),
                description: format!("工具: {tool_name}"),
            });
            self.add_edge(
                intent_id.clone(),
                tool_node_id,
                GraphEdge::SuggestsTool { priority: 80 },
            );
        }

        self.rebuild_index();

        Ok(LearnedRuleSummary {
            intent_id,
            intent_name,
            triggers: triggers_disp,
            reminder_id: rem_id,
            reminder_title: rem_title,
            reminder_level: level,
            reminder_content: reminder_text,
            suggested_tools: tools_list,
            source: "manual:structured".into(),
        })
    }

    fn learn_from_natural_language(&mut self, text: &str) -> Result<LearnedRuleSummary, String> {
        let t_lower = text.to_lowercase();

        // 1. Determine Reminder Level
        let level = if text.contains("必须")
            || text.contains("强制")
            || text.contains("禁止")
            || text.contains("严禁")
            || phrase_in_text(&t_lower, "mandatory")
            || phrase_in_text(&t_lower, "must")
        {
            ReminderLevel::Mandatory
        } else if text.contains("提示") || text.contains("参考") || phrase_in_text(&t_lower, "info")
        {
            ReminderLevel::Info
        } else {
            ReminderLevel::Recommended
        };

        // 2. Split condition and action clauses
        let separators = [
            "时，",
            "时,",
            "时 ",
            "，则",
            "，就",
            "，请",
            "，必须",
            "，建议",
            "，优先",
            "，应当",
            "，务必",
            "，需要",
            " => ",
            " -> ",
            "，",
            ",",
        ];
        let mut condition = "";
        let mut action = text;
        for sep in separators {
            if let Some((c, a)) = text.split_once(sep) {
                if !c.trim().is_empty() && !a.trim().is_empty() {
                    condition = c.trim();
                    action = a.trim();
                    break;
                }
            }
        }
        if condition.is_empty() {
            condition = text;
        }

        // Clean condition
        let mut clean_cond = condition;
        for prefix in [
            "当用户要求",
            "当用户询问",
            "当用户输入",
            "当用户说",
            "当用户",
            "当遇到",
            "当涉及",
            "如果涉及",
            "如果",
            "当",
            "若",
            "针对",
            "遇到",
        ] {
            if let Some(stripped) = clean_cond.strip_prefix(prefix) {
                clean_cond = stripped.trim();
            }
        }

        // Extract known tools mentioned anywhere in the rule
        let known_tool_names = [
            "deepwiki",
            "find",
            "grep",
            "read_file",
            "search_replace",
            "write",
            "list_dir",
            "web_search",
            "web_fetch",
            "bash",
            "cargo",
            "git",
            "diff",
        ];
        let mut tools_list = Vec::new();
        for &tool in &known_tool_names {
            if t_lower.contains(tool) {
                tools_list.push(tool.to_string());
            }
        }

        let (matched_verbs, raw_objects) = extract_verbs_and_objects(clean_cond);
        let raw_objects = cap_learned_objects(raw_objects, 8);
        let allow_latin_ident = raw_objects.iter().any(|o| {
            o.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });

        let hash = stable_hash(&format!("{clean_cond}-{action}"));
        let intent_id = format!("custom-intent-{hash}");
        let trig_id = format!("custom-trig-{hash}");
        let rem_id = format!("custom-rem-{hash}");
        let intent_name = if clean_cond.chars().count() <= 16 {
            clean_cond.to_string()
        } else {
            format!("{}...", clean_cond.chars().take(15).collect::<String>())
        };

        // Create Intent Node
        self.add_node(GraphNode::Intent {
            id: intent_id.clone(),
            name: intent_name.clone(),
            description: format!("自主学习意图: {clean_cond}"),
            priority: if level == ReminderLevel::Mandatory {
                90
            } else {
                70
            },
            category: "learned".into(),
        });

        let mut triggers_disp = Vec::new();

        if !matched_verbs.is_empty() && !raw_objects.is_empty() {
            triggers_disp.push(format!(
                "[{}] + [{}]",
                matched_verbs.join(","),
                raw_objects.join(",")
            ));
            self.add_node(GraphNode::Trigger {
                id: trig_id.clone(),
                mode: MatchMode::VerbObject {
                    verbs: matched_verbs,
                    objects: raw_objects,
                    allow_latin_ident,
                },
                weight: 1.2,
                is_negative: false,
            });
            self.add_edge(
                trig_id.clone(),
                intent_id.clone(),
                GraphEdge::Triggers { weight: 1.2 },
            );
        } else {
            // Substring mode
            triggers_disp.push(clean_cond.to_string());
            self.add_node(GraphNode::Trigger {
                id: trig_id.clone(),
                mode: MatchMode::Substring {
                    phrase: clean_cond.to_string(),
                },
                weight: 1.0,
                is_negative: false,
            });
            self.add_edge(
                trig_id.clone(),
                intent_id.clone(),
                GraphEdge::Triggers { weight: 1.0 },
            );
        }

        // Create Reminder Node
        let rem_title = format!("{intent_name}指引");
        self.add_node(GraphNode::Reminder {
            id: rem_id.clone(),
            title: rem_title.clone(),
            level,
            priority: if level == ReminderLevel::Mandatory {
                95
            } else {
                75
            },
            category: "learned".into(),
            template: action.to_string(),
            cooldown_turns: 1,
            scope: "global".into(),
        });
        self.add_edge(
            intent_id.clone(),
            rem_id.clone(),
            GraphEdge::InjectsReminder {
                priority: if level == ReminderLevel::Mandatory {
                    95
                } else {
                    75
                },
                weight: 1.0,
                condition: None,
            },
        );

        // Add Tool Suggestions
        for tool_name in &tools_list {
            let tool_node_id = format!("tool-{tool_name}");
            self.add_node(GraphNode::Tool {
                id: tool_node_id.clone(),
                name: tool_name.clone(),
                description: format!("工具: {tool_name}"),
            });
            self.add_edge(
                intent_id.clone(),
                tool_node_id,
                GraphEdge::SuggestsTool { priority: 80 },
            );
        }

        self.rebuild_index();

        Ok(LearnedRuleSummary {
            intent_id,
            intent_name,
            triggers: triggers_disp,
            reminder_id: rem_id,
            reminder_title: rem_title,
            reminder_level: level,
            reminder_content: action.to_string(),
            suggested_tools: tools_list,
            source: "manual:natural_language".into(),
        })
    }

    /// Learn and update intent graph from recent interaction trajectory (user query + tool calls).
    pub fn learn_from_interaction(
        &mut self,
        query: &str,
        tools_used: &[String],
        _user_feedback: Option<&str>,
    ) -> Result<LearnedRuleSummary, String> {
        let clean_q = strip_xml_and_meta(query);
        let q = clean_q.trim();
        if q.is_empty()
            || q.starts_with("### ")
            || q.contains("Learned Tool Intent")
            || q.contains("Graph Intent Guidance")
        {
            return Err("会话交互内容为空或包含系统元标记，无法提取学习特征".into());
        }

        let (matched_verbs, raw_objects) = extract_verbs_and_objects(q);
        let raw_objects = cap_learned_objects(raw_objects, 8);
        if matched_verbs.is_empty() && raw_objects.is_empty() {
            return Err("无法从会话中提取有效的动词或对象特征，请改用 `/learn <规则文本>`".into());
        }

        let hash = stable_hash(q);
        let intent_id = format!("custom-intent-{hash}");
        let trig_id = format!("custom-trig-{hash}");
        let rem_id = format!("custom-rem-{hash}");

        let short_name: String = q.chars().take(14).collect();
        let intent_name = format!("{short_name}策略");

        self.add_node(GraphNode::Intent {
            id: intent_id.clone(),
            name: intent_name.clone(),
            description: format!("从会话轨迹学习: {q}"),
            priority: 70,
            category: "trajectory".into(),
        });

        let mut triggers_disp = Vec::new();
        let allow_latin_ident = raw_objects.iter().any(|o| {
            o.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });

        if !matched_verbs.is_empty() && !raw_objects.is_empty() {
            triggers_disp.push(format!(
                "[{}] + [{}]",
                matched_verbs.join(","),
                raw_objects.join(",")
            ));
            self.add_node(GraphNode::Trigger {
                id: trig_id.clone(),
                mode: MatchMode::VerbObject {
                    verbs: matched_verbs,
                    objects: raw_objects,
                    allow_latin_ident,
                },
                weight: 1.1,
                is_negative: false,
            });
        } else {
            let phrase = raw_objects
                .first()
                .cloned()
                .unwrap_or_else(|| short_name.clone());
            triggers_disp.push(phrase.clone());
            self.add_node(GraphNode::Trigger {
                id: trig_id.clone(),
                mode: MatchMode::Substring { phrase },
                weight: 1.0,
                is_negative: false,
            });
        }
        self.add_edge(
            trig_id,
            intent_id.clone(),
            GraphEdge::Triggers { weight: 1.1 },
        );

        let reminder_content = if !tools_used.is_empty() {
            format!(
                "针对类似【{}】的任务，优先推荐结合 `{}` 工具进行检索分析与实现验证。",
                short_name,
                tools_used.join("`, `")
            )
        } else {
            format!(
                "针对【{}】相关任务，注意结合权威文档与项目上下文进行审慎处理。",
                short_name
            )
        };

        let rem_title = format!("{short_name}策略指引");
        self.add_node(GraphNode::Reminder {
            id: rem_id.clone(),
            title: rem_title.clone(),
            level: ReminderLevel::Recommended,
            priority: 75,
            category: "trajectory".into(),
            template: reminder_content.clone(),
            cooldown_turns: 1,
            scope: "global".into(),
        });
        self.add_edge(
            intent_id.clone(),
            rem_id.clone(),
            GraphEdge::InjectsReminder {
                priority: 75,
                weight: 1.0,
                condition: None,
            },
        );

        for tool_name in tools_used {
            let tool_node_id = format!("tool-{tool_name}");
            self.add_node(GraphNode::Tool {
                id: tool_node_id.clone(),
                name: tool_name.clone(),
                description: format!("工具: {tool_name}"),
            });
            self.add_edge(
                intent_id.clone(),
                tool_node_id,
                GraphEdge::SuggestsTool { priority: 80 },
            );
        }

        self.rebuild_index();

        Ok(LearnedRuleSummary {
            intent_id,
            intent_name,
            triggers: triggers_disp,
            reminder_id: rem_id,
            reminder_title: rem_title,
            reminder_level: ReminderLevel::Recommended,
            reminder_content,
            suggested_tools: tools_used.to_vec(),
            source: "session:trajectory".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verb_object_and_alias_matching() {
        let graph = IntentGraph::with_builtin_rules();
        let params = HashMap::new();

        // 1. User says "查资料" -> matches verb "查" + object "资料"
        let res = graph.infer("我想查一下资料", &params);
        assert!(
            !res.matched_intents.is_empty(),
            "Should match RetrieveInformation"
        );
        assert!(
            res.matched_intents
                .iter()
                .any(|i| i.intent_id == "RetrieveInformation"),
            "Intent RetrieveInformation should be active"
        );

        // 2. User says "查阅开源库文档" -> matches SearchExternalDocs
        let res_docs = graph.infer("查阅开源库文档", &params);
        assert!(
            res_docs
                .matched_intents
                .iter()
                .any(|i| i.intent_id == "SearchExternalDocs"),
            "SearchExternalDocs should be active"
        );
        assert!(
            !res_docs.active_reminders.is_empty(),
            "Reminder should be present"
        );
        assert_eq!(
            res_docs.active_reminders[0].reminder_id,
            "rem-deepwiki-docs"
        );
    }

    #[test]
    fn test_veto_exclusion_edge() {
        let graph = IntentGraph::with_builtin_rules();
        let params = HashMap::new();

        // Query with veto trigger: "查看当前项目源码，不要查开源库"
        let res = graph.infer("查看当前项目源码，不要查开源库", &params);
        // SearchExternalDocs has VetoedBy trig-local-source-only ("当前项目源码")
        assert!(
            !res.matched_intents
                .iter()
                .any(|i| i.intent_id == "SearchExternalDocs"),
            "SearchExternalDocs should be vetoed by negative trigger"
        );
    }

    #[test]
    fn test_mandatory_git_safety_reminder() {
        let graph = IntentGraph::with_builtin_rules();
        let params = HashMap::new();

        let res = graph.infer("帮我执行 git reset --hard 回退", &params);
        assert!(
            res.matched_intents
                .iter()
                .any(|i| i.intent_id == "GitDestructiveAction"),
            "GitDestructiveAction should be matched"
        );
        assert_eq!(res.active_reminders.len(), 1);
        assert_eq!(res.active_reminders[0].level, ReminderLevel::Mandatory);
        assert_eq!(res.active_reminders[0].reminder_id, "rem-git-safety-guard");

        let md = res
            .render_reminder_markdown()
            .expect("Markdown should render");
        assert!(md.contains("强制规范"));
        assert!(md.contains("破坏性 Git 操作确认"));
    }

    #[test]
    fn test_multi_hop_sub_intent_inheritance() {
        let graph = IntentGraph::with_builtin_rules();
        let params = HashMap::new();

        // SearchExternalDocs is a SubIntentOf RetrieveInformation
        let res = graph.infer("这个第三方库怎么用", &params);
        assert!(res
            .matched_intents
            .iter()
            .any(|i| i.intent_id == "SearchExternalDocs"));
        // Inherited parent intent should also be in matched_intents
        assert!(res
            .matched_intents
            .iter()
            .any(|i| i.intent_id == "RetrieveInformation"));
    }

    #[test]
    fn test_json_roundtrip_and_custom_graph() {
        let mut graph = IntentGraph::new();
        graph.add_node(GraphNode::Intent {
            id: "RefactorCode".into(),
            name: "代码重构".into(),
            description: "重构模块结构".into(),
            priority: 80,
            category: "coding".into(),
        });
        graph.add_node(GraphNode::Trigger {
            id: "trig-refactor".into(),
            mode: MatchMode::Substring {
                phrase: "重构".into(),
            },
            weight: 1.0,
            is_negative: false,
        });
        graph.add_edge(
            "trig-refactor",
            "RefactorCode",
            GraphEdge::Triggers { weight: 1.0 },
        );

        graph.add_node(GraphNode::Reminder {
            id: "rem-refactor-tests".into(),
            title: "重构前运行测试".into(),
            level: ReminderLevel::Mandatory,
            priority: 90,
            category: "safety".into(),
            template: "重构目标 {{target}} 前，务必先保证现有测试全部通过。".into(),
            cooldown_turns: 0,
            scope: "project".into(),
        });
        graph.add_edge(
            "RefactorCode",
            "rem-refactor-tests",
            GraphEdge::InjectsReminder {
                priority: 90,
                weight: 1.0,
                condition: None,
            },
        );
        graph.rebuild_index();

        let json = serde_json::to_string(&graph).expect("Serialize graph");
        let restored = IntentGraph::from_json_str(&json).expect("Deserialize graph");

        let mut params = HashMap::new();
        params.insert("target".into(), "auth 模块".into());

        let res = restored.infer("我们需要重构一下这里", &params);
        assert_eq!(res.matched_intents.len(), 1);
        assert_eq!(res.matched_intents[0].intent_id, "RefactorCode");
        assert_eq!(res.active_reminders.len(), 1);
        assert!(res.active_reminders[0]
            .rendered_content
            .contains("auth 模块"));
    }

    #[test]
    fn test_empty_or_unmatched_query() {
        let graph = IntentGraph::with_builtin_rules();
        let params = HashMap::new();

        let res_empty = graph.infer("", &params);
        assert!(res_empty.matched_intents.is_empty());
        assert!(res_empty.render_reminder_markdown().is_none());

        let res_unrelated = graph.infer("今天天气真好", &params);
        assert!(res_unrelated.matched_intents.is_empty());
        assert!(res_unrelated.render_reminder_markdown().is_none());
    }

    #[test]
    fn test_learn_from_natural_language_and_infer() {
        let mut graph = IntentGraph::with_builtin_rules();
        let summary = graph
            .learn_from_text("当用户询问架构或源码实现时，建议优先使用 find 和 deepwiki 定位与查阅")
            .expect("Should learn rule from natural language");

        assert_eq!(summary.reminder_level, ReminderLevel::Recommended);
        assert!(summary.suggested_tools.contains(&"find".to_string()));
        assert!(summary.suggested_tools.contains(&"deepwiki".to_string()));

        let params = HashMap::new();
        // Query that should trigger the learned rule
        let res = graph.infer("分析 hermes 的源码架构设计与实现", &params);
        assert!(!res.matched_intents.is_empty());
        assert!(
            res.active_reminders
                .iter()
                .any(|r| r.reminder_id.starts_with("custom-rem-")),
            "learned reminder should be injected, got {:?}",
            res.active_reminders
                .iter()
                .map(|r| r.reminder_id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(res
            .suggested_tools
            .iter()
            .any(|t| t.tool_name == "deepwiki"));
    }

    #[test]
    fn test_learn_from_structured_text_mandatory() {
        let mut graph = IntentGraph::with_builtin_rules();
        let summary = graph
            .learn_from_text("意图: 数据库迁移 | 触发: 迁移,执行 + 数据库,db,migration | 提醒: 迁移数据库前必须先备份现有生产数据 | 级别: 强制 | 工具: bash")
            .expect("Should learn structured rule");

        assert_eq!(summary.reminder_level, ReminderLevel::Mandatory);
        assert_eq!(summary.intent_name, "数据库迁移");
        assert_eq!(summary.suggested_tools, vec!["bash"]);

        let params = HashMap::new();
        let res = graph.infer("执行数据库迁移脚本", &params);
        assert!(!res.matched_intents.is_empty());
        assert_eq!(res.active_reminders[0].level, ReminderLevel::Mandatory);
        assert!(res.active_reminders[0]
            .rendered_content
            .contains("备份现有生产数据"));

        let rules = graph.list_custom_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].intent_name, "数据库迁移");
    }

    #[test]
    fn test_learn_from_interaction_trajectory() {
        let mut graph = IntentGraph::with_builtin_rules();
        let tools = vec!["find".into(), "deepwiki".into()];
        let summary = graph
            .learn_from_interaction("看看hermes怎么实现记忆上下文管理的", &tools, None)
            .expect("Should learn from trajectory");

        assert!(!summary.triggers.is_empty());
        assert_eq!(summary.suggested_tools, vec!["find", "deepwiki"]);

        let params = HashMap::new();
        let res = graph.infer("分析 hermes 的上下文管理实现", &params);
        assert!(!res.matched_intents.is_empty());
        assert!(!res.active_reminders.is_empty());
    }

    #[test]
    fn test_conceptual_clarification_suppresses_search_reminders() {
        let mut graph = IntentGraph::with_builtin_rules();
        graph
            .learn_from_text("当用户询问架构或源码实现时，建议优先使用 find 和 deepwiki 定位与查阅")
            .expect("learn rule");

        let params = HashMap::new();

        // 1. Conceptual clarification query without search verbs -> should suppress tool reminders & suggestions
        let res_clarify = graph.infer(
            "我可以理解就是它是一种约束而不是实现的是吧就是一个把一些md撞到了插件里面",
            &params,
        );
        assert!(
            res_clarify.active_reminders.is_empty(),
            "Non-mandatory tool reminders should be suppressed for conceptual clarifications"
        );
        assert!(
            res_clarify.suggested_tools.is_empty(),
            "Suggested tools should be suppressed for conceptual clarifications"
        );

        // 2. Query with explicit search verb -> should trigger normally
        let res_search = graph.infer("帮我查一下架构实现", &params);
        assert!(
            !res_search.active_reminders.is_empty(),
            "Search queries should still trigger reminders"
        );
    }

    #[test]
    fn test_reject_system_reminder_meta_learning() {
        let mut graph = IntentGraph::with_builtin_rules();
        let xml_junk =
            "<system-reminder>\n### Learned Tool Intent\n- **[tool-xyz]** ...\n</system-reminder>";
        let err = graph.learn_from_text(xml_junk);
        assert!(
            err.is_err(),
            "Should reject learning from system reminder XML"
        );

        let err2 = graph.learn_from_interaction(xml_junk, &["deepwiki".into()], None);
        assert!(
            err2.is_err(),
            "Should reject learning from trajectory with system reminder XML"
        );
    }

    #[test]
    fn test_intent_golden_set() {
        let graph = IntentGraph::with_builtin_rules();
        let params = HashMap::new();

        struct Case {
            q: &'static str,
            must: &'static [&'static str],
            forbid: &'static [&'static str],
            reminder: Option<&'static str>,
            no_reminder: bool,
        }

        let cases = [
            Case {
                q: "帮我查一下 reqwest 的用法",
                must: &["SearchExternalDocs", "RetrieveInformation"],
                forbid: &[],
                reminder: Some("rem-deepwiki-docs"),
                no_reminder: false,
            },
            Case {
                q: "帮我查一下 tokio 的用法",
                must: &["SearchExternalDocs"],
                forbid: &[],
                reminder: Some("rem-deepwiki-docs"),
                no_reminder: false,
            },
            Case {
                q: "search for serde docs",
                must: &["SearchExternalDocs"],
                forbid: &[],
                reminder: Some("rem-deepwiki-docs"),
                no_reminder: false,
            },
            Case {
                q: "我想查一下资料",
                must: &["RetrieveInformation"],
                forbid: &["SearchExternalDocs", "GitDestructiveAction"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "检查一下这个仓库",
                must: &[],
                forbid: &["RetrieveInformation", "SearchExternalDocs"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "检查一下这个数据库配置",
                must: &[],
                forbid: &["RetrieveInformation", "SearchExternalDocs"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "看看这个方法对不对",
                must: &[],
                forbid: &["RetrieveInformation", "SearchExternalDocs"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "使用这个方法",
                must: &[],
                forbid: &["RetrieveInformation"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "git push --force",
                must: &["GitDestructiveAction"],
                forbid: &[],
                reminder: Some("rem-git-safety-guard"),
                no_reminder: false,
            },
            Case {
                q: "force push this branch",
                must: &["GitDestructiveAction"],
                forbid: &[],
                reminder: Some("rem-git-safety-guard"),
                no_reminder: false,
            },
            Case {
                q: "帮我强制推送",
                must: &["GitDestructiveAction"],
                forbid: &[],
                reminder: Some("rem-git-safety-guard"),
                no_reminder: false,
            },
            Case {
                q: "删除远程分支",
                must: &["GitDestructiveAction"],
                forbid: &[],
                reminder: Some("rem-git-safety-guard"),
                no_reminder: false,
            },
            Case {
                q: "跑一下测试",
                must: &["RunTestsAndVerification"],
                forbid: &["GitDestructiveAction"],
                reminder: Some("rem-test-failure-diagnosis"),
                no_reminder: false,
            },
            Case {
                q: "帮我测一下",
                must: &[],
                forbid: &["RunTestsAndVerification"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "看看新闻",
                must: &["WebFactSearch"],
                forbid: &["SearchExternalDocs"],
                reminder: Some("rem-web-fact-check"),
                no_reminder: false,
            },
            Case {
                q: "这个是不是 bug",
                must: &[],
                forbid: &["SearchExternalDocs", "WebFactSearch"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "今天天气真好",
                must: &[],
                forbid: &["RetrieveInformation", "SearchExternalDocs"],
                reminder: None,
                no_reminder: true,
            },
            Case {
                q: "分析 hermes 的源码架构",
                must: &["SearchExternalDocs"],
                forbid: &["GitDestructiveAction"],
                reminder: Some("rem-deepwiki-docs"),
                no_reminder: false,
            },
        ];

        let mut failures = Vec::new();
        for c in cases {
            let res = graph.infer(c.q, &params);
            let ids: Vec<&str> = res
                .matched_intents
                .iter()
                .map(|i| i.intent_id.as_str())
                .collect();
            for must in c.must {
                if !ids.contains(must) {
                    failures.push(format!("{:?} missing intent {must}, got {ids:?}", c.q));
                }
            }
            for forbid in c.forbid {
                if ids.contains(forbid) {
                    failures.push(format!(
                        "{:?} unexpectedly matched {forbid}, got {ids:?}",
                        c.q
                    ));
                }
            }
            if let Some(rem) = c.reminder {
                if !res.active_reminders.iter().any(|r| r.reminder_id == rem) {
                    failures.push(format!("{:?} missing reminder {rem}", c.q));
                }
            }
            if c.no_reminder && !res.active_reminders.is_empty() {
                failures.push(format!(
                    "{:?} should not inject reminders, got {:?}",
                    c.q,
                    res.active_reminders
                        .iter()
                        .map(|r| r.reminder_id.as_str())
                        .collect::<Vec<_>>()
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn test_library_entity_interpolated() {
        let graph = IntentGraph::with_builtin_rules();
        let res = graph.infer("帮我查一下 tokio 的用法", &HashMap::new());
        assert!(res.active_entities.iter().any(|e| e == "tokio"));
        assert!(res.active_reminders[0].rendered_content.contains("tokio"));
    }

    #[test]
    fn test_plan_mode_keeps_only_mandatory() {
        let graph = IntentGraph::with_builtin_rules();
        let git = graph.infer_with(
            "git push --force",
            &InferOptions {
                mandatory_only: true,
                ..InferOptions::default()
            },
        );
        assert_eq!(git.active_reminders.len(), 1);
        assert_eq!(git.active_reminders[0].level, ReminderLevel::Mandatory);
        assert!(git.suggested_tools.is_empty());

        let docs = graph.infer_with(
            "帮我查一下 tokio 的用法",
            &InferOptions {
                mandatory_only: true,
                ..InferOptions::default()
            },
        );
        assert!(docs.active_reminders.is_empty());
    }

    #[test]
    fn test_cooldown_skips_recommended_reminder() {
        let graph = IntentGraph::with_builtin_rules();
        let first = graph.infer_with(
            "帮我查一下 tokio 的用法",
            &InferOptions {
                turn_index: 1,
                ..InferOptions::default()
            },
        );
        assert!(!first.active_reminders.is_empty());

        let mut last = HashMap::new();
        last.insert("rem-deepwiki-docs".into(), 1);
        let cooled = graph.infer_with(
            "帮我查一下 tokio 的用法",
            &InferOptions {
                turn_index: 2,
                reminder_last_turn: last.clone(),
                ..InferOptions::default()
            },
        );
        assert!(
            cooled.active_reminders.is_empty(),
            "cooldown_turns=2 should suppress the next turn"
        );

        let later = graph.infer_with(
            "帮我查一下 tokio 的用法",
            &InferOptions {
                turn_index: 4,
                reminder_last_turn: last,
                ..InferOptions::default()
            },
        );
        assert!(!later.active_reminders.is_empty());
    }

    #[test]
    fn test_tool_availability_filter() {
        let graph = IntentGraph::with_builtin_rules();
        let res = graph.infer_with(
            "帮我查一下 tokio 的用法",
            &InferOptions {
                available_tools: vec!["find".into()],
                ..InferOptions::default()
            },
        );
        assert!(res.suggested_tools.iter().any(|t| t.tool_name == "find"));
        assert!(!res
            .suggested_tools
            .iter()
            .any(|t| t.tool_name == "deepwiki"));
    }

    #[test]
    fn test_conflicts_with_keeps_higher_priority() {
        let mut graph = IntentGraph::new();
        graph.add_node(GraphNode::Intent {
            id: "A".into(),
            name: "A".into(),
            description: String::new(),
            priority: 90,
            category: "x".into(),
        });
        graph.add_node(GraphNode::Intent {
            id: "B".into(),
            name: "B".into(),
            description: String::new(),
            priority: 40,
            category: "x".into(),
        });
        graph.add_node(GraphNode::Trigger {
            id: "t".into(),
            mode: MatchMode::Substring {
                phrase: "冲突样例".into(),
            },
            weight: 1.0,
            is_negative: false,
        });
        graph.add_edge("t", "A", GraphEdge::Triggers { weight: 1.0 });
        graph.add_edge("t", "B", GraphEdge::Triggers { weight: 1.0 });
        graph.add_edge("A", "B", GraphEdge::ConflictsWith);
        graph.rebuild_index();

        let res = graph.infer("冲突样例", &HashMap::new());
        assert!(res.matched_intents.iter().any(|i| i.intent_id == "A"));
        assert!(!res.matched_intents.iter().any(|i| i.intent_id == "B"));
    }
}
