//! MCP adapter (`~/.one/agent/mcp.json` + project `.one/mcp.json` chain).
//!
//! Override semantics follow the runtime exactly (`one_mcp::config::load_one_only`):
//! the user file is loaded first, then each `.one/mcp.json` from the project root
//! down to the working directory, and a later layer replaces a same-named server
//! **as a whole entry** (`McpConfig::merge`). There is no field-level merge, so the
//! studio explains that deleting an override restores inheritance from above
//! rather than partially un-editing a field.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config_studio::adapters::{DocKind, ResolvedDoc, StudioPaths, Validation};
use crate::config_studio::document::{
    Capabilities, ConfigDocument, Diagnostic, DocFormat, EffectTiming, EffectiveEntry,
    EffectiveReport, FieldKind, FieldSpec, FormModel, ModuleId, OverrideInfo, Scope,
};
use crate::config_studio::save::hash_bytes;

/// Document id for the user-level MCP file.
pub const DOC_USER: &str = "mcp.user";

/// Tool exposure modes understood by the runtime.
pub const TOOL_EXPOSURE_MODES: &[&str] = &["deferred", "direct"];

/// Top-level keys the MCP parser recognises.
const KNOWN_ROOT_KEYS: &[&str] = &[
    "mcpServers",
    "mcp_servers",
    "maxOutputBytes",
    "max_output_bytes",
    "disabledServers",
    "disabled_servers",
    "toolExposure",
    "tool_exposure",
];

/// User-level MCP file (see [`super::settings::path`] on path resolution).
///
/// `one_mcp::paths::agent_dir` resolves the same directory, so this matches the
/// file `one_mcp::config::load_one_only` actually reads.
pub fn user_path(paths: &StudioPaths) -> PathBuf {
    paths.agent_dir.join("mcp.json")
}

/// Project-level MCP file for a directory.
pub fn project_path(dir: &Path) -> PathBuf {
    dir.join(".one").join("mcp.json")
}

/// Stable document id for a project MCP file (never a client-supplied path).
pub fn project_doc_id(path: &Path) -> String {
    let digest = hash_bytes(path.display().to_string().as_bytes());
    format!("mcp.project.{}", &digest[..8])
}

/// Catalog entries for every MCP layer the runtime reads.
pub fn documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();

    let user = user_path(paths);
    let mut user_doc = ConfigDocument::new(
        DOC_USER,
        ModuleId::Mcp,
        Scope::Global,
        "mcp.json（全局）",
        user.clone(),
        DocFormat::Json,
    )
    .writable(Capabilities::EDITABLE_JSON)
    .managed_by("one-mcp config::load_one_only()")
    .override_note("最低优先级层：项目同名服务会整条替换这里的条目。")
    .precedence(0);
    user_doc.sensitive = true;
    user_doc.effect = EffectTiming::NewSession;
    user_doc.effect_note = "保存后新会话生效；Studio 不会重启正在运行的 MCP 子进程。".to_string();
    out.push(ResolvedDoc {
        doc: user_doc,
        root: paths.agent_dir.clone(),
        kind: DocKind::Mcp,
    });

    // Project layer, outermost first (matching the loader's precedence).
    let chain = paths.project_chain.clone();
    let last = chain.len().saturating_sub(1);
    for (index, dir) in chain.iter().enumerate() {
        let path = project_path(dir);
        let exists = path.is_file();
        // Only offer to create the file for the working directory itself; an
        // empty override in an ancestor would shadow nothing and just add noise.
        let is_cwd_layer = index == last;
        if !exists && !is_cwd_layer {
            continue;
        }

        let mut doc = ConfigDocument::new(
            project_doc_id(&path),
            ModuleId::Mcp,
            Scope::Project,
            if is_cwd_layer {
                "mcp.json（当前项目）".to_string()
            } else {
                format!("mcp.json（祖先目录 {}）", dir.display())
            },
            path.clone(),
            DocFormat::Json,
        )
        .managed_by("one-mcp config::load_one_only()")
        .precedence((index + 1) as u32);
        doc.project_root = Some(dir.display().to_string());
        doc.sensitive = true;
        doc.exists = exists;
        doc.effect = EffectTiming::NewSession;
        doc.effect_note = "保存后新会话生效；MCP 服务不会热重启。".to_string();

        if is_cwd_layer || exists {
            doc = doc.writable(Capabilities::EDITABLE_JSON);
            doc.override_note = Some(if exists {
                "同名服务整条替换上层配置；删除整个条目会重新继承上层（不会逐字段回退）。"
                    .to_string()
            } else {
                "文件尚不存在。保存会创建它，并放入一份完整的条目作为项目覆盖；\
                 删除该条目即可恢复继承上层配置。"
                    .to_string()
            });
        }

        out.push(ResolvedDoc {
            doc,
            root: dir.clone(),
            kind: DocKind::Mcp,
        });
    }

    out.extend(foreign_documents(paths));
    out
}

