//! Settings adapter (`~/.one/agent/settings.json`).
//!
//! `settings.json` is **global only**: no project-level override exists in the
//! runtime, so the studio does not invent one (`docs/web-config.md` §2).
//!
//! Drafts are validated by deserializing into the real [`crate::settings::Settings`]
//! type, which is exactly what `settings::load()` does at runtime. Unknown keys
//! are reported as warnings rather than errors because the loader ignores them —
//! but a silent typo is a real bug, so it must be visible.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::config_studio::adapters::{DocKind, ResolvedDoc, StudioPaths, Validation};
use crate::config_studio::document::{
    Capabilities, ConfigDocument, Diagnostic, DocFormat, EffectTiming, FieldKind, FieldSpec,
    FormModel, ModuleId, Scope,
};
use crate::settings::Settings;

/// Document id for the global settings file.
pub const DOC_ID: &str = "settings.global";

/// Keys the runtime loader recognises, including serde aliases.
const KNOWN_KEYS: &[&str] = &[
    "provider",
    "model",
    "thinking",
    "auto_approve",
    "permissionMode",
    "permission_mode",
    "context_window",
    "sandbox",
    "additional_directories",
    "permissions",
    "bash_sandbox",
    "skills_config",
    "features",
    "tool_output",
    "compaction",
    "memory",
    "empty_response_retries",
    "maxTurns",
    "max_turns",
    "enabledModels",
    "enabled_models",
    "batchExploration",
    "batch_exploration",
];

/// Allowed values for `permissionMode` (see `settings::set_key`).
pub const PERMISSION_MODES: &[&str] = &[
    "default",
    "ask",
    "acceptEdits",
    "auto",
    "dontAsk",
    "bypassPermissions",
];

/// Allowed values for `sandbox` (`one_tools::SandboxMode::parse`).
pub const SANDBOX_MODES: &[&str] = &["workspace-write", "full-access"];

/// Allowed values for `thinking`.
pub const THINKING_LEVELS: &[&str] = &["off", "low", "medium", "high"];

/// Feature ids the studio exposes as first-class toggles.
pub const FEATURE_TOGGLES: &[(&str, &str)] = &[
    ("subagent", "Subagent (task / spawn_subagent)"),
    ("server_search", "Server search"),
    ("memory", "Memory"),
];

/// Settings file path.
///
/// Derived from [`StudioPaths`], which mirrors the loaders' own resolution
/// (`ONE_AGENT_DIR` / `ONE_DATA_DIR` / `$HOME/.one/agent`). Keeping one
/// injection point means the studio cannot display a path the runtime would not
/// read, and tests can run without mutating process-global state.
pub fn path(paths: &StudioPaths) -> PathBuf {
    paths.agent_dir.join("settings.json")
}

/// Catalog entry for `settings.json`.
pub fn documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let file = path(paths);
    let doc = ConfigDocument::new(
        DOC_ID,
        ModuleId::Settings,
        Scope::Global,
        "settings.json",
        file.clone(),
        DocFormat::Json,
    )
    .writable(Capabilities::EDITABLE_JSON)
    .managed_by("one-cli settings::load() / settings::save()")
    .override_note(
        "settings.json 只有全局一份：运行时没有项目级覆盖规则，\
         因此不存在项目层可写入口。",
    )
    .precedence(0);

    let mut doc = doc;
    doc.effect = EffectTiming::NewSession;
    doc.effect_note = "保存后对新会话生效；模型、权限、压缩等运行中会话不受影响。".to_string();

    vec![ResolvedDoc {
        doc,
        root: paths.agent_dir.clone(),
        kind: DocKind::Settings,
    }]
}

