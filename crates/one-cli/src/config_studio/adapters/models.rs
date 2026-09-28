//! Models & providers adapter (`~/.one/agent/models.json`).
//!
//! Drafts are validated with `one_ai::try_parse_models_file`, the same function
//! the runtime uses, so trailing-comma tolerance and `includeDefaults` semantics
//! behave identically. `models.json` is global-only in the runtime.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config_studio::adapters::{DocKind, ResolvedDoc, StudioPaths, Validation};
use crate::config_studio::document::{
    Capabilities, CollectionSpec, ConfigDocument, Diagnostic, DocFormat, EffectTiming,
    EffectiveEntry, EffectiveReport, FieldKind, FieldSpec, FormModel, ModuleId, OverrideInfo,
    Scope,
};
use crate::config_studio::mask::is_env_reference;

/// Document id for `models.json`.
pub const DOC_ID: &str = "models.global";

/// Wire protocols understood by the runtime (`one_ai::ProviderApi`).
pub const WIRE_APIS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
    "gemini-generate-content",
];

/// Top-level keys recognised by the models loader.
const KNOWN_ROOT_KEYS: &[&str] = &["includeDefaults", "include_defaults", "providers", "models"];

/// Path of `models.json` (see [`super::settings::path`] on path resolution).
pub fn path(paths: &StudioPaths) -> PathBuf {
    paths.agent_dir.join("models.json")
}

/// Catalog entry for `models.json`.
pub fn documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let file = path(paths);
    let mut doc = ConfigDocument::new(
        DOC_ID,
        ModuleId::Providers,
        Scope::Global,
        "models.json",
        file,
        DocFormat::Json,
    )
    .writable(Capabilities::EDITABLE_JSON)
    .managed_by("one-cli provider::load_config() → one-ai models_file")
    .override_note(
        "models.json 只有全局一份。includeDefaults 为 true（默认）时，\
         文件内容叠加在内置模型目录之上；写 CRUD 后运行时会写入 false 的快照。",
    )
    .precedence(0);
    doc.sensitive = true;
    doc.effect = EffectTiming::NewSession;
    doc.effect_note = "保存后新会话生效；当前会话的 provider / model 不会被切换。".to_string();

    vec![ResolvedDoc {
        doc,
        root: paths.agent_dir.clone(),
        kind: DocKind::Models,
    }]
}

/// Validate a draft with the runtime's models parser plus extra semantics.
pub fn validate(draft: &str) -> Validation {
    if let Err(err) = one_ai::try_parse_models_file(draft) {
        return Validation {
            diagnostics: vec![Diagnostic::error(err)],
            parsed: None,
        };
    }

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

    if let Some(map) = value.as_object() {
        for key in map.keys() {
            if !KNOWN_ROOT_KEYS.contains(&key.as_str()) {
                diagnostics.push(
                    Diagnostic::warning(format!("未知顶层字段 `{key}`：运行时会忽略")).field(key),
                );
            }
        }
    }

    if let Some(providers) = value.get("providers").and_then(Value::as_object) {
        for (id, entry) in providers {
            if !entry.is_object() {
                diagnostics.push(
                    Diagnostic::error(format!("providers.{id} 必须是对象"))
                        .field(format!("providers.{id}")),
                );
                continue;
            }
            for key in ["api", "providerType"] {
                if let Some(raw) = entry.get(key).and_then(Value::as_str) {
                    if !WIRE_APIS.iter().any(|a| a.eq_ignore_ascii_case(raw)) {
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "providers.{id}.{key} 取值 `{raw}` 无法识别，可用：{}",
                                WIRE_APIS.join(" | ")
                            ))
                            .field(format!("providers.{id}.{key}")),
                        );
                    }
                }
            }
            if let Some(base) = entry.get("baseUrl").and_then(Value::as_str) {
                if !base.starts_with("http://") && !base.starts_with("https://") {
                    diagnostics.push(
                        Diagnostic::warning(format!(
                            "providers.{id}.baseUrl 不是 http(s) 地址：`{base}`"
                        ))
                        .field(format!("providers.{id}.baseUrl")),
                    );
                }
            }
            if let Some(key) = entry.get("apiKey").and_then(Value::as_str) {
                if !is_env_reference(key) && !key.is_empty() {
                    diagnostics.push(
                        Diagnostic::warning(format!(
                            "providers.{id}.apiKey 是明文凭据，会以明文写入磁盘；\
                             建议改成 ${{{id_upper}_API_KEY}} 形式的环境变量引用",
                            id_upper = id.to_uppercase().replace('-', "_")
                        ))
                        .field(format!("providers.{id}.apiKey")),
                    );
                }
            }
            let models_empty = entry
                .get("models")
                .and_then(Value::as_array)
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if models_empty {
                diagnostics.push(Diagnostic::warning(format!(
                    "providers.{id} 没有定义任何模型：该 provider 不会出现在 /model 切换器中"
                )));
            }
        }
    }

    if value.get("providers").is_none() && value.get("models").is_none() {
        diagnostics.push(Diagnostic::warning(
            "文件既没有 `providers` 也没有 `models`：运行时将只使用内置模型目录",
        ));
    }

    crate::config_studio::adapters::sort_diagnostics(&mut diagnostics);

    Validation {
        diagnostics,
        parsed: Some(value),
    }
}