/// Read-only listing of other tools' MCP configs that the runtime would not load.
fn foreign_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let Ok(candidates) = one_mcp::config::scan_import_candidates(&paths.cwd) else {
        return Vec::new();
    };

    let mut by_path: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for candidate in candidates {
        by_path
            .entry(candidate.path.clone())
            .or_default()
            .push(candidate.name.clone());
    }

    let mut out = Vec::new();
    for (path, mut names) in by_path {
        names.sort();
        names.dedup();
        let doc = ConfigDocument::new(
            format!(
                "mcp.foreign.{}",
                &hash_bytes(path.display().to_string().as_bytes())[..8]
            ),
            ModuleId::Mcp,
            Scope::Foreign,
            format!("导入候选 · {}", path.display()),
            path.clone(),
            DocFormat::Json,
        )
        .managed_by("one-mcp config::scan_import_candidates()")
        .read_only_reason(
            "其他工具（Claude / Cursor / Codex / Grok / .mcp.json）的配置。\
             运行时默认不加载；如需使用，用 `one mcp import` 显式导入到全局 mcp.json。",
        )
        .precedence(100);
        out.push(ResolvedDoc {
            doc,
            root: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(paths.cwd.clone()),
            kind: DocKind::ReadOnly,
        });
    }
    out
}

/// Validate a draft with the runtime's own MCP parser plus extra semantics.
pub fn validate(draft: &str) -> Validation {
    let config = match one_mcp::config::parse_config_json(draft) {
        Ok(cfg) => cfg,
        Err(err) => {
            return Validation {
                diagnostics: vec![Diagnostic::error(format!("{err}"))],
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

    if let Some(map) = value.as_object() {
        // A bare server map is legal (the parser accepts it); only warn when the
        // document mixes wrapped and unwrapped shapes.
        let wrapped = map.contains_key("mcpServers") || map.contains_key("mcp_servers");
        if !wrapped {
            let looks_like_root = map.contains_key("maxOutputBytes")
                || map.contains_key("max_output_bytes")
                || map.contains_key("toolExposure");
            if looks_like_root && !map.is_empty() {
                diagnostics.push(Diagnostic::warning(
                    "文件看起来是本工具的根配置，但缺少 `mcpServers` 包裹层；\
                     运行时会把顶层键名当作服务名解析",
                ));
            }
        }
        for key in map.keys() {
            if !KNOWN_ROOT_KEYS.contains(&key.as_str()) && wrapped {
                diagnostics.push(
                    Diagnostic::warning(format!("未知顶层字段 `{key}`：运行时会忽略")).field(key),
                );
            }
        }
    }

    for (name, server) in &config.mcp_servers {
        if let Err(err) = server.validate(name) {
            diagnostics
                .push(Diagnostic::error(err.to_string()).field(format!("mcpServers.{name}")));
        }
        if !server.enabled.unwrap_or(true) {
            diagnostics.push(
                Diagnostic::info(format!("服务 `{name}` 处于禁用状态"))
                    .field(format!("mcpServers.{name}.enabled")),
            );
        }
    }

    if config.mcp_servers.is_empty() {
        diagnostics.push(Diagnostic::warning(
            "没有任何 MCP 服务：删除全部条目会清空该层配置",
        ));
    }

    for disabled in &config.disabled_servers {
        if !config.mcp_servers.contains_key(disabled) {
            diagnostics.push(
                Diagnostic::warning(format!(
                    "disabledServers 中的 `{disabled}` 在本文件中不存在；\
                     若它来自下层配置，禁用仍然生效"
                ))
                .field("disabledServers"),
            );
        }
    }

    // Surface unresolved `${VAR}` references: the value silently becomes empty
    // at load time, which is a common and confusing failure.
    let mut seen_refs: Vec<String> = Vec::new();
    if let Some(map) = value.get("mcpServers").and_then(Value::as_object) {
        for (name, entry) in map {
            for section in ["env", "headers"] {
                if let Some(obj) = entry.get(section).and_then(Value::as_object) {
                    for (key, val) in obj {
                        let Some(raw) = val.as_str() else { continue };
                        let Some(var) = env_reference_name(raw) else {
                            continue;
                        };
                        if std::env::var(&var).is_err() && !seen_refs.contains(&var) {
                            seen_refs.push(var.clone());
                            diagnostics.push(
                                Diagnostic::warning(format!(
                                    "`{name}` 的 {section}.{key} 引用未设置的变量 ${{{var}}}：\
                                     运行时展开后会是空字符串"
                                ))
                                .field(format!("mcpServers.{name}.{section}.{key}")),
                            );
                        }
                    }
                }
            }
        }
    }

    crate::config_studio::adapters::sort_diagnostics(&mut diagnostics);

    Validation {
        diagnostics,
        parsed: Some(value),
    }
}

/// Extract the variable name from `${VAR}` / `$VAR`, when the value is a reference.
fn env_reference_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if let Some(rest) = trimmed.strip_prefix("${") {
        return rest
            .split_once('}')
            .map(|(name, _)| name.to_string())
            .filter(|n| !n.is_empty());
    }
    if let Some(rest) = trimmed.strip_prefix('$') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(rest.to_string());
        }
    }
    None
}