/// Validate a draft against the real settings parser plus cross-field rules.
pub fn validate(draft: &str) -> Validation {
    let parsed: Settings = match serde_json::from_str(draft) {
        Ok(s) => s,
        Err(err) => {
            let mut diagnostic = Diagnostic::error(format!("JSON 解析失败：{err}"));
            if err.line() > 0 && err.column() > 0 {
                diagnostic = diagnostic.at(err.line(), err.column());
            } else {
                diagnostic = locate_offset(draft, diagnostic);
            }
            return Validation {
                diagnostics: vec![diagnostic],
                parsed: None,
            };
        }
    };

    let value: Value = match serde_json::from_str(draft) {
        Ok(v) => v,
        Err(err) => {
            return Validation {
                diagnostics: vec![Diagnostic::error(format!("JSON 解析失败：{err}"))],
                parsed: None,
            }
        }
    };

    let mut diagnostics = Vec::new();

    // Unknown top-level keys are silently dropped by the runtime; surface them.
    if let Some(map) = value.as_object() {
        for key in map.keys() {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                diagnostics.push(
                    Diagnostic::warning(format!(
                        "未知字段 `{key}`：运行时会忽略它（可能是拼写错误）"
                    ))
                    .field(key.clone()),
                );
            }
        }
    }

    if let Some(mode) = settings_required_choice(&parsed.permission_mode, PERMISSION_MODES) {
        diagnostics.push(
            Diagnostic::error(format!(
                "permissionMode 取值 `{mode}` 无效，可选：{}",
                PERMISSION_MODES.join(" | ")
            ))
            .field("permissionMode"),
        );
    }
    if let Some(mode) = settings_required_choice(&parsed.sandbox, SANDBOX_MODES) {
        diagnostics.push(
            Diagnostic::error(format!(
                "sandbox 取值 `{mode}` 无效，可选：{}",
                SANDBOX_MODES.join(" | ")
            ))
            .field("sandbox"),
        );
    }
    if let Some(level) = settings_required_choice(&parsed.thinking, THINKING_LEVELS) {
        diagnostics.push(
            Diagnostic::error(format!(
                "thinking 取值 `{level}` 无效，可选：{}",
                THINKING_LEVELS.join(" | ")
            ))
            .field("thinking"),
        );
    }

    if let Some(compaction) = &parsed.compaction {
        if let Some(ratio) = compaction.ratio {
            if !(ratio.is_finite() && ratio > 0.0 && ratio <= 1.0) {
                diagnostics.push(
                    Diagnostic::error("compaction.ratio 必须在 (0, 1] 之间")
                        .field("compaction.ratio"),
                );
            }
        }
        if let Some(threshold) = compaction.threshold {
            if threshold == 0 {
                diagnostics.push(
                    Diagnostic::error("compaction.threshold 必须大于 0")
                        .field("compaction.threshold"),
                );
            }
        }
        if let Some(keep) = compaction.keep_recent {
            if keep == 0 {
                diagnostics.push(
                    Diagnostic::error("compaction.keep_recent 必须大于 0")
                        .field("compaction.keep_recent"),
                );
            }
        }
        if let Some(lead) = compaction.prefire_lead_ratio {
            if !(lead.is_finite() && lead > 0.0 && lead < 1.0) {
                diagnostics.push(
                    Diagnostic::error("compaction.prefire_lead_ratio 必须在 (0, 1) 之间")
                        .field("compaction.prefire_lead_ratio"),
                );
            }
        }
        if compaction.two_pass.unwrap_or(false) {
            diagnostics.push(Diagnostic::info(
                "已启用 two_pass：会额外产生 Pass-1 摘要请求与后台预触发",
            ));
        }
    }

    if let Some(memory) = &parsed.memory {
        if let Some(sub) = &memory.subagent {
            if crate::protocol::MemoryResourceMode::parse(sub).is_none() {
                diagnostics.push(
                    Diagnostic::error("memory.subagent 只支持 off | index")
                        .field("memory.subagent"),
                );
            }
        }
        if let Some(max) = memory.index_max_lines {
            if max == 0 {
                diagnostics.push(
                    Diagnostic::error("memory.index_max_lines 必须大于 0")
                        .field("memory.index_max_lines"),
                );
            }
        }
    }

    if let Some(dirs) = &parsed.additional_directories {
        for (i, dir) in dirs.iter().enumerate() {
            if dir.trim().is_empty() {
                diagnostics.push(
                    Diagnostic::error("additional_directories 不允许为空字符串")
                        .field(format!("additional_directories[{i}]")),
                );
            }
        }
    }

    for (i, rule) in parsed.batch_exploration.iter().enumerate() {
        if rule.provider.trim().is_empty() || rule.model.trim().is_empty() {
            diagnostics.push(
                Diagnostic::error("batchExploration 的 provider 和 model 不能为空")
                    .field(format!("batchExploration[{i}]")),
            );
        }
        if rule.after_single_reads == 0 {
            diagnostics.push(
                Diagnostic::error("batchExploration.afterSingleReads 必须大于 0")
                    .field(format!("batchExploration[{i}].afterSingleReads")),
            );
        }
    }

    if let Some(features) = &parsed.features {
        for (id, _) in features {
            if !crate::runtime::features::FEATURE_REGISTRY
                .iter()
                .any(|f| f.id == id)
                && id != crate::runtime::features::FEATURE_MEMORY_LEGACY
            {
                diagnostics.push(
                    Diagnostic::warning(format!("未知功能开关 `{id}`：运行时会忽略"))
                        .field(format!("features.{id}")),
                );
            }
        }
    }

    crate::config_studio::adapters::sort_diagnostics(&mut diagnostics);

    Validation {
        diagnostics,
        parsed: Some(value),
    }
}

