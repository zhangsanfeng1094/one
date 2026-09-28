//! Read-only adapters: everything the studio can show but not write in P1.
//!
//! `docs/web-config.md` §2 requires the catalog to cover *all* configuration
//! sources and to show real paths, real provenance, and why an item is not
//! editable — rather than hiding read-only resources or offering fake controls.
//! Built-in and third-party resources are created the way the runtime already
//! supports (`one agent`, skill installs), so the studio only reports them.
//!
//! Runtime data — sessions, caches, logs, memory bodies — is deliberately out of
//! scope and never appears here.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config_studio::adapters::{DocKind, ResolvedDoc, StudioPaths};
use crate::config_studio::document::{ConfigDocument, DocFormat, ModuleId, Scope};
use crate::config_studio::save::hash_bytes;

/// Depth limit for recursive `SKILL.md` discovery (mirrors `one-resources`).
const MAX_SKILL_DEPTH: u32 = 5;

/// Every read-only document the studio knows about.
pub fn documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();
    out.extend(agent_documents(paths));
    out.extend(agent_instruction_documents(paths));
    out.extend(prompt_documents(paths));
    out.extend(skill_documents(paths));
    out.extend(plugin_documents(paths));
    out.extend(integration_documents(paths));
    out
}

/// Read-only documents rooted at the agent home.
fn integration_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();
    let agent = &paths.agent_dir;

    // Extension registry + hooks: both read from the agent home.
    for (name, who) in [
        (
            "extensions.json",
            "one-ext loader::discover()（扩展注册表）",
        ),
        ("hooks.json", "one-ext hooks::load_hooks()（生命周期钩子）"),
    ] {
        let path = agent.join(name);
        let mut doc = ConfigDocument::new(
            format!("extensions.{}", name.replace('.', "_")),
            ModuleId::Extensions,
            Scope::Global,
            name,
            path.clone(),
            DocFormat::Json,
        )
        .managed_by(who)
        .read_only_reason(
            "P1 仅展示。扩展与钩子的写入会在 P2 阶段接入共享写入服务（需要校验脚本路径与权限边界）。",
        )
        .precedence(10);
        if !path.is_file() {
            doc.exists = false;
            doc.read_only_reason = Some(format!("文件尚不存在：{}", path.display()));
        }
        out.push(ResolvedDoc {
            doc,
            root: agent.clone(),
            kind: DocKind::ReadOnly,
        });
    }

    let hooks_dir = agent.join("hooks").join("hooks.json");
    if hooks_dir.is_file() {
        let doc = ConfigDocument::new(
            format!(
                "extensions.hooks{}",
                &hash_bytes(hooks_dir.display().to_string().as_bytes())[..6]
            ),
            ModuleId::Extensions,
            Scope::Global,
            "hooks/hooks.json",
            hooks_dir.clone(),
            DocFormat::Json,
        )
        .managed_by("one-ext hooks::load_hooks()")
        .read_only_reason("P1 仅展示。")
        .precedence(11);
        out.push(ResolvedDoc {
            doc,
            root: agent.clone(),
            kind: DocKind::ReadOnly,
        });
    }

    // Bot gateway configuration (global + project).
    for (path, label) in [
        (paths.home.join(".one").join("bot.toml"), "bot.toml（全局）"),
        (paths.cwd.join("bot.toml"), "bot.toml（项目）"),
        (paths.cwd.join("bot.json"), "bot.json（项目）"),
    ] {
        if !path.is_file() {
            continue;
        }
        let doc = ConfigDocument::new(
            format!(
                "extensions.bot.{}",
                &hash_bytes(path.display().to_string().as_bytes())[..8]
            ),
            ModuleId::Extensions,
            if path.starts_with(&paths.cwd) {
                Scope::Project
            } else {
                Scope::Global
            },
            label,
            path.clone(),
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                DocFormat::Toml
            } else {
                DocFormat::Json
            },
        )
        .managed_by("one-bot config::load()")
        .read_only_reason("Bot 凭据与通道配置；P1 仅展示，避免误改导致网关无法启动。")
        .sensitive(true)
        .precedence(20);
        out.push(ResolvedDoc {
            doc,
            root: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| paths.cwd.clone()),
            kind: DocKind::ReadOnly,
        });
    }

    // Credential store: always shown masked.
    let auth = agent.join("auth.json");
    let mut doc = ConfigDocument::new(
        "extensions.auth",
        ModuleId::Extensions,
        Scope::Global,
        "auth.json（凭据库）",
        auth.clone(),
        DocFormat::Json,
    )
    .managed_by("one-ai auth::storage::AuthStorage")
    .read_only_reason(
        "OAuth / API 凭据由登录流程管理（one login / one logout）。\
         本页只读且默认脱敏，Studio 不提供直接写入，避免破坏刷新令牌。",
    )
    .sensitive(true)
    .precedence(1);
    if !auth.is_file() {
        doc.exists = false;
    }
    out.push(ResolvedDoc {
        doc,
        root: agent.clone(),
        kind: DocKind::ReadOnly,
    });

    // User-taught intent graph rules (configuration, not runtime data).
    let custom = agent.join("intent_graph").join("custom.json");
    if custom.is_file() {
        let doc = ConfigDocument::new(
            "extensions.intent_graph",
            ModuleId::Extensions,
            Scope::Global,
            "intent_graph/custom.json（自定义规则）",
            custom.clone(),
            DocFormat::Json,
        )
        .managed_by("one-resources intent_graph 加载器")
        .read_only_reason("通过 `one learn` 教学与重置；P1 仅展示。")
        .precedence(30);
        out.push(ResolvedDoc {
            doc,
            root: agent.clone(),
            kind: DocKind::ReadOnly,
        });
    }

    out
}