/// Field schema for one provider entry.
fn provider_fields() -> Vec<FieldSpec> {
    vec![
        field_enum(
            "api",
            "wire api",
            WIRE_APIS,
            None,
            "与 providerType 等价；决定请求协议",
            false,
        ),
        field(
            "baseUrl",
            "baseUrl",
            FieldKind::Text,
            "兼容端点根地址，如 https://api.openai.com/v1",
            false,
        ),
        field_secret(
            "apiKey",
            "apiKey",
            "建议写成 ${ENV_VAR} 引用；明文会写入磁盘",
        ),
        field_enum(
            "providerType",
            "providerType",
            WIRE_APIS,
            None,
            "旧字段：仅当 api 未设置时生效",
            true,
        ),
    ]
}

/// Field schema for one model entry inside a provider.
fn model_fields() -> Vec<FieldSpec> {
    vec![
        field(
            "id",
            "id",
            FieldKind::Text,
            "模型 id，传给 provider 的值",
            false,
        ),
        field(
            "name",
            "name",
            FieldKind::Text,
            "显示名；缺省等于 id",
            false,
        ),
        field_number(
            "context_window",
            "context_window",
            None,
            Some(1.0),
            "上下文窗口 token 数，用于压缩阈值与页脚百分比",
            false,
        ),
        field_enum(
            "api",
            "api",
            WIRE_APIS,
            None,
            "覆盖 provider 级 wire api",
            true,
        ),
        field(
            "baseUrl",
            "baseUrl",
            FieldKind::Text,
            "覆盖 provider 级 baseUrl",
            true,
        ),
        field_bool(
            "reasoning",
            "reasoning",
            false,
            "该模型是否支持扩展思考",
            true,
        ),
        FieldSpec {
            path: "quirks".to_string(),
            label: "行为倾向 (Quirks)".to_string(),
            kind: FieldKind::StringList,
            help: Some(
                "模型行为倾向修正位：over_planning, stops_early, reluctant_to_use_tools, weak_verification"
                    .to_string(),
            ),
            options: vec![
                "over_planning".to_string(),
                "stops_early".to_string(),
                "reluctant_to_use_tools".to_string(),
                "weak_verification".to_string(),
            ],
            default: None,
            min: None,
            max: None,
            advanced: false,
        },
    ]
}

/// Field schema for the legacy flat `models` array.
fn flat_model_fields() -> Vec<FieldSpec> {
    vec![
        field(
            "provider",
            "provider",
            FieldKind::Text,
            "provider id",
            false,
        ),
        field("id", "id", FieldKind::Text, "模型 id", false),
        field("name", "name", FieldKind::Text, "显示名", false),
        field_number(
            "context_window",
            "context_window",
            None,
            Some(1.0),
            "上下文窗口 token 数",
            true,
        ),
        field_enum("api", "api", WIRE_APIS, None, "wire api", true),
        field(
            "base_url",
            "base_url",
            FieldKind::Text,
            "兼容端点地址",
            true,
        ),
        field_secret("api_key", "api_key", "API key（建议用 ${VAR} 引用）"),
        FieldSpec {
            path: "quirks".to_string(),
            label: "行为倾向 (Quirks)".to_string(),
            kind: FieldKind::StringList,
            help: Some(
                "模型行为倾向修正位：over_planning, stops_early, reluctant_to_use_tools, weak_verification"
                    .to_string(),
            ),
            options: vec![
                "over_planning".to_string(),
                "stops_early".to_string(),
                "reluctant_to_use_tools".to_string(),
                "weak_verification".to_string(),
            ],
            default: None,
            min: None,
            max: None,
            advanced: false,
        },
    ]
}

