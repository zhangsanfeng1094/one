//! First-class enhancer documents, using the runtime parser and studio writer.
use super::{DocKind, ResolvedDoc, StudioPaths, Validation};
use crate::config_studio::document::{
    Capabilities, ConfigDocument, Diagnostic, DocFormat, ModuleId, Scope,
};
use crate::runtime::prompt_enhancer::config;

pub fn documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut layers = vec![(
        "enhancers.global".to_string(),
        Scope::Global,
        paths.agent_dir.clone(),
    )];
    for (index, root) in config::project_roots(&paths.cwd).into_iter().enumerate() {
        let id = if root == paths.cwd {
            "enhancers.project".into()
        } else {
            format!("enhancers.ancestor.{index}")
        };
        layers.push((id, Scope::Project, root.join(".one")));
    }
    layers.into_iter().enumerate().map(|(index, (id, scope, root))| {
        let mut doc = ConfigDocument::new(id, ModuleId::Prompts, scope, "模型增强 / Model Enhancers", root.join(config::RELATIVE_PATH), DocFormat::Json)
            .writable(Capabilities::EDITABLE_TEXT)
            .managed_by("Model Enhancer → compile_host → one-prompt")
            .precedence(index as u32)
            .override_note("模型增强入口可直接编辑文本；源码视图用于高级配置。内置默认值只读，删除当前层覆盖后恢复继承。");
        doc.effect_note = "保存即刻用于 Effective Prompt 预览；runtime 在下次编译（新会话、模型切换或 reload）时读取。".into();
        ResolvedDoc { doc, root, kind: DocKind::Enhancers }
    }).collect()
}

pub fn validate(draft: &str) -> Validation {
    match config::parse(draft) {
        Ok(_) => {
            Validation::ok(serde_json::from_str(draft).expect("runtime parser validated JSON"))
        }
        Err(e) => Validation {
            diagnostics: vec![Diagnostic::error(e)],
            parsed: None,
        },
    }
}