/// Agent spec files the runtime would load, plus the embedded presets.
fn agent_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();

    // `runtime::presets::agent_search_dirs` order: project first, then user.
    let dirs = [
        (
            paths.cwd.join(".one").join("agents"),
            Scope::Project,
            "项目",
        ),
        (paths.agent_dir.join("agents"), Scope::Global, "全局"),
    ];

    for (dir, scope, label) in dirs {
        for path in list_files(&dir, &["json", "md"]) {
            let doc = ConfigDocument::new(
                format!(
                    "agents.file.{}",
                    &hash_bytes(path.display().to_string().as_bytes())[..8]
                ),
                ModuleId::Agents,
                scope,
                format!(
                    "{} 定义 · {}",
                    label,
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("agent")
                ),
                path.clone(),
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    DocFormat::Markdown
                } else {
                    DocFormat::Json
                },
            )
            .managed_by("one-cli runtime::presets::load_spec_file()")
            .read_only_reason(
                "Agent 规格（工具权限、模型、资源引用）。P1 仅展示与校验；\
                 新增请直接写入该目录，P2 会提供表单编辑。",
            )
            .precedence(if scope == Scope::Project { 0 } else { 1 });
            out.push(ResolvedDoc {
                doc,
                root: dir.clone(),
                kind: DocKind::ReadOnly,
            });
        }
    }

    // Embedded presets are compiled in, so they have no file to edit.
    for (name, id) in [
        ("explore", "explore"),
        ("plan", "plan"),
        ("general", "general"),
        ("main / default", "main"),
    ] {
        let mut doc = ConfigDocument::new(
            format!("agents.builtin.{id}"),
            ModuleId::Agents,
            Scope::Builtin,
            format!("内置预设 · {name}"),
            PathBuf::from(format!("(内置: builtin_{id})")),
            DocFormat::Json,
        )
        .managed_by("one-cli protocol::AgentSpec::builtin_*()")
        .read_only_reason("编译进二进制的预设。要覆盖它，请在同名目录放置项目或全局定义文件。")
        .precedence(100);
        doc.exists = true;
        out.push(ResolvedDoc {
            doc,
            root: paths.cwd.clone(),
            kind: DocKind::ReadOnly,
        });
    }

    out
}

/// `AGENTS.md` / `CLAUDE.md` instruction files merged into the system prompt.
fn agent_instruction_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();

    let mut candidates: Vec<(PathBuf, Scope)> = Vec::new();
    let global = paths.agent_dir.join("AGENTS.md");
    if global.is_file() {
        candidates.push((global, Scope::Global));
    }
    for dir in &paths.project_chain {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if path.is_file() {
                candidates.push((path, Scope::Project));
            }
        }
    }

    for (path, scope) in candidates {
        let doc = ConfigDocument::new(
            format!(
                "agents.instructions.{}",
                &hash_bytes(path.display().to_string().as_bytes())[..8]
            ),
            ModuleId::Agents,
            scope,
            format!(
                "指令文件 · {}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("AGENTS.md")
            ),
            path.clone(),
            DocFormat::Markdown,
        )
        .managed_by("one-resources agents::load_agents_files()")
        .read_only_reason("AGENTS.md 会被原样合并进系统提示。P1 仅展示；编辑请直接改文件。")
        .precedence(if scope == Scope::Project { 0 } else { 1 });
        out.push(ResolvedDoc {
            doc,
            root: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| paths.cwd.clone()),
            kind: DocKind::ReadOnly,
        });
    }

    out
}

/// Prompt-related configuration: built-in rules, discovered templates, DSL
/// files, and what each agent actually references.
///
/// Four distinct sources feed prompts, and a module that showed only one of them
/// would be silently incomplete:
///
/// | source | where it lives | who reads it |
/// | --- | --- | --- |
/// | built-in presets/components | compiled into the binary | `one-prompt::builtin::registry()` |
/// | slash-command templates | `<agent_dir>/prompts`, `<cwd>/.one/prompts` (`.md` only) | `one-resources::prompts::discover_prompts` |
/// | DSL specs | referenced by an agent spec's `prompt.file`, relative to the declaring file | `PromptConfig::load` → `PromptSpec::load` |
/// | agent selection | inside an agent spec's `prompt` block | `PromptConfig::load` |
fn prompt_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();

    out.extend(builtin_prompt_documents());
    out.extend(prompt_template_documents(paths));
    out.extend(agent_prompt_documents(paths));

    out
}