/// Declarative form schema for `models.json`.
pub fn form(value: &Value) -> FormModel {
    let masked_fields = crate::config_studio::mask::mask_value(value).1;
    FormModel {
        fields: vec![field_bool(
            "includeDefaults",
            "includeDefaults",
            true,
            "true 时文件叠加在内置模型目录之上；false 表示文件即完整快照",
            false,
        )],
        collections: vec![
            CollectionSpec {
                path: "providers".to_string(),
                label: "Providers".to_string(),
                key_label: "provider id".to_string(),
                entry_fields: provider_fields(),
                nested: Some(Box::new(CollectionSpec {
                    path: "models".to_string(),
                    label: "模型".to_string(),
                    key_label: "id".to_string(),
                    entry_fields: model_fields(),
                    nested: None,
                    key_immutable: false,
                })),
                key_immutable: false,
            },
            CollectionSpec {
                path: "models".to_string(),
                label: "遗留扁平模型列表（建议改用 providers）".to_string(),
                key_label: "index".to_string(),
                entry_fields: flat_model_fields(),
                nested: None,
                key_immutable: false,
            },
        ],
        value: value.clone(),
        masked_fields,
    }
}

fn field(path: &str, label: &str, kind: FieldKind, help: &str, advanced: bool) -> FieldSpec {
    FieldSpec {
        path: path.to_string(),
        label: label.to_string(),
        kind,
        help: Some(help.to_string()),
        options: Vec::new(),
        default: None,
        min: None,
        max: None,
        advanced,
    }
}

fn field_bool(path: &str, label: &str, default: bool, help: &str, advanced: bool) -> FieldSpec {
    FieldSpec {
        default: Some(json!(default)),
        ..field(path, label, FieldKind::Boolean, help, advanced)
    }
}

fn field_number(
    path: &str,
    label: &str,
    default: Option<Value>,
    min: Option<f64>,
    help: &str,
    advanced: bool,
) -> FieldSpec {
    FieldSpec {
        default,
        min,
        max: None,
        ..field(path, label, FieldKind::Number, help, advanced)
    }
}

fn field_enum(
    path: &str,
    label: &str,
    options: &[&str],
    default: Option<&str>,
    help: &str,
    advanced: bool,
) -> FieldSpec {
    FieldSpec {
        options: options.iter().map(|o| (*o).to_string()).collect(),
        default: default.map(|d| json!(d)),
        ..field(path, label, FieldKind::Enum, help, advanced)
    }
}

fn field_secret(path: &str, label: &str, help: &str) -> FieldSpec {
    field(path, label, FieldKind::Secret, help, false)
}

/// Resolve the effective provider/model catalog with provenance.
pub fn effective(paths: &StudioPaths) -> EffectiveReport {
    let file = path(paths);
    let raw = std::fs::read_to_string(&file).ok();
    let file_providers: Vec<String> = raw
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|v| {
            v.get("providers")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
        })
        .unwrap_or_default();

    let config = one_ai::load_models_file(&file);

    let mut entries: Vec<EffectiveEntry> = Vec::new();
    let mut overrides: Vec<OverrideInfo> = Vec::new();

    for provider in &config.providers {
        let from_file = file_providers.contains(&provider.id);
        let model_count = config.registry.list_by_provider(&provider.id).len();
        let models_with_quirks: Vec<String> = config
            .registry
            .list_by_provider(&provider.id)
            .iter()
            .filter(|m| !m.quirks.is_empty())
            .map(|m| {
                let q = m
                    .quirks
                    .iter()
                    .map(|q| q.as_str())
                    .collect::<Vec<_>>()
                    .join("+");
                format!("{}({})", m.id, q)
            })
            .collect();
        let mut value = String::new();
        if let Some(base) = &provider.base_url {
            value.push_str(base);
        } else {
            value.push_str("(内置默认端点)");
        }
        if let Some(api) = &provider.api {
            value.push_str(&format!(" · {}", api.as_str()));
        }
        value.push_str(&format!(" · {model_count} 个模型"));
        if !models_with_quirks.is_empty() {
            value.push_str(&format!(" · quirks: {}", models_with_quirks.join(", ")));
        }

        // Report whether a credential reference actually resolves.
        if let Some(raw_key) = &provider.api_key_raw {
            if let Some(var) = env_var_name(raw_key) {
                let resolved = std::env::var(&var).is_ok();
                overrides.push(OverrideInfo {
                    name: var.clone(),
                    value: if resolved {
                        Some("已设置".to_string())
                    } else {
                        None
                    },
                    affects: format!("provider `{}` 的凭据", provider.id),
                });
                if !resolved {
                    value.push_str(&format!(" · 凭据 ${{{var}}} 未设置"));
                }
            }
        }

        entries.push(EffectiveEntry {
            name: provider.id.clone(),
            source: if from_file {
                file.display().to_string()
            } else {
                "内置默认目录".to_string()
            },
            value,
            overridden: provider.api_key_raw.is_some(),
        });
    }

    if let Ok(dir) = std::env::var("ONE_AGENT_DIR") {
        overrides.push(OverrideInfo {
            name: "ONE_AGENT_DIR".to_string(),
            value: Some(dir),
            affects: "整份 models.json 的位置".to_string(),
        });
    }

    let note = format!(
        "来自 {} 的 provider 与模型目录（只读）。\
         网络可达性、认证结果与真实推理测试属于诊断阶段，本页不做连通性判断。",
        file.display()
    );

    EffectiveReport {
        module: ModuleId::Providers,
        note,
        entries,
        sources: vec![file.display().to_string(), "内置模型目录".to_string()],
        overrides,
    }
}

