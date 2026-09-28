//! Agent-facing prompt selection; relative paths are bound at the declaring file.
use one_prompt::{PromptSpec, SlotOperation};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<SlotOperation>,
    /// Optional inline DSL for programmatic agents (same schema as TOML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<PromptSpec>,
    #[serde(skip)]
    pub directory: Option<PathBuf>,
    #[serde(skip)]
    pub source: String,
}
impl PromptConfig {
    pub fn role(text: String) -> Self {
        Self {
            preset: Some("general".into()),
            operations: vec![SlotOperation::replace("role", text)],
            ..Self::default()
        }
    }
    pub fn load(&self, cwd: &Path) -> Result<PromptSpec, one_prompt::PromptError> {
        let dir = self.directory.as_deref().unwrap_or(cwd);
        let count = usize::from(self.preset.is_some())
            + usize::from(self.file.is_some())
            + usize::from(self.spec.is_some());
        if count > 1 {
            return Err(one_prompt::PromptError {
                kind: one_prompt::ErrorKind::InvalidOperation,
                source_location: self.source.clone(),
                message: "prompt accepts only one of preset, file, spec".into(),
            });
        }
        let mut spec = if let Some(file) = &self.file {
            PromptSpec::load(dir.join(file))?
        } else if let Some(spec) = &self.spec {
            spec.clone()
        } else {
            PromptSpec {
                preset: self.preset.clone().unwrap_or_else(|| "code".into()),
                ..PromptSpec::default()
            }
        };
        if spec.source.is_empty() || spec.source == "<inline>" {
            spec.source = if self.source.is_empty() {
                "AgentSpec.prompt".into()
            } else {
                self.source.clone()
            };
        }
        // Preserve the declaring AgentSpec as the source of host-side overrides,
        // even when their base DSL lives in a different file.
        let mut overrides = PromptSpec {
            source: if self.source.is_empty() {
                "AgentSpec.prompt".into()
            } else {
                self.source.clone()
            },
            operations: self.operations.clone(),
            ..Default::default()
        };
        for (i, op) in overrides.operations.iter_mut().enumerate() {
            op.source = format!("{}: operations[{i}]", overrides.source);
        }
        overrides.load_bodies(dir)?;
        spec.load_bodies(dir)?;
        spec.operations.extend(overrides.operations);
        Ok(spec)
    }
}

pub fn reject_legacy_prompt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<(), D::Error> {
    let _ = serde::de::IgnoredAny::deserialize(d)?;
    Err(serde::de::Error::custom("system_prompt / append_system_prompt were removed; migrate to prompt.preset or prompt.file and prompt.operations (replace role / append extra); see docs/prompt-dsl.md"))
}
