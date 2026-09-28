//! Pure, deterministic prompt compilation. File IO lives in [`PromptSpec::load`].
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub mod builtin;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSpec {
    pub version: u32,
    #[serde(default = "code_preset")]
    pub preset: String,
    #[serde(default)]
    pub components: Vec<Component>,
    #[serde(default)]
    pub operations: Vec<SlotOperation>,
    #[serde(default)]
    pub rules: Vec<ModelRule>,
    #[serde(skip)]
    pub source: String,
}
fn code_preset() -> String {
    "code".into()
}
impl Default for PromptSpec {
    fn default() -> Self {
        Self {
            version: 1,
            preset: code_preset(),
            components: vec![],
            operations: vec![],
            rules: vec![],
            source: "<inline>".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(skip)]
    pub source: String,
}
impl Body {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// All named capabilities must be present in the host's actual tool/capability set.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub modes: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub id: String,
    pub body: Body,
    #[serde(default)]
    pub when: Condition,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub id: String,
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Append,
    Replace,
    Disable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotOperation {
    pub slot: String,
    pub op: Operation,
    #[serde(default)]
    pub body: Option<Body>,
    #[serde(skip)]
    pub source: String,
}
impl SlotOperation {
    pub fn replace(slot: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            slot: slot.into(),
            op: Operation::Replace,
            body: Some(Body::text(text)),
            source: String::new(),
        }
    }
    pub fn append(slot: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            op: Operation::Append,
            ..Self::replace(slot, text)
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRule {
    pub provider: String,
    pub model: String,
    pub operations: Vec<SlotOperation>,
}

#[derive(Debug, Clone)]
pub struct CompileContext {
    pub provider: String,
    pub model: String,
    pub mode: String,
    pub capabilities: BTreeSet<String>,
    pub variables: BTreeMap<String, String>,
}
impl Default for CompileContext {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            mode: "act".into(),
            capabilities: BTreeSet::new(),
            variables: ["resources", "environment", "memory_catalog"]
                .map(|key| (key.into(), String::new()))
                .into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub operation: Operation,
    pub source: String,
    pub rule: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotTrace {
    pub id: String,
    pub component: String,
    pub source: String,
    pub emitted: bool,
    pub text: String,
    pub operations: Vec<OperationRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPrompt {
    pub text: String,
    pub slots: Vec<SlotTrace>,
    pub matched_rules: Vec<usize>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Parse,
    Version,
    UnknownPreset,
    DuplicateComponent,
    DuplicateSlot,
    UnknownSlot,
    InvalidOperation,
    Body,
    MissingVariable,
    Io,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{source_location}: {kind:?}: {message}")]
pub struct PromptError {
    pub kind: ErrorKind,
    pub source_location: String,
    pub message: String,
}
fn error(kind: ErrorKind, source: &str, message: impl Into<String>) -> PromptError {
    PromptError {
        kind,
        source_location: source.into(),
        message: message.into(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct ComponentRegistry {
    components: BTreeMap<String, Component>,
    presets: BTreeMap<String, Vec<String>>,
}
impl ComponentRegistry {
    pub fn with_builtins() -> Self {
        builtin::registry()
    }
    pub fn register(&mut self, component: Component) -> Result<(), PromptError> {
        if self.components.contains_key(&component.id) {
            return Err(error(
                ErrorKind::DuplicateComponent,
                "registry",
                &component.id,
            ));
        }
        let mut ids = BTreeSet::new();
        for slot in &component.slots {
            if !ids.insert(&slot.id) {
                return Err(error(ErrorKind::DuplicateSlot, "registry", &slot.id));
            }
        }
        self.components.insert(component.id.clone(), component);
        Ok(())
    }
    pub fn preset(
        &mut self,
        name: impl Into<String>,
        components: Vec<String>,
    ) -> Result<(), PromptError> {
        let name = name.into();
        if self.presets.contains_key(&name) {
            return Err(error(
                ErrorKind::InvalidOperation,
                "registry",
                format!("duplicate preset {name}"),
            ));
        }
        self.presets.insert(name, components);
        Ok(())
    }

    // ---- read-only inspection ----
    //
    // These exist so tooling (the Config Studio) can report what is actually
    // registered instead of keeping a second hand-written list that would drift.

    /// Registered preset names, in stable order.
    pub fn preset_names(&self) -> Vec<&str> {
        self.presets.keys().map(String::as_str).collect()
    }

    /// Component ids a preset expands to, in declaration order.
    pub fn preset_components(&self, name: &str) -> Option<&[String]> {
        self.presets.get(name).map(Vec::as_slice)
    }

    /// Whether a preset name resolves — used to flag stale references.
    pub fn has_preset(&self, name: &str) -> bool {
        self.presets.contains_key(name)
    }

    /// A component by id.
    pub fn component(&self, id: &str) -> Option<&Component> {
        self.components.get(id)
    }

    /// All component ids, in stable order.
    pub fn component_ids(&self) -> Vec<&str> {
        self.components.keys().map(String::as_str).collect()
    }
}

impl PromptSpec {
    pub fn parse(raw: &str, source: impl Into<String>) -> Result<Self, PromptError> {
        let source = source.into();
        let mut spec: Self =
            toml::from_str(raw).map_err(|e| error(ErrorKind::Parse, &source, e.to_string()))?;
        spec.source = source;
        Ok(spec)
    }
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PromptError> {
        let path = path.as_ref();
        let source = path.display().to_string();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| error(ErrorKind::Io, &source, e.to_string()))?;
        let mut spec = Self::parse(&raw, source)?;
        spec.load_bodies(path.parent().unwrap_or(Path::new(".")))?;
        Ok(spec)
    }
    /// Resolve bodies once at the IO boundary; compile never reads files.
    pub fn load_bodies(&mut self, directory: &Path) -> Result<(), PromptError> {
        let source = self.source.clone();
        let bodies = self
            .components
            .iter_mut()
            .flat_map(|c| c.slots.iter_mut().map(|s| &mut s.body))
            .chain(self.operations.iter_mut().filter_map(|o| o.body.as_mut()))
            .chain(
                self.rules
                    .iter_mut()
                    .flat_map(|r| r.operations.iter_mut().filter_map(|o| o.body.as_mut())),
            );
        for body in bodies {
            if let Some(file) = &body.file {
                if body.text.is_some() {
                    return Err(error(
                        ErrorKind::Body,
                        &source,
                        "body requires exactly one of text or file",
                    ));
                }
                let path = directory.join(file);
                body.source = format!("{} -> {}", source, path.display());
                body.text = Some(
                    std::fs::read_to_string(&path)
                        .map_err(|e| error(ErrorKind::Io, &body.source, e.to_string()))?,
                );
                body.file = None;
            }
        }
        Ok(())
    }
    /// Compatibility helper for migrating a full role body to a named slot.
    pub fn role(text: impl Into<String>) -> Self {
        Self {
            preset: "general".into(),
            operations: vec![SlotOperation::replace("role", text)],
            ..Self::default()
        }
    }
}

fn body_text<'a>(body: &'a Body, source: &str) -> Result<&'a str, PromptError> {
    if body.file.is_some() || body.text.is_none() {
        return Err(error(
            ErrorKind::Body,
            source,
            "expected loaded text body; use PromptSpec::load for files",
        ));
    }
    Ok(body.text.as_deref().unwrap())
}
fn matches(pattern: &str, value: &str) -> bool {
    // Linear-space wildcard matching; '*' is the only special character.
    let mut row = vec![false; value.chars().count() + 1];
    row[0] = true;
    for p in pattern.chars() {
        let mut next = vec![false; row.len()];
        next[0] = p == '*' && row[0];
        for (i, c) in value.chars().enumerate() {
            next[i + 1] = if p == '*' {
                row[i + 1] || next[i]
            } else {
                row[i] && p == c
            };
        }
        row = next;
    }
    *row.last().unwrap()
}
fn render(text: &str, context: &CompileContext, source: &str) -> Result<String, PromptError> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail
            .find("}}")
            .ok_or_else(|| error(ErrorKind::Body, source, "unclosed variable"))?;
        let name = tail[..end].trim();
        out.push_str(
            context
                .variables
                .get(name)
                .ok_or_else(|| error(ErrorKind::MissingVariable, source, name))?,
        );
        rest = &tail[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn compile(
    spec: &PromptSpec,
    registry: &ComponentRegistry,
    context: &CompileContext,
) -> Result<CompiledPrompt, PromptError> {
    if spec.version != 1 {
        return Err(error(
            ErrorKind::Version,
            &spec.source,
            format!("unsupported version {}", spec.version),
        ));
    }
    let preset = registry
        .presets
        .get(&spec.preset)
        .ok_or_else(|| error(ErrorKind::UnknownPreset, &spec.source, &spec.preset))?;
    let mut components = Vec::new();
    for id in preset {
        components.push(registry.components.get(id).ok_or_else(|| {
            error(
                ErrorKind::UnknownPreset,
                &spec.source,
                format!("missing component {id}"),
            )
        })?);
    }
    components.extend(spec.components.iter());
    let mut component_ids = BTreeSet::new();
    let mut slot_ids = BTreeMap::new();
    let mut slots = Vec::new();
    let mut bodies = Vec::new();
    let mut enabled = Vec::new();
    let mut conditions = Vec::new();
    for component in components {
        if !component_ids.insert(&component.id) {
            return Err(error(
                ErrorKind::DuplicateComponent,
                &spec.source,
                &component.id,
            ));
        }
        for slot in &component.slots {
            let source = if slot.body.source.is_empty() {
                format!(
                    "{}: components.{}.slots.{}",
                    spec.source, component.id, slot.id
                )
            } else {
                slot.body.source.clone()
            };
            if slot_ids.insert(slot.id.clone(), slots.len()).is_some() {
                return Err(error(ErrorKind::DuplicateSlot, &source, &slot.id));
            }
            body_text(&slot.body, &source)?;
            bodies.push(vec![(slot.body.clone(), source.clone())]);
            enabled.push(true);
            conditions.push(&slot.when);
            slots.push(SlotTrace {
                id: slot.id.clone(),
                component: component.id.clone(),
                source,
                emitted: false,
                text: String::new(),
                operations: vec![],
            });
        }
    }
    // Validate even unmatched rules so typos never become model-dependent surprises.
    for (label, ops) in std::iter::once(("operations".to_string(), &spec.operations)).chain(
        spec.rules
            .iter()
            .enumerate()
            .map(|(i, r)| (format!("rules[{i}].operations"), &r.operations)),
    ) {
        for (i, op) in ops.iter().enumerate() {
            let source = if op.source.is_empty() {
                format!("{}: {label}[{i}]", spec.source)
            } else {
                op.source.clone()
            };
            if !slot_ids.contains_key(&op.slot) {
                return Err(error(ErrorKind::UnknownSlot, &source, &op.slot));
            }
            match (op.op, &op.body) {
                (Operation::Disable, None) => (),
                (Operation::Append | Operation::Replace, Some(body)) => {
                    body_text(body, &source)?;
                }
                _ => {
                    return Err(error(
                        ErrorKind::InvalidOperation,
                        &source,
                        "disable forbids body; append/replace require body",
                    ))
                }
            }
        }
    }
    let matched_rules: Vec<_> = spec
        .rules
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            matches(&r.provider, &context.provider) && matches(&r.model, &context.model)
        })
        .map(|(i, _)| i)
        .collect();
    for (rule, ops) in std::iter::once((None, &spec.operations)).chain(
        matched_rules
            .iter()
            .map(|&i| (Some(i), &spec.rules[i].operations)),
    ) {
        for (j, op) in ops.iter().enumerate() {
            let i = slot_ids[&op.slot];
            let source = if op.source.is_empty() {
                format!(
                    "{}: {}operations[{j}]",
                    spec.source,
                    rule.map(|r| format!("rules[{r}].")).unwrap_or_default()
                )
            } else {
                op.source.clone()
            };
            match op.op {
                Operation::Disable => enabled[i] = false,
                Operation::Replace => {
                    bodies[i].clear();
                    enabled[i] = true;
                }
                Operation::Append if !enabled[i] => {
                    return Err(error(
                        ErrorKind::InvalidOperation,
                        &source,
                        format!("append to disabled slot {}", op.slot),
                    ))
                }
                Operation::Append => (),
            }
            if let Some(body) = &op.body {
                bodies[i].push((
                    body.clone(),
                    if body.source.is_empty() {
                        source.clone()
                    } else {
                        body.source.clone()
                    },
                ));
            }
            slots[i].operations.push(OperationRecord {
                operation: op.op,
                source,
                rule,
            });
        }
    }
    let mut text = String::new();
    for (i, slot) in slots.iter_mut().enumerate() {
        let when = conditions[i];
        slot.emitted = enabled[i]
            && when
                .capabilities
                .iter()
                .all(|c| context.capabilities.contains(c))
            && (when.modes.is_empty() || when.modes.contains(&context.mode));
        if slot.emitted {
            let mut slot_text = String::new();
            for (body, source) in &bodies[i] {
                slot_text.push_str(&render(body_text(body, source)?, context, source)?);
            }
            text.push_str(&slot_text);
            slot.text = slot_text;
        }
    }
    Ok(CompiledPrompt {
        text,
        slots,
        matched_rules,
    })
}