/// Extract `${VAR}` / `$VAR` name from a credential reference.
fn env_var_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("${") {
        return rest
            .split_once('}')
            .map(|(n, _)| n.to_string())
            .filter(|n| !n.is_empty());
    }
    if let Some(rest) = trimmed.strip_prefix('$') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(rest.to_string());
        }
    }
    None
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
    fn catalog_has_one_global_sensitive_document() {
        let docs = documents(&paths());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].doc.id, DOC_ID);
        assert_eq!(docs[0].doc.module, ModuleId::Providers);
        assert!(docs[0].doc.sensitive);
        assert!(docs[0].doc.capabilities.write);
    }

    #[test]
    fn valid_provider_document_passes() {
        let draft = r#"{
            "includeDefaults": true,
            "providers": {
                "openai": {
                    "baseUrl": "https://api.openai.com/v1",
                    "api": "openai-responses",
                    "apiKey": "${OPENAI_API_KEY}",
                    "models": [{"id": "gpt-4o", "name": "GPT-4o", "context_window": 128000}]
                }
            }
        }"#;
        let v = validate(draft);
        assert!(v.parsed.is_some(), "{:?}", v.diagnostics);
        assert!(!v.has_errors(), "{:?}", v.diagnostics);
    }

    #[test]
    fn invalid_json_is_rejected() {
        let v = validate("{ oops");
        assert!(v.has_errors());
        assert!(v.parsed.is_none());
    }

    #[test]
    fn unknown_wire_api_is_rejected() {
        let draft = r#"{"providers":{"x":{"api":"carrier-pigeon","models":[{"id":"a"}]}}}"#;
        let v = validate(draft);
        assert!(v.has_errors());
        assert!(v
            .diagnostics
            .iter()
            .any(|d| d.message.contains("carrier-pigeon")));
    }

    #[test]
    fn literal_api_key_warns_but_passes() {
        let draft = r#"{"providers":{"x":{"baseUrl":"https://x","apiKey":"sk-live-123","models":[{"id":"a"}]}}}"#;
        let v = validate(draft);
        assert!(!v.has_errors());
        assert!(v.diagnostics.iter().any(|d| d.message.contains("明文")));
    }

    #[test]
    fn provider_without_models_warns() {
        let draft = r#"{"providers":{"x":{"baseUrl":"https://x"}}}"#;
        let v = validate(draft);
        assert!(v
            .diagnostics
            .iter()
            .any(|d| d.message.contains("没有定义任何模型")));
    }

    #[test]
    fn non_http_base_url_warns() {
        let draft = r#"{"providers":{"x":{"baseUrl":"ftp://x","models":[{"id":"a"}]}}}"#;
        let v = validate(draft);
        assert!(v.diagnostics.iter().any(|d| d.message.contains("baseUrl")));
    }

    #[test]
    fn form_exposes_providers_with_nested_models() {
        let value = json!({"providers":{"x":{"models":[{"id":"a"}]}}});
        let model = form(&value);
        assert_eq!(model.collections.len(), 2);
        let providers = &model.collections[0];
        assert_eq!(providers.path, "providers");
        let nested = providers.nested.as_ref().expect("nested models collection");
        assert_eq!(nested.path, "models");
        let entry_paths: Vec<&str> = providers
            .entry_fields
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(entry_paths.contains(&"api"));
        assert!(entry_paths.contains(&"baseUrl"));
        assert!(entry_paths.contains(&"apiKey"));
        let nested_paths: Vec<&str> = nested
            .entry_fields
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(nested_paths.contains(&"id"));
        assert!(nested_paths.contains(&"context_window"));
    }

    #[test]
    fn env_var_name_parses_references() {
        assert_eq!(env_var_name("${FOO}"), Some("FOO".to_string()));
        assert_eq!(env_var_name("$FOO"), Some("FOO".to_string()));
        assert_eq!(env_var_name("sk-literal"), None);
    }
}