/// The component registry and its presets, rendered from the live registry.
fn builtin_prompt_documents() -> Vec<ResolvedDoc> {
    let registry = one_prompt::ComponentRegistry::with_builtins();
    let mut out = Vec::new();

    // One index so the available building blocks are discoverable at all.
    let mut index = String::from("# 内置提示词组件（one-prompt 组件注册表）\n\n");
    index.push_str("这些组件编译进二进制，没有对应文件。\n\n");
    index.push_str("| 组件 id | slot | 条件 |\n| --- | --- | --- |\n");
    for id in registry.component_ids() {
        let Some(component) = registry.component(id) else {
            continue;
        };
        for slot in &component.slots {
            let mut conditions = Vec::new();
            if !slot.when.capabilities.is_empty() {
                conditions.push(format!("需工具 {}", slot.when.capabilities.join("+")));
            }
            if !slot.when.modes.is_empty() {
                conditions.push(format!("模式 {}", slot.when.modes.join("|")));
            }
            index.push_str(&format!(
                "| `{id}` | `{}` | {} |\n",
                slot.id,
                if conditions.is_empty() {
                    "—".to_string()
                } else {
                    conditions.join("、")
                }
            ));
        }
    }
    let mut registry_doc = ConfigDocument::new(
        "prompts.builtin.registry",
        ModuleId::Prompts,
        Scope::Builtin,
        "内置组件注册表",
        PathBuf::from("(内置: one-prompt::builtin::registry)"),
        DocFormat::Markdown,
    )
    .managed_by("one-prompt ComponentRegistry::with_builtins()")
    .read_only_reason(
        "组件正文编译进二进制。要改提示词，请用 Agent 规格里的 `prompt` 块覆盖对应 slot，\
             而不是修改源码。",
    )
    .precedence(100)
    .builtin_content(index);
    // Content is compiled in, so there is nothing missing even though no file
    // backs it.
    registry_doc.exists = true;
    out.push(ResolvedDoc {
        doc: registry_doc,
        root: PathBuf::new(),
        kind: DocKind::ReadOnly,
    });

    // Each preset, expanded into its components with bodies — this is the
    // "规则 + 正文" a user needs to reason about before overriding a slot.
    for name in registry.preset_names() {
        let mut text =
            format!("# 内置预设 `{name}`\n\n来源：编译进二进制的 one-prompt 组件注册表。\n\n");
        let mut used = Vec::new();
        if let Some(ids) = registry.preset_components(name) {
            text.push_str("## 组装顺序\n\n");
            for (i, id) in ids.iter().enumerate() {
                text.push_str(&format!("{}. `{id}`\n", i + 1));
            }
            text.push_str("\n## 组件正文\n");
            for id in ids {
                let Some(component) = registry.component(id) else {
                    text.push_str(&format!(
                        "\n### `{id}`\n\n**引用错误**：注册表中不存在该组件。\n"
                    ));
                    continue;
                };
                used.push(format!("`{id}`"));
                for slot in &component.slots {
                    text.push_str(&format!("\n### `{id}` → slot `{}`\n\n", slot.id));
                    if !slot.when.capabilities.is_empty() {
                        text.push_str(&format!(
                            "条件：需工具 {}\n\n",
                            slot.when.capabilities.join("+")
                        ));
                    }
                    if !slot.when.modes.is_empty() {
                        text.push_str(&format!("条件：模式 {}\n\n", slot.when.modes.join("|")));
                    }
                    match &slot.body.text {
                        Some(body) => {
                            text.push_str("```text\n");
                            text.push_str(body);
                            if !body.ends_with('\n') {
                                text.push('\n');
                            }
                            text.push_str("```\n");
                        }
                        None => text.push_str("（该组件从此处读取正文文件）\n"),
                    }
                }
            }
        }

        // Drift guard: listing a preset whose slots have vanished would hide a
        // real regression, so say so instead of rendering a partial view.
        let missing: Vec<String> = ids_missing(&registry, name);
        if !missing.is_empty() {
            text.push_str(&format!(
                "\n**引用错误**：预设引用了未注册的组件 {}。\n",
                missing
                    .iter()
                    .map(|id| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }

        let mut preset_doc = ConfigDocument::new(
            format!("prompts.builtin.preset.{name}"),
            ModuleId::Prompts,
            Scope::Builtin,
            format!("内置预设 · {name}"),
            PathBuf::from(format!("(内置: one-prompt preset {name})")),
            DocFormat::Markdown,
        )
        .managed_by("one-prompt ComponentRegistry::preset()")
        .read_only_reason(
            "预设本身不可编辑；在 Agent 规格的 `prompt` 块里用 preset + operations 覆盖。",
        )
        .override_note(format!("包含 {} 个组件。", used.len()))
        .precedence(100)
        .builtin_content(text);
        preset_doc.exists = true;
        out.push(ResolvedDoc {
            doc: preset_doc,
            root: PathBuf::new(),
            kind: DocKind::ReadOnly,
        });
    }

    out
}

/// Component ids a preset names but the registry does not define.
fn ids_missing(registry: &one_prompt::ComponentRegistry, preset: &str) -> Vec<String> {
    registry
        .preset_components(preset)
        .map(|ids| {
            ids.iter()
                .filter(|id| registry.component(id).is_none())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Slash-command templates. `discover_prompts` reads `.md` only, so the studio
/// must not advertise `.toml` files it would never load.
fn prompt_template_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();
    // Same pair, same order, as `ResourceLoader::discover`.
    let dirs = [
        (paths.agent_dir.join("prompts"), Scope::Global, "全局"),
        (
            paths.cwd.join(".one").join("prompts"),
            Scope::Project,
            "项目",
        ),
    ];

    for (dir, scope, label) in dirs {
        for path in list_files(&dir, &["md"]) {
            let doc = ConfigDocument::new(
                format!("prompts.template.{}", short_id(&path)),
                ModuleId::Prompts,
                scope,
                format!(
                    "{}提示词模板 · {}",
                    label,
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("prompt")
                ),
                path.clone(),
                DocFormat::Markdown,
            )
            .managed_by("one-resources prompts::discover_prompts()")
            .read_only_reason(
                "斜杠命令模板（以文件名作为命令名）。P1 只展示来源与内容；编辑请直接改文件。",
            )
            .precedence(if scope == Scope::Project { 0 } else { 1 });
            out.push(ResolvedDoc {
                doc,
                root: dir.clone(),
                kind: DocKind::ReadOnly,
            });
        }
    }

    out
}

/// What each agent spec asks for, plus the DSL files it points at.
///
/// This is where 引用关系 and 引用错误 become visible: a `prompt.preset` that no
/// longer exists, or a `prompt.file` that was moved, currently fails only when
/// that agent is spawned.
fn agent_prompt_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();
    let registry = one_prompt::ComponentRegistry::with_builtins();
    let mut seen_specs: BTreeSet<PathBuf> = BTreeSet::new();

    for (path, scope, label) in agent_spec_files(paths) {
        let spec = match crate::runtime::presets::load_spec_file(&path) {
            Ok(spec) => spec,
            Err(err) => {
                out.push(ResolvedDoc {
                    doc: ConfigDocument::new(
                        format!("prompts.ref.unreadable.{}", short_id(&path)),
                        ModuleId::Prompts,
                        scope,
                        format!("{}提示词引用 · {}（无法解析）", label, file_name(&path)),
                        path.clone(),
                        DocFormat::Markdown,
                    )
                    .managed_by("one-cli runtime::presets::load_spec_file()")
                    .read_only_reason("Agent 规格本身无法解析，因此无法读取其提示词配置。")
                    .override_note(format!("**引用错误**：{}", err.message))
                    .precedence(50)
                    .builtin_content(format!(
                        "# 无法解析 Agent 规格\n\n`{}`\n\n**引用错误**：{}\n",
                        path.display(),
                        err.message
                    )),
                    root: path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    kind: DocKind::ReadOnly,
                });
                continue;
            }
        };

        // Resolve exactly the way the runtime does: `prompt.directory` is bound
        // to the declaring file, so relative `file:` paths resolve identically.
        let resolution = spec.prompt.load(&paths.cwd);

        let agent_name = spec
            .name
            .clone()
            .unwrap_or_else(|| file_stem(&path).to_string());

        let mut text = format!("# Agent `{agent_name}` 的提示词配置\n\n");
        text.push_str(&format!("声明于 `{}`\n\n", path.display()));
        text.push_str("## 选择方式\n\n");
        match (&spec.prompt.preset, &spec.prompt.file, &spec.prompt.spec) {
            (Some(preset), _, _) => {
                let known = registry.has_preset(preset);
                text.push_str(&format!(
                    "- `prompt.preset = \"{preset}\"` {}\n",
                    if known {
                        "（内置预设，存在）"
                    } else {
                        "**引用错误**：注册表中没有这个预设"
                    }
                ));
            }
            (_, Some(file), _) => {
                let resolved = spec
                    .prompt
                    .directory
                    .clone()
                    .unwrap_or_else(|| paths.cwd.clone())
                    .join(file);
                text.push_str(&format!(
                    "- `prompt.file = \"{}\"` → `{}` {}\n",
                    file.display(),
                    resolved.display(),
                    if resolved.is_file() {
                        "（存在）"
                    } else {
                        "**引用错误**：文件不存在"
                    }
                ));
            }
            (_, _, Some(_)) => text.push_str("- `prompt.spec`：内联 DSL\n"),
            _ => text.push_str("- 未声明选择方式，运行时使用默认预设 `code`\n"),
        }

        text.push_str("\n## 覆盖操作（operations）\n\n");
        if spec.prompt.operations.is_empty() {
            text.push_str("（无）\n");
        } else {
            for (i, op) in spec.prompt.operations.iter().enumerate() {
                text.push_str(&format!("{}. slot `{}` · {:?}\n", i + 1, op.slot, op.op));
                if let Some(body) = op.body.as_ref().and_then(|b| b.text.as_deref()) {
                    text.push_str("\n```text\n");
                    text.push_str(body);
                    if !body.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str("```\n\n");
                }
            }
        }

        text.push_str("\n## 解析结果\n\n");
        match &resolution {
            Ok(spec_out) => {
                // `PromptConfig::load` does not validate the preset name —
                // `compile` does, at spawn time. Check the registry here so the
                // error surfaces while the user is looking at the file.
                let known = registry.has_preset(&spec_out.preset);
                text.push_str(&format!(
                    "- 基础预设：`{}`{}\n",
                    spec_out.preset,
                    if known {
                        ""
                    } else {
                        " — **引用错误**：注册表中没有这个预设，生成该 Agent 时会失败"
                    }
                ));
                text.push_str(&format!("- 生效组件数：{}\n", spec_out.components.len()));
            }
            Err(err) => {
                text.push_str(&format!(
                    "**引用错误**（运行时同样会失败）：{}\n",
                    err.message
                ));
            }
        }

        out.push(ResolvedDoc {
            doc: ConfigDocument::new(
                format!("prompts.ref.{}", short_id(&path)),
                ModuleId::Prompts,
                scope,
                format!("{}提示词引用 · {agent_name}", label),
                path.clone(),
                DocFormat::Markdown,
            )
            .managed_by("one-cli prompt_config::PromptConfig::load()")
            .read_only_reason(
                "提示词选择写在 Agent 规格里。P1 只展示引用关系与引用错误；\
                 编辑请改对应的 Agent 定义文件。",
            )
            .override_note(match &resolution {
                // `PromptConfig::load` resolves bodies but does not validate the
                // preset name — `compile` does, at spawn time. The studio must
                // not report "resolvable" for a preset that will fail then.
                Ok(spec_out) if registry.has_preset(&spec_out.preset) => "引用可解析。".to_string(),
                Ok(spec_out) => format!(
                    "**引用错误**：预设 `{}` 未注册，生成该 Agent 时会失败。",
                    spec_out.preset
                ),
                Err(err) => format!("**引用错误**：{}", err.message),
            })
            .precedence(50)
            .builtin_content(text),
            root: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            kind: DocKind::ReadOnly,
        });

        // The DSL file itself, when the agent points at one.
        if let Some(file) = &spec.prompt.file {
            let resolved = spec
                .prompt
                .directory
                .clone()
                .unwrap_or_else(|| paths.cwd.clone())
                .join(file);
            if resolved.is_file() && seen_specs.insert(resolved.clone()) {
                out.push(ResolvedDoc {
                    doc: ConfigDocument::new(
                        format!("prompts.spec.{}", short_id(&resolved)),
                        ModuleId::Prompts,
                        scope,
                        format!("{}提示词 DSL · {}", label, file_name(&resolved)),
                        resolved.clone(),
                        if resolved.extension().and_then(|e| e.to_str()) == Some("toml") {
                            DocFormat::Toml
                        } else {
                            DocFormat::Markdown
                        },
                    )
                    .managed_by("one-prompt PromptSpec::load()")
                    .read_only_reason(
                        "提示词 DSL 规格（组件、slot、operations）。P1 只展示；编辑请直接改文件。",
                    )
                    .precedence(if scope == Scope::Project { 0 } else { 1 })
                    .override_note(format!("被 Agent `{}` 引用（`prompt.file`）。", agent_name)),
                    root: resolved.parent().map(Path::to_path_buf).unwrap_or_default(),
                    kind: DocKind::ReadOnly,
                });
            }
        }
    }

    out
}

/// Agent spec files in runtime search order, with the layer they belong to.
fn agent_spec_files(paths: &StudioPaths) -> Vec<(PathBuf, Scope, &'static str)> {
    let dirs = [
        (
            paths.cwd.join(".one").join("agents"),
            Scope::Project,
            "项目",
        ),
        (paths.agent_dir.join("agents"), Scope::Global, "全局"),
    ];
    let mut out = Vec::new();
    for (dir, scope, label) in dirs {
        for path in list_files(&dir, &["json", "md"]) {
            out.push((path, scope, label));
        }
    }
    out
}

/// 8-hex-char digest of a path, used for stable document ids.
fn short_id(path: &Path) -> String {
    hash_bytes(path.display().to_string().as_bytes())[..8].to_string()
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("(unknown)")
}

fn file_stem(path: &Path) -> &str {
    path.file_stem().and_then(|n| n.to_str()).unwrap_or("agent")
}

/// `SKILL.md` packages across every discovery root.
fn skill_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();
    let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();

    // Builtin skills materialise under `<agent_dir>/builtin-skills`. Note that
    // `<agent_dir>/skills` is a *user* root (`user_skill_dirs`), not a builtin one.
    let builtin_root = paths.agent_dir.join("builtin-skills");

    let mut roots: Vec<(PathBuf, Scope, &'static str)> = Vec::new();
    for dir in one_resources::skill_discovery_dirs(&paths.cwd, &paths.agent_dir) {
        // Root `~/.one/agent/skills` and friends.
        roots.push((
            dir,
            Scope::Global,
            "one-resources loader::skill_discovery_dirs()",
        ));
    }
    roots.push((
        builtin_root.clone(),
        Scope::Builtin,
        "one-resources builtin_skills::load_builtin_skills()",
    ));

    for (root, scope, who) in roots {
        if !root.is_dir() {
            continue;
        }
        for skill_md in find_files_named(&root, "SKILL.md", MAX_SKILL_DEPTH) {
            if !seen_paths.insert(skill_md.clone()) {
                continue;
            }
            let is_builtin = skill_md.starts_with(&builtin_root);
            let name = skill_md
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("skill");

            let doc = ConfigDocument::new(
                format!("skills.{}", &hash_bytes(skill_md.display().to_string().as_bytes())[..8]),
                ModuleId::Extensions,
                if is_builtin { Scope::Builtin } else { scope },
                format!("Skill · {name}"),
                skill_md.clone(),
                DocFormat::Markdown,
            )
            .managed_by(who)
            .read_only_reason(if is_builtin {
                "内置或客户端自带 Skill。启停通过 settings.json 的 skills_config 生效（P2 提供开关）。"
            } else {
                "第三方 / 用户 Skill 包。P1 仅展示目录与来源；启停与安装走既有流程。"
            })
            .precedence(if is_builtin { 2 } else { 0 });
            out.push(ResolvedDoc {
                doc,
                root: skill_md
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone()),
                kind: DocKind::ReadOnly,
            });
        }
    }

    out
}

/// Plugin manifests discovered from the project chain.
fn plugin_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut out = Vec::new();
    const MANIFESTS: [&str; 3] = [
        ".one-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        "plugin.json",
    ];

    let mut dirs: Vec<PathBuf> = paths.project_chain.clone();
    dirs.push(paths.cwd.clone());

    for dir in dirs {
        for manifest in MANIFESTS {
            let path = dir.join(manifest);
            if !path.is_file() {
                continue;
            }
            let doc = ConfigDocument::new(
                format!(
                    "plugins.{}",
                    &hash_bytes(path.display().to_string().as_bytes())[..8]
                ),
                ModuleId::Extensions,
                Scope::Project,
                format!(
                    "插件 · {}",
                    dir.file_name().and_then(|n| n.to_str()).unwrap_or(".")
                ),
                path.clone(),
                DocFormat::Json,
            )
            .managed_by("one-ext plugin::discover()")
            .read_only_reason("插件清单由插件作者维护；Studio 只报告发现结果与提供的 MCP / 钩子。")
            .precedence(0);
            out.push(ResolvedDoc {
                doc,
                root: dir.clone(),
                kind: DocKind::ReadOnly,
            });
        }
    }

    out
}

/// Files directly inside `dir` whose extension is in `exts`, sorted by name.
fn list_files(dir: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| exts.contains(&e))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

/// Recursively find files named `name` under `root`, skipping noise directories.
fn find_files_named(root: &Path, name: &str, max_depth: u32) -> Vec<PathBuf> {
    fn walk(dir: &Path, name: &str, depth: u32, max_depth: u32, out: &mut Vec<PathBuf>) {
        if depth > max_depth {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with('.') || file_name == "node_modules" || file_name == "target" {
                continue;
            }
            if path.is_dir() {
                walk(&path, name, depth + 1, max_depth, out);
            } else if file_name == name {
                out.push(path);
            }
        }
    }

    let mut out = Vec::new();
    walk(root, name, 0, max_depth, &mut out);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-config-ro-{tag}-{}-{}",
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
    fn builtin_presets_are_always_listed() {
        let agent = temp_dir("agent");
        let project = temp_dir("project");
        let docs = documents(&paths_for(&project, &agent));
        let builtins: Vec<_> = docs
            .iter()
            .filter(|d| d.doc.scope == Scope::Builtin && d.doc.module == ModuleId::Agents)
            .collect();
        assert_eq!(builtins.len(), 4);
        for doc in builtins {
            assert!(!doc.doc.capabilities.write);
            assert!(doc.doc.read_only_reason.is_some());
        }
    }

    /// Minimal valid agent spec, so prompt tests exercise the real loader.
    fn agent_spec(prompt: &str) -> String {
        format!(
            r#"{{
  "name": "tester",
  "tools": {{ "profile": "none" }},
  "model": {{ "inherit": true }},
  "prompt": {prompt}
}}"#
        )
    }

    fn prompts_docs(paths: &StudioPaths) -> Vec<ResolvedDoc> {
        documents(paths)
            .into_iter()
            .filter(|d| d.doc.module == ModuleId::Prompts)
            .collect()
    }

    #[test]
    fn prompt_module_exposes_the_builtin_registry_and_every_preset() {
        let agent = temp_dir("prompt-builtin-agent");
        let project = temp_dir("prompt-builtin-project");
        let registry = one_prompt::ComponentRegistry::with_builtins();

        let docs = prompts_docs(&paths_for(&project, &agent));
        assert!(docs.iter().any(|d| d.doc.id == "prompts.builtin.registry"));

        // One document per registered preset — no hand-maintained duplicate list.
        for name in registry.preset_names() {
            let id = format!("prompts.builtin.preset.{name}");
            let doc = docs
                .iter()
                .find(|d| d.doc.id == id)
                .unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(doc.doc.scope, Scope::Builtin);
            assert!(!doc.doc.capabilities.write);
            let content = doc.doc.builtin_content.as_deref().unwrap();
            assert!(content.contains("## 组装顺序"), "{content}");
            assert!(!content.contains("引用错误"), "{content}");
        }

        // A preset that names an unregistered component would hide a real
        // regression, so the rendered body has to include component text.
        let code = docs
            .iter()
            .find(|d| d.doc.id == "prompts.builtin.preset.code")
            .unwrap();
        let content = code.doc.builtin_content.as_deref().unwrap();
        assert!(content.contains("```text"), "expected component bodies");
        assert!(content.contains("safety"), "{content}");
    }

    #[test]
    fn prompt_module_reports_an_agent_prompt_reference() {
        let agent = temp_dir("prompt-ref-agent");
        let project = temp_dir("prompt-ref-project");
        fs::create_dir_all(project.join(".one/agents")).unwrap();
        fs::write(
            project.join(".one/agents/tester.json"),
            agent_spec(
                r#"{ "preset": "general", "operations": [ { "slot": "role", "op": "replace", "body": { "text": "CUSTOM ROLE" } } ] }"#,
            ),
        )
        .unwrap();

        let docs = prompts_docs(&paths_for(&project, &agent));
        let reference = docs
            .iter()
            .find(|d| d.doc.id.starts_with("prompts.ref."))
            .expect("an agent prompt reference document");

        assert_eq!(reference.doc.scope, Scope::Project);
        assert!(!reference.doc.capabilities.write);
        // The declared file is the real source, not a synthesized path.
        assert!(reference.doc.path.ends_with(".one/agents/tester.json"));
        assert_eq!(reference.doc.override_note.as_deref(), Some("引用可解析。"));

        let content = reference.doc.builtin_content.as_deref().unwrap();
        assert!(content.contains("`general`"), "{content}");
        assert!(content.contains("CUSTOM ROLE"), "{content}");
        assert!(content.contains("生效组件数"), "{content}");
        assert!(!content.contains("引用错误"), "{content}");
    }

    #[test]
    fn prompt_module_flags_an_unknown_preset() {
        let agent = temp_dir("prompt-badpreset-agent");
        let project = temp_dir("prompt-badpreset-project");
        fs::create_dir_all(project.join(".one/agents")).unwrap();
        fs::write(
            project.join(".one/agents/tester.json"),
            agent_spec(r#"{ "preset": "definitely-not-registered" }"#),
        )
        .unwrap();

        let docs = prompts_docs(&paths_for(&project, &agent));
        let reference = docs
            .iter()
            .find(|d| d.doc.id.starts_with("prompts.ref."))
            .expect("reference doc");
        let note = reference.doc.override_note.clone().unwrap();
        assert!(note.contains("引用错误"), "{note}");
        let content = reference.doc.builtin_content.as_deref().unwrap();
        assert!(content.contains("引用错误"), "{content}");
    }

    #[test]
    fn prompt_module_flags_a_missing_dsl_file_and_lists_a_present_one() {
        let agent = temp_dir("prompt-file-agent");
        let project = temp_dir("prompt-file-project");
        fs::create_dir_all(project.join(".one/agents")).unwrap();

        // Missing: relative to the declaring spec, exactly like the runtime.
        fs::write(
            project.join(".one/agents/broken.json"),
            agent_spec(r#"{ "file": "nowhere.toml" }"#),
        )
        .unwrap();
        let paths = paths_for(&project, &agent);
        let docs = prompts_docs(&paths);
        let broken = docs
            .iter()
            .find(|d| d.doc.path.ends_with("broken.json"))
            .expect("reference doc for the broken spec");
        assert!(broken
            .doc
            .override_note
            .as_deref()
            .unwrap()
            .contains("引用错误"));
        assert!(
            !docs.iter().any(|d| d.doc.id.starts_with("prompts.spec.")),
            "a missing DSL file must not be listed as a document"
        );

        // Present: the referenced DSL file becomes its own document.
        fs::write(
            project.join(".one/agents/researcher.toml"),
            "preset = \"general\"\n",
        )
        .unwrap();
        fs::write(
            project.join(".one/agents/tester.json"),
            agent_spec(r#"{ "file": "researcher.toml" }"#),
        )
        .unwrap();

        let docs = prompts_docs(&paths);
        let dsl = docs
            .iter()
            .find(|d| d.doc.id.starts_with("prompts.spec."))
            .expect("the referenced DSL file is listed");
        assert_eq!(dsl.doc.format, DocFormat::Toml);
        assert_eq!(dsl.doc.scope, Scope::Project);
        assert!(dsl
            .doc
            .override_note
            .as_deref()
            .unwrap()
            .contains("prompt.file"));
        assert!(Path::new(&dsl.doc.path).is_file());
        assert!(!dsl.doc.capabilities.write);
    }

    #[test]
    fn prompt_templates_ignore_toml_because_the_loader_does() {
        let agent = temp_dir("prompt-tpl-agent");
        let project = temp_dir("prompt-tpl-project");
        fs::create_dir_all(agent.join("prompts")).unwrap();
        fs::write(agent.join("prompts/notes.md"), "# notes").unwrap();
        fs::write(agent.join("prompts/ignored.toml"), "preset = \"code\"").unwrap();

        let docs = prompts_docs(&paths_for(&project, &agent));
        let templates: Vec<_> = docs
            .iter()
            .filter(|d| d.doc.id.starts_with("prompts.template."))
            .collect();
        assert_eq!(
            templates.len(),
            1,
            "{:?}",
            templates.iter().map(|d| &d.doc.path).collect::<Vec<_>>()
        );
        assert!(templates[0].doc.path.ends_with("notes.md"));
        assert_eq!(templates[0].doc.scope, Scope::Global);
        assert!(!templates[0].doc.capabilities.write);
    }

    #[test]
    fn agent_spec_files_are_discovered_with_real_paths() {
        let agent = temp_dir("agent-spec");
        let project = temp_dir("project-spec");
        fs::create_dir_all(project.join(".one/agents")).unwrap();
        fs::create_dir_all(agent.join("agents")).unwrap();
        fs::write(project.join(".one/agents/reviewer.json"), "{}").unwrap();
        fs::write(agent.join("agents/helper.md"), "# helper").unwrap();

        let docs = documents(&paths_for(&project, &agent));
        let spec_docs: Vec<_> = docs
            .iter()
            .filter(|d| d.doc.id.starts_with("agents.file."))
            .collect();
        assert_eq!(spec_docs.len(), 2);
        assert!(spec_docs.iter().any(|d| d.doc.scope == Scope::Project));
        assert!(spec_docs.iter().any(|d| d.doc.scope == Scope::Global));
        for doc in spec_docs {
            assert!(Path::new(&doc.doc.path).exists());
            assert!(!doc.doc.capabilities.write);
        }
    }

    #[test]
    fn agents_md_is_listed_from_the_project_chain() {
        let agent = temp_dir("agent-md");
        let root = temp_dir("root-md");
        let nested = root.join("pkg");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        fs::write(nested.join("CLAUDE.md"), "local rules").unwrap();

        let docs = documents(&paths_for(&nested, &agent));
        let instruction_docs: Vec<_> = docs
            .iter()
            .filter(|d| d.doc.id.starts_with("agents.instructions."))
            .collect();
        assert_eq!(instruction_docs.len(), 2);
        assert!(instruction_docs
            .iter()
            .any(|d| d.doc.path.ends_with("AGENTS.md")));
        assert!(instruction_docs
            .iter()
            .any(|d| d.doc.path.ends_with("CLAUDE.md")));
    }

    #[test]
    fn skills_are_found_and_builtins_flagged() {
        let agent = temp_dir("agent-skills");
        let project = temp_dir("project-skills");
        let builtin = agent.join("builtin-skills/create-skill");
        fs::create_dir_all(&builtin).unwrap();
        fs::write(builtin.join("SKILL.md"), "---\nname: create-skill\n---\n").unwrap();

        let user_skill = agent.join("skills/my-skill");
        fs::create_dir_all(&user_skill).unwrap();
        fs::write(user_skill.join("SKILL.md"), "---\nname: my-skill\n---\n").unwrap();

        // Skill discovery also walks real user roots (`~/.agents/skills`, …), so
        // assert on the fixtures we created rather than on the total count.
        let docs = documents(&paths_for(&project, &agent));
        let skills: Vec<_> = docs
            .iter()
            .filter(|d| d.doc.id.starts_with("skills."))
            .collect();

        let builtin_doc = skills
            .iter()
            .find(|d| Path::new(&d.doc.path).starts_with(&builtin))
            .expect("builtin skill discovered");
        assert!(builtin_doc.doc.title.contains("create-skill"));
        assert_eq!(builtin_doc.doc.scope, Scope::Builtin);

        let user_doc = skills
            .iter()
            .find(|d| Path::new(&d.doc.path).starts_with(&agent.join("skills")))
            .expect("user skill discovered");
        assert!(user_doc.doc.title.contains("my-skill"));
        assert_eq!(user_doc.doc.scope, Scope::Global);
        assert!(!user_doc.doc.capabilities.write);
    }

    #[test]
    fn auth_store_is_sensitive_and_read_only() {
        let agent = temp_dir("agent-auth");
        let project = temp_dir("project-auth");
        fs::write(
            agent.join("auth.json"),
            "{\"openai\":{\"accessToken\":\"x\"}}",
        )
        .unwrap();
        let docs = documents(&paths_for(&project, &agent));
        let auth = docs.iter().find(|d| d.doc.id == "extensions.auth").unwrap();
        assert!(auth.doc.sensitive);
        assert!(!auth.doc.capabilities.write);
        assert!(auth.doc.exists);
    }

    #[test]
    fn plugin_manifests_are_discovered() {
        let agent = temp_dir("agent-plugin");
        let project = temp_dir("project-plugin");
        fs::create_dir_all(project.join(".one-plugin")).unwrap();
        fs::write(project.join(".one-plugin/plugin.json"), "{\"name\":\"p\"}").unwrap();

        let docs = documents(&paths_for(&project, &agent));
        assert!(docs.iter().any(|d| d.doc.id.starts_with("plugins.")));
    }

    #[test]
    fn missing_files_are_reported_as_not_existing() {
        let agent = temp_dir("agent-missing");
        let project = temp_dir("project-missing");
        let docs = documents(&paths_for(&project, &agent));
        let ext = docs
            .iter()
            .find(|d| d.doc.id == "extensions.extensions_json")
            .unwrap();
        assert!(!ext.doc.exists);
        assert!(ext
            .doc
            .read_only_reason
            .as_deref()
            .unwrap()
            .contains("不存在"));
    }
}