/// Entry field schema shared by every MCP server.
pub fn server_fields() -> Vec<FieldSpec> {
    vec![
        field(
            "command",
            "command",
            FieldKind::Text,
            "stdio 传输：可执行文件（与 url 二选一）",
            false,
        ),
        field(
            "args",
            "args",
            FieldKind::StringList,
            "stdio 传输参数，每行一个",
            false,
        ),
        field(
            "url",
            "url",
            FieldKind::Text,
            "HTTP/SSE 传输地址（与 command 二选一）",
            false,
        ),
        field_enum(
            "type",
            "transport",
            &["http", "sse"],
            None,
            "传输类型提示，仅 HTTP 传输需要",
            true,
        ),
        field(
            "env",
            "env",
            FieldKind::StringMap,
            "stdio 子进程环境变量；建议写成 ${VAR} 引用而不落库明文",
            false,
        ),
        field(
            "headers",
            "headers",
            FieldKind::StringMap,
            "HTTP 请求头；值支持 ${VAR} 展开",
            false,
        ),
        field_secret(
            "authToken",
            "authToken",
            "内联 bearer token；优先改用 ${VAR} 引用",
        ),
        field(
            "bearerTokenEnvVar",
            "bearerTokenEnvVar",
            FieldKind::Text,
            "存放 token 的环境变量名（Cursor/Grok 风格）",
            false,
        ),
        field_bool("enabled", "enabled", true, "false 时该服务不启动"),
        field_number(
            "startupTimeoutSec",
            "启动超时（秒）",
            Some(json!(30)),
            Some(1.0),
            "握手超时；可由 ONE_MCP_STARTUP_TIMEOUT_SECS 覆盖",
            true,
        ),
        field_number(
            "toolTimeoutSec",
            "工具超时（秒）",
            Some(json!(120)),
            Some(1.0),
            "单次工具调用超时",
            true,
        ),
        field_list(
            "tools",
            "tools 白名单",
            "只暴露列出的工具；留空暴露全部",
            true,
        ),
        field("cwd", "cwd", FieldKind::Text, "stdio 子进程工作目录", true),
    ]
}