/// Return the offending value when an optional enum field is set but invalid.
fn settings_required_choice(value: &Option<String>, allowed: &[&str]) -> Option<String> {
    let raw = value.as_ref()?;
    let trimmed = raw.trim();
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(trimmed)) {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn locate_offset(draft: &str, diagnostic: Diagnostic) -> Diagnostic {
    // serde_json reports 0/0 for some IO-level errors; point at the file start.
    let _ = draft;
    diagnostic.at(1, 1)
}

/// Declarative field schema for the settings form.
pub fn fields() -> Vec<FieldSpec> {
    let tool_defaults = one_tools::ToolOutputLimits::default();

    let mut fields = vec![
        field(
            "provider",
            "Provider",
            FieldKind::Text,
            "当前使用的 provider id（如 openai-codex / xai / opencode）",
        ),
        field(
            "model",
            "模型",
            FieldKind::Text,
            "模型 id；留空使用 provider 默认",
        ),
        field_enum(
            "thinking",
            "思考等级",
            THINKING_LEVELS,
            Some("medium"),
            "传给 provider 的 reasoning effort",
        ),
        field_enum(
            "permissionMode",
            "权限模式",
            PERMISSION_MODES,
            Some("default"),
            "工具审批策略；bypassPermissions 等于始终批准",
        ),
        field_bool(
            "auto_approve",
            "自动批准 bash",
            false,
            "跳过 bash 危险命令确认（等价于旧的 auto_approve）",
        ),
        field_enum(
            "sandbox",
            "路径沙箱",
            SANDBOX_MODES,
            Some("workspace-write"),
            "workspace-write 限制在 workspace 内；full-access 关闭边界",
        ),
        field_bool(
            "bash_sandbox",
            "bash 使用 bubblewrap",
            true,
            "workspace-write 下用 bwrap 隔离 bash",
        ),
        field_number(
            "context_window",
            "上下文窗口覆盖",
            None,
            Some(1000.0),
            "仅用于页脚显示百分比；留空按模型元数据",
        ),
        field_number(
            "maxTurns",
            "每轮最大步数",
            Some(json!(0)),
            Some(0.0),
            "0 表示不限制",
        ),
        field_number(
            "empty_response_retries",
            "空回复重试次数",
            Some(json!(one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES)),
            Some(0.0),
            "空白回复或临时故障的额外采样次数",
        ),
        field_list(
            "enabledModels",
            "模型切换器白名单",
            "留空显示全部目录模型；每行一个 provider:id",
        ),
        field_list(
            "additional_directories",
            "额外可访问目录",
            "每行一个绝对路径，等价于 --add-dir",
        ),
    ];

    fields.push(field_number(
        "tool_output.max_lines",
        "工具输出行数上限",
        Some(json!(tool_defaults.max_lines)),
        Some(1.0),
        "超过后写入 ~/.one/agent/tool-outputs/ 并只给预览",
    ));
    fields.push(field_number(
        "tool_output.max_bytes",
        "工具输出字节上限",
        Some(json!(tool_defaults.max_bytes)),
        Some(1.0),
        "超过后同样落盘，避免撑爆上下文",
    ));

    fields.push(field_bool(
        "compaction.auto",
        "自动压缩",
        true,
        "接近上限时在回合前自动压缩",
    ));
    fields.push(field_number(
        "compaction.ratio",
        "压缩触发比例",
        Some(json!(one_core::compaction::DEFAULT_COMPACT_RATIO)),
        Some(0.01),
        "占上下文窗口的比例，0–1",
    ));
    fields.push(field_number(
        "compaction.keep_recent",
        "保留最近用户轮次",
        Some(json!(one_core::compaction::DEFAULT_KEEP_RECENT_TURNS)),
        Some(1.0),
        "压缩后原样保留的最近用户轮数",
    ));
    fields.push(field_bool(
        "compaction.prune",
        "裁剪旧工具结果",
        false,
        "按用户轮次年龄裁剪旧工具正文",
    ));
    fields.push(field_bool(
        "compaction.two_pass",
        "两遍压缩",
        false,
        "Pass-1 笔记 + Pass-2 终稿，并允许后台预触发",
    ));

    // Advanced compaction knobs stay available but collapsed by default.
    fields.push(
        FieldSpec {
            ..field_number(
                "compaction.threshold",
                "绝对 token 阈值",
                None,
                Some(0.0),
                "设置后优先于 ratio",
            )
        }
        .into_advanced(),
    );
    fields.push(
        FieldSpec {
            ..field_number(
                "compaction.prune_keep_last_n_turns",
                "不裁剪的最近轮次",
                Some(json!(3)),
                Some(0.0),
                "最近 N 个用户轮次的工具结果永不裁剪",
            )
        }
        .into_advanced(),
    );
    fields.push(
        FieldSpec {
            ..field_number(
                "compaction.prune_soft_trim_threshold",
                "软裁剪字符阈值",
                Some(json!(4000)),
                Some(0.0),
                "超过该长度的旧工具结果转占位符",
            )
        }
        .into_advanced(),
    );
    fields.push(
        FieldSpec {
            ..field_number(
                "compaction.prune_hard_clear_age_turns",
                "硬清除轮次年龄",
                Some(json!(10)),
                Some(1.0),
                "超过该年龄的工具结果全部变成占位符",
            )
        }
        .into_advanced(),
    );
    fields.push(
        FieldSpec {
            ..field_number(
                "compaction.prefire_lead_ratio",
                "预触发提前量",
                Some(json!(one_core::compaction::DEFAULT_PREFIRE_LEAD_RATIO)),
                Some(0.01),
                "two_pass 下提前启动 Pass-1 的比例",
            )
        }
        .into_advanced(),
    );

    fields.push(field_bool(
        "memory.enabled",
        "记忆总开关",
        true,
        "注入 L2 目录并启用记忆工具",
    ));
    fields.push(field_number(
        "memory.index_max_lines",
        "L2 目录条目上限",
        Some(json!(one_resources::DEFAULT_INDEX_MAX_LINES)),
        Some(1.0),
        "系统提示中注入的最大条目数",
    ));
    fields.push(field_bool(
        "memory.write",
        "允许写入记忆",
        true,
        "允许 agent 在记忆根目录写文件",
    ));
    fields.push(field_number(
        "memory.max_lookups_per_turn",
        "每轮记忆查询上限",
        Some(json!(one_resources::DEFAULT_MAX_LOOKUPS_PER_TURN)),
        Some(0.0),
        "限制每题读取记忆的次数",
    ));
    fields.push(field_enum(
        "memory.subagent",
        "子代理记忆模式",
        &["off", "index"],
        Some("off"),
        "子代理默认关闭记忆以保持上下文干净",
    ));
    fields.push(field_bool(
        "memory.archive_compaction",
        "压缩摘要归档到 L4",
        true,
        "把压缩摘要写入 memory/sessions/",
    ));

    for (id, label) in FEATURE_TOGGLES {
        fields.push(field_bool(
            &format!("features.{id}"),
            label,
            default_feature_enabled(id),
            "功能包开关；影响模型上下文的项在 /new 后生效",
        ));
    }

    fields
}

fn default_feature_enabled(id: &str) -> bool {
    crate::runtime::features::FEATURE_REGISTRY
        .iter()
        .find(|f| f.id == id)
        .map(|f| f.default_enabled)
        .unwrap_or(true)
}

fn field(path: &str, label: &str, kind: FieldKind, help: &str) -> FieldSpec {
    FieldSpec {
        path: path.to_string(),
        label: label.to_string(),
        kind,
        help: Some(help.to_string()),
        options: Vec::new(),
        default: None,
        min: None,
        max: None,
        advanced: false,
    }
}

fn field_bool(path: &str, label: &str, default: bool, help: &str) -> FieldSpec {
    FieldSpec {
        default: Some(json!(default)),
        ..field(path, label, FieldKind::Boolean, help)
    }
}

fn field_number(
    path: &str,
    label: &str,
    default: Option<Value>,
    min: Option<f64>,
    help: &str,
) -> FieldSpec {
    FieldSpec {
        default,
        min,
        max: None,
        ..field(path, label, FieldKind::Number, help)
    }
}

fn field_enum(
    path: &str,
    label: &str,
    options: &[&str],
    default: Option<&str>,
    help: &str,
) -> FieldSpec {
    FieldSpec {
        options: options.iter().map(|o| (*o).to_string()).collect(),
        default: default.map(|d| json!(d)),
        ..field(path, label, FieldKind::Enum, help)
    }
}

fn field_list(path: &str, label: &str, help: &str) -> FieldSpec {
    field(path, label, FieldKind::StringList, help)
}

/// Helper for marking a spec as advanced from a functional-update literal.
trait IntoAdvanced {
    fn into_advanced(self) -> Self;
}

impl IntoAdvanced for FieldSpec {
    fn into_advanced(mut self) -> Self {
        self.advanced = true;
        self
    }
}

/// Build the settings form model over the masked document value.
pub fn form(value: &Value) -> FormModel {
    let masked_fields = crate::config_studio::mask::mask_value(value).1;
    FormModel {
        fields: fields(),
        collections: Vec::new(),
        value: value.clone(),
        masked_fields,
    }
}

/// Whether the parsed value is a JSON object (settings must be one).
pub fn ensure_object(value: &Value) -> Result<&Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "settings.json 的顶层必须是 JSON 对象".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> StudioPaths {
        StudioPaths {
            cwd: PathBuf::from("/tmp/project"),
            agent_dir: PathBuf::from("/tmp/agent"),
            home: PathBuf::from("/tmp/home"),
            project_chain: vec![PathBuf::from("/tmp/project")],
        }
    }

    #[test]
    fn catalog_exposes_one_global_document() {
        let docs = documents(&paths());
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc.doc.id, DOC_ID);
        assert_eq!(doc.doc.scope, Scope::Global);
        assert!(doc.doc.capabilities.write);
        assert!(doc.doc.override_note.as_deref().unwrap().contains("全局"));
    }

    #[test]
    fn valid_draft_passes_without_diagnostics() {
        let draft = r#"{"provider":"xai","model":"grok-4","thinking":"high"}"#;
        let v = validate(draft);
        assert!(v.parsed.is_some(), "diagnostics: {:?}", v.diagnostics);
        assert!(!v.has_errors(), "unexpected errors: {:?}", v.diagnostics);
    }

    #[test]
    fn broken_json_reports_position() {
        let v = validate("{\n  \"provider\": ,\n}\n");
        assert!(v.has_errors());
        assert!(v.parsed.is_none());
        assert!(v.diagnostics[0].line.is_some());
    }

    #[test]
    fn unknown_keys_warn_but_do_not_block() {
        let v = validate(r#"{"provder":"typo"}"#);
        assert!(!v.has_errors());
        assert!(v.diagnostics.iter().any(|d| d.message.contains("未知字段")));
    }

    #[test]
    fn invalid_enums_are_rejected() {
        let v = validate(r#"{"permissionMode":"yolo"}"#);
        assert!(v.has_errors());
        let v = validate(r#"{"sandbox":"none"}"#);
        assert!(v.has_errors());
        let v = validate(r#"{"thinking":"ultra"}"#);
        assert!(v.has_errors());
    }

    #[test]
    fn compaction_ranges_are_checked() {
        assert!(validate(r#"{"compaction":{"ratio":0}}"#).has_errors());
        assert!(validate(r#"{"compaction":{"ratio":1.5}}"#).has_errors());
        assert!(validate(r#"{"compaction":{"threshold":0}}"#).has_errors());
        assert!(validate(r#"{"compaction":{"prefire_lead_ratio":1}}"#).has_errors());
        assert!(!validate(r#"{"compaction":{"ratio":0.8}}"#).has_errors());
    }

    #[test]
    fn batch_exploration_rules_are_validated() {
        assert!(!validate(r#"{"batchExploration":[{"provider":"cpa","model":"gemini-3.8-flash-high","thinkingLevel":"medium","afterSingleReads":4}]}"#).has_errors());
        assert!(validate(
            r#"{"batchExploration":[{"provider":"cpa","model":"x","afterSingleReads":0}]}"#
        )
        .has_errors());
        assert!(validate(
            r#"{"batchExploration":[{"provider":"","model":"x","afterSingleReads":4}]}"#
        )
        .has_errors());
        assert!(validate(r#"{"batchExploration":[{"provider":"cpa","model":"x","thinkingLevel":"invalid","afterSingleReads":4}]}"#).has_errors());
    }

    #[test]
    fn memory_subagent_accepts_only_off_or_index() {
        assert!(validate(r#"{"memory":{"subagent":"bogus"}}"#).has_errors());
        assert!(!validate(r#"{"memory":{"subagent":"index"}}"#).has_errors());
    }

    #[test]
    fn empty_additional_directory_is_rejected() {
        assert!(validate(r#"{"additional_directories":[""]}"#).has_errors());
        assert!(!validate(r#"{"additional_directories":["/tmp/x"]}"#).has_errors());
    }

    #[test]
    fn form_covers_major_settings_groups() {
        let value = json!({"provider":"xai","compaction":{"ratio":0.7},"memory":{"enabled":true}});
        let model = form(&value);
        let paths: Vec<&str> = model.fields.iter().map(|f| f.path.as_str()).collect();
        for expected in [
            "provider",
            "model",
            "permissionMode",
            "sandbox",
            "tool_output.max_lines",
            "compaction.auto",
            "compaction.ratio",
            "memory.enabled",
            "features.subagent",
        ] {
            assert!(paths.contains(&expected), "missing field {expected}");
        }
        assert_eq!(model.value["provider"], "xai");
    }

    #[test]
    fn feature_defaults_come_from_the_registry() {
        let features = crate::runtime::features::FEATURE_REGISTRY;
        for (id, _) in FEATURE_TOGGLES {
            let expected = features
                .iter()
                .find(|f| f.id == *id)
                .unwrap()
                .default_enabled;
            assert_eq!(default_feature_enabled(id), expected);
        }
    }
}