/// Declarative form schema for an MCP document.
pub fn form(value: &Value) -> FormModel {
    let masked_fields = crate::config_studio::mask::mask_value(value).1;
    FormModel {
        fields: vec![
            field_number(
                "maxOutputBytes",
                "单次工具结果上限（字节）",
                Some(json!(one_mcp::config::DEFAULT_MAX_OUTPUT_BYTES)),
                Some(1.0),
                "超过后截断，避免撑爆上下文",
                false,
            ),
            field_enum(
                "toolExposure",
                "工具暴露方式",
                TOOL_EXPOSURE_MODES,
                Some("deferred"),
                "deferred 只注入 search_tool/use_tool；direct 展开全部工具 schema",
                false,
            ),
            field_list(
                "disabledServers",
                "禁用的服务名",
                "只对全局 mcp.json 生效，遍历多来源合并后应用",
                false,
            ),
        ],
        collections: vec![crate::config_studio::document::CollectionSpec {
            path: "mcpServers".to_string(),
            label: "MCP 服务".to_string(),
            key_label: "服务名".to_string(),
            entry_fields: server_fields(),
            nested: None,
            key_immutable: false,
        }],
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

fn field_bool(path: &str, label: &str, default: bool, help: &str) -> FieldSpec {
    FieldSpec {
        default: Some(json!(default)),
        ..field(path, label, FieldKind::Boolean, help, false)
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

fn field_list(path: &str, label: &str, help: &str, advanced: bool) -> FieldSpec {
    field(path, label, FieldKind::StringList, help, advanced)
}

fn field_secret(path: &str, label: &str, help: &str) -> FieldSpec {
    field(path, label, FieldKind::Secret, help, false)
}

/// Resolve the effective MCP configuration with per-server provenance.
pub fn effective(paths: &StudioPaths) -> EffectiveReport {
    let mut overrides = Vec::new();
    if let Ok(value) = std::env::var("ONE_MCP_TOOL_EXPOSURE") {
        overrides.push(OverrideInfo {
            name: "ONE_MCP_TOOL_EXPOSURE".to_string(),
            value: Some(value),
            affects: "toolExposure".to_string(),
        });
    }
    if let Ok(value) = std::env::var("ONE_MCP_MERGE_FOREIGN") {
        overrides.push(OverrideInfo {
            name: "ONE_MCP_MERGE_FOREIGN".to_string(),
            value: Some(value),
            affects: "是否合并 Claude/Cursor/Codex/Grok 的 MCP 配置".to_string(),
        });
    }
    if let Ok(value) = std::env::var("ONE_MCP_STARTUP_TIMEOUT_SECS") {
        overrides.push(OverrideInfo {
            name: "ONE_MCP_STARTUP_TIMEOUT_SECS".to_string(),
            value: Some(value),
            affects: "所有服务的启动超时默认值".to_string(),
        });
    }

    match one_mcp::config::load_effective(&paths.cwd) {
        Ok(loaded) => {
            let disabled: std::collections::HashSet<&String> =
                loaded.config.disabled_servers.iter().collect();
            // A name is "overridden" only when more than one layer declared it.
            // `server_sources` records the winner for every name, so using its
            // presence here would flag every server as shadowed.
            let declarers = |name: &str| {
                loaded
                    .sources
                    .iter()
                    .filter(|s| s.server_names.iter().any(|n| n == name))
                    .count()
            };
            let entries = loaded
                .config
                .mcp_servers
                .iter()
                .map(|(name, server)| {
                    let source = loaded
                        .server_sources
                        .get(name)
                        .map(|k| k.as_str().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let transport = if server.is_stdio() { "stdio" } else { "http" };
                    let mut value = format!("{transport} · ");
                    if let Some(cmd) = &server.command {
                        value.push_str(cmd);
                    } else if let Some(url) = &server.url {
                        value.push_str(url);
                    }
                    if disabled.contains(name) {
                        value.push_str(" · 已禁用");
                    }
                    EffectiveEntry {
                        name: name.clone(),
                        source,
                        value,
                        overridden: declarers(name) > 1,
                    }
                })
                .collect();

            let sources = loaded
                .sources
                .iter()
                .map(|s| format!("{} ({})", s.path.display(), s.kind.as_str()))
                .collect();

            EffectiveReport {
                module: ModuleId::Mcp,
                note: "这里是运行时合并后的结果，只读。同名服务由更近的一层整条替换，\
                       没有字段级合并。"
                    .to_string(),
                entries,
                sources,
                overrides,
            }
        }
        Err(err) => EffectiveReport {
            module: ModuleId::Mcp,
            note: format!("加载失败：{err}"),
            entries: Vec::new(),
            sources: Vec::new(),
            overrides,
        },
    }
}

/// Studio's global-only view reads the same file exposed by its editor.
pub fn effective_global(paths: &StudioPaths) -> EffectiveReport {
    let path = user_path(paths);
    let loaded = if path.exists() {
        one_mcp::config::load_file(&path)
    } else {
        Ok(Default::default())
    };
    match loaded {
        Ok(config) => EffectiveReport {
            module: ModuleId::Mcp,
            note: "内置默认 → 全局自定义 → 最终生效。此处展示保存的全局 MCP 设置。".into(),
            entries: config
                .mcp_servers
                .iter()
                .map(|(name, server)| EffectiveEntry {
                    name: name.clone(),
                    source: "Global Override".into(),
                    value: format!(
                        "{}{}",
                        server
                            .command
                            .as_deref()
                            .or(server.url.as_deref())
                            .unwrap_or(""),
                        if server.enabled == Some(false) || config.disabled_servers.contains(name) {
                            " · 已禁用"
                        } else {
                            ""
                        }
                    ),
                    overridden: false,
                })
                .collect(),
            sources: if path.exists() {
                vec![path.display().to_string()]
            } else {
                vec![]
            },
            overrides: vec![],
        },
        Err(error) => EffectiveReport {
            module: ModuleId::Mcp,
            note: format!("加载失败：{error}"),
            entries: vec![],
            sources: vec![],
            overrides: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-config-mcp-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths_for(cwd: &Path, agent: &Path) -> StudioPaths {
        StudioPaths {
            cwd: cwd.to_path_buf(),
            agent_dir: agent.to_path_buf(),
            home: agent.to_path_buf(),
            project_chain: crate::config_studio::adapters::ancestor_chain(cwd),
        }
    }

    #[test]
    fn catalog_lists_user_and_project_layers() {
        let agent = temp_dir("agent");
        let project = temp_dir("project");
        fs::create_dir_all(project.join(".git")).unwrap();

        let docs = documents(&paths_for(&project, &agent));
        let ids: Vec<&str> = docs.iter().map(|d| d.doc.id.as_str()).collect();
        assert!(ids.contains(&DOC_USER));
        assert!(ids.iter().any(|id| id.starts_with("mcp.project.")));

        let user = docs.iter().find(|d| d.doc.id == DOC_USER).unwrap();
        assert!(user.doc.capabilities.write);
        assert!(user.doc.sensitive);
        assert_eq!(user.doc.precedence, 0);

        // The working directory layer outranks the user layer.
        let project_doc = docs.iter().find(|d| d.doc.scope == Scope::Project).unwrap();
        assert!(project_doc.doc.precedence > user.doc.precedence);
    }

    #[test]
    fn ancestor_layers_are_listed_with_real_paths() {
        let agent = temp_dir("agent-anc");
        let root = temp_dir("root-anc");
        let nested = root.join("pkg").join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        // Only the ancestor defines a project MCP file.
        fs::create_dir_all(root.join(".one")).unwrap();
        fs::write(project_path(&root), r#"{"mcpServers":{}}"#).unwrap();

        let docs = documents(&paths_for(&nested, &agent));
        let project_docs: Vec<_> = docs
            .iter()
            .filter(|d| d.doc.scope == Scope::Project)
            .collect();
        assert_eq!(project_docs.len(), 2, "ancestor + cwd placeholder");
        assert!(project_docs
            .iter()
            .any(|d| d.doc.path == project_path(&root).display().to_string()));
        // The inherited, not-yet-existing cwd layer offers creation.
        let cwd_doc = project_docs
            .iter()
            .find(|d| !d.doc.exists)
            .expect("cwd placeholder");
        assert!(cwd_doc.doc.capabilities.create);
        assert!(cwd_doc
            .doc
            .override_note
            .as_deref()
            .unwrap()
            .contains("继承"));
    }

    #[test]
    fn project_doc_id_is_stable_and_path_derived() {
        let a = PathBuf::from("/tmp/a/.one/mcp.json");
        let b = PathBuf::from("/tmp/a/.one/mcp.json");
        assert_eq!(project_doc_id(&a), project_doc_id(&b));
        assert_ne!(
            project_doc_id(&a),
            project_doc_id(Path::new("/tmp/b/.one/mcp.json"))
        );
        assert!(project_doc_id(&a).starts_with("mcp.project."));
    }

    #[test]
    fn valid_config_passes() {
        let draft = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","srv"]}}}"#;
        let v = validate(draft);
        assert!(v.parsed.is_some());
        assert!(!v.has_errors(), "unexpected: {:?}", v.diagnostics);
    }

    #[test]
    fn server_without_transport_is_rejected() {
        let v = validate(r#"{"mcpServers":{"broken":{}}}"#);
        assert!(v.has_errors());
        assert!(v.diagnostics[0].message.contains("broken"));
    }

    #[test]
    fn server_with_both_transports_is_rejected() {
        let v = validate(r#"{"mcpServers":{"both":{"command":"x","url":"http://y"}}}"#);
        assert!(v.has_errors());
    }

    #[test]
    fn broken_json_is_rejected() {
        let v = validate("{ not json }");
        assert!(v.has_errors());
        assert!(v.parsed.is_none());
    }

    #[test]
    fn unresolved_env_reference_warns() {
        let draft =
            r#"{"mcpServers":{"s":{"command":"x","env":{"K":"${ONE_CONFIG_TEST_UNSET_VAR}"}}}}"#;
        let v = validate(draft);
        assert!(!v.has_errors());
        assert!(v
            .diagnostics
            .iter()
            .any(|d| d.message.contains("ONE_CONFIG_TEST_UNSET_VAR")));
    }

    #[test]
    fn resolved_env_reference_does_not_warn() {
        std::env::set_var("ONE_CONFIG_TEST_SET_VAR", "value");
        let draft =
            r#"{"mcpServers":{"s":{"command":"x","env":{"K":"${ONE_CONFIG_TEST_SET_VAR}"}}}}"#;
        let v = validate(draft);
        assert!(
            !v.diagnostics
                .iter()
                .any(|d| d.message.contains("ONE_CONFIG_TEST_SET_VAR")),
            "{:?}",
            v.diagnostics
        );
        std::env::remove_var("ONE_CONFIG_TEST_SET_VAR");
    }

    #[test]
    fn disabled_servers_are_reported() {
        let draft = r#"{"mcpServers":{"s":{"command":"x","enabled":false}}}"#;
        let v = validate(draft);
        assert!(v.diagnostics.iter().any(|d| d.message.contains("禁用")));
    }

    #[test]
    fn form_describes_servers_collection() {
        let value = json!({"mcpServers":{"fs":{"command":"npx","env":{"K":"${V}"}}}});
        let model = form(&value);
        assert_eq!(model.collections.len(), 1);
        let coll = &model.collections[0];
        assert_eq!(coll.path, "mcpServers");
        let paths: Vec<&str> = coll.entry_fields.iter().map(|f| f.path.as_str()).collect();
        for expected in ["command", "args", "url", "env", "headers", "enabled"] {
            assert!(paths.contains(&expected), "missing {expected}");
        }
        let root_paths: Vec<&str> = model.fields.iter().map(|f| f.path.as_str()).collect();
        assert!(root_paths.contains(&"maxOutputBytes"));
        assert!(root_paths.contains(&"toolExposure"));
    }

    #[test]
    fn effective_reports_the_nearest_project_layer_as_the_source() {
        let agent = temp_dir("eff-agent");
        let root = temp_dir("eff-root");
        let nested = root.join("pkg").join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".one")).unwrap();
        fs::create_dir_all(nested.join(".one")).unwrap();

        // Unique names so a developer's real user-level mcp.json cannot shadow
        // this fixture (the user layer is process-global and not injectable).
        fs::write(
            project_path(&root),
            r#"{"mcpServers":{"one-studio-test-shared":{"command":"root-cmd"},"one-studio-test-root-only":{"command":"r"}}}"#,
        )
        .unwrap();
        fs::write(
            project_path(&nested),
            r#"{"mcpServers":{"one-studio-test-shared":{"command":"nested-cmd"}}}"#,
        )
        .unwrap();

        let report = effective(&paths_for(&nested, &agent));
        let shared = report
            .entries
            .iter()
            .find(|e| e.name == "one-studio-test-shared")
            .expect("shared server resolved");
        assert!(shared.value.contains("nested-cmd"), "{}", shared.value);
        assert_eq!(shared.source, "one-project");
        assert!(report
            .entries
            .iter()
            .any(|e| e.name == "one-studio-test-root-only"));

        // `overridden` must mean "a lower layer also declared this name", not
        // merely "we know where it came from" — otherwise every server would
        // look shadowed and the flag would carry no information.
        let root_only = report
            .entries
            .iter()
            .find(|e| e.name == "one-studio-test-root-only")
            .expect("root-only server resolved");
        assert!(shared.overridden, "a name in two layers is shadowed");
        assert!(
            !root_only.overridden,
            "a name in one layer is not shadowed: {:?}",
            root_only
        );

        // Both ancestor and cwd project files are reported as real sources.
        let project_sources: Vec<&String> = report
            .sources
            .iter()
            .filter(|s| s.contains("one-project"))
            .collect();
        assert_eq!(project_sources.len(), 2, "{:?}", report.sources);
        assert!(project_sources
            .iter()
            .any(|s| s.contains(&project_path(&root).display().to_string())));
    }
}
