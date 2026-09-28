//! Config document model for the One Config Studio.
//!
//! The studio separates two ideas that are easy to conflate:
//!
//! * **编辑目标 (edit scope)** — the concrete file a module reads, which is what
//!   the user actually mutates.
//! * **生效配置 (effective config)** — the merged result the runtime will use,
//!   which is read-only and always attributed back to real sources.
//!
//! Every document is addressed by a stable [`ConfigDocument::id`] minted from the
//! catalog, never by a client-supplied path.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which layer a document belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Under the One agent home (`~/.one/agent`, overridable by `ONE_AGENT_DIR`).
    Global,
    /// Under the project (`.one/…`), possibly in an ancestor directory.
    Project,
    /// Compiled into the binary; never writable.
    Builtin,
    /// Exists in another tool's directory and is only an import candidate.
    Foreign,
}

/// On-disk format, which drives both the editor language and whether comments
/// survive a round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocFormat {
    /// JSON — parsed strictly, comments are not supported.
    Json,
    /// TOML — only safe to edit as text when comment preservation matters.
    Toml,
    /// Markdown (skills, prompts, `AGENTS.md`).
    Markdown,
    /// Line-delimited JSON.
    Jsonl,
    /// Anything else; text only.
    Text,
}

impl DocFormat {
    /// Value for the `language` hint consumed by the frontend editor.
    pub fn language(self) -> &'static str {
        match self {
            DocFormat::Json => "json",
            DocFormat::Jsonl => "json",
            DocFormat::Toml => "toml",
            DocFormat::Markdown => "markdown",
            DocFormat::Text => "text",
        }
    }

    /// Whether the format can be parsed into a structured value for form editing.
    pub fn is_structured(self) -> bool {
        matches!(self, DocFormat::Json)
    }

    /// Whether a text round trip can preserve comments.
    pub fn preserves_comments(self) -> bool {
        matches!(
            self,
            DocFormat::Toml | DocFormat::Markdown | DocFormat::Text
        )
    }
}

/// The information-architecture module a document belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleId {
    /// 模型与服务商 — providers, credentials, model parameters.
    Providers,
    /// 系统设置 — permissions, compaction, tool output, memory, features, skills.
    Settings,
    /// MCP — global/project servers, transport, tool policy.
    Mcp,
    /// Agent — built-in and custom specs, tool permissions, resources.
    Agents,
    /// 提示词 — prompt config, bodies, rules and references.
    Prompts,
    /// 扩展与集成 — skills, plugins, bot, auth status.
    Extensions,
}

impl ModuleId {
    /// All modules in navigation order.
    pub const ALL: [ModuleId; 6] = [
        ModuleId::Providers,
        ModuleId::Settings,
        ModuleId::Mcp,
        ModuleId::Agents,
        ModuleId::Prompts,
        ModuleId::Extensions,
    ];

    /// Stable id used by the API.
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleId::Providers => "providers",
            ModuleId::Settings => "settings",
            ModuleId::Mcp => "mcp",
            ModuleId::Agents => "agents",
            ModuleId::Prompts => "prompts",
            ModuleId::Extensions => "extensions",
        }
    }

    /// Chinese label matching `docs/web-config.md` §2.
    pub fn label(self) -> &'static str {
        match self {
            ModuleId::Providers => "模型与服务商",
            ModuleId::Settings => "系统设置",
            ModuleId::Mcp => "MCP",
            ModuleId::Agents => "Agent",
            ModuleId::Prompts => "提示词",
            ModuleId::Extensions => "扩展与集成",
        }
    }

    /// One-line description of what the module manages.
    pub fn summary(self) -> &'static str {
        match self {
            ModuleId::Providers => "Provider、模型参数、凭据引用",
            ModuleId::Settings => "权限、压缩、工具输出、记忆、功能开关、Skills 启停",
            ModuleId::Mcp => "全局／项目服务、传输参数、工具策略",
            ModuleId::Agents => "内置及自定义规格、工具权限、资源引用",
            ModuleId::Prompts => "配置、正文、规则及引用关系",
            ModuleId::Extensions => "Skills、插件、Bot、认证状态",
        }
    }
}

/// Actions the studio can perform on a document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Save the document through the studio's transactional writer.
    pub write: bool,
    /// Create the file when it does not exist yet.
    pub create: bool,
    /// Structured form editing (requires [`DocFormat::is_structured`]).
    pub form: bool,
    /// Raw text editing.
    pub source: bool,
    /// Restore from a studio-managed backup.
    pub restore: bool,
}

impl Capabilities {
    /// Read-only document.
    pub const READ_ONLY: Self = Self {
        write: false,
        create: false,
        form: false,
        source: false,
        restore: false,
    };

    /// Fully editable JSON document.
    pub const EDITABLE_JSON: Self = Self {
        write: true,
        create: true,
        form: true,
        source: true,
        restore: true,
    };

    /// Editable as raw text only (no structured form).
    pub const EDITABLE_TEXT: Self = Self {
        write: true,
        create: true,
        form: false,
        source: true,
        restore: true,
    };
}

/// When a saved change takes effect, stated explicitly so the UI can promise
/// exactly what the runtime will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectTiming {
    /// Takes effect for sessions started after the save.
    NewSession,
    /// Re-read by the running session on its next reload.
    Immediate,
    /// Requires restarting the process.
    Restart,
}

/// A single configuration artifact the studio can display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDocument {
    /// Opaque, catalog-minted id (never a client-supplied path).
    pub id: String,
    /// Owning module.
    pub module: ModuleId,
    /// Layer this document belongs to.
    pub scope: Scope,
    /// Human-facing title.
    pub title: String,
    /// Absolute path on disk (shown verbatim, never simplified away).
    pub path: String,
    /// Project root this document resolves against, for project-scope entries.
    pub project_root: Option<String>,
    /// Whether the file currently exists.
    pub exists: bool,
    /// On-disk format.
    pub format: DocFormat,
    /// What the studio may do with it.
    pub capabilities: Capabilities,
    /// Whether the document can contain credentials.
    pub sensitive: bool,
    /// When changes take effect.
    pub effect: EffectTiming,
    /// Explanation of the effect timing, shown next to the save button.
    pub effect_note: String,
    /// Runtime loader(s) that read this file — the "真实来源" evidence.
    pub managed_by: String,
    /// Why the document is not writable, when it is not.
    pub read_only_reason: Option<String>,
    /// Precedence rank within the module (lower wins).
    pub precedence: u32,
    /// Note about override semantics for this layer.
    pub override_note: Option<String>,
    /// Content for a document that is compiled into the binary rather than read
    /// from a file (built-in prompt presets, the component registry).
    ///
    /// Not serialized: the catalog is an inventory, and the text travels in the
    /// document view instead of being duplicated into every catalog response.
    #[serde(skip)]
    pub builtin_content: Option<String>,
}

impl ConfigDocument {
    /// Convenience constructor with read-only defaults filled in.
    pub fn new(
        id: impl Into<String>,
        module: ModuleId,
        scope: Scope,
        title: impl Into<String>,
        path: PathBuf,
        format: DocFormat,
    ) -> Self {
        Self {
            id: id.into(),
            module,
            scope,
            title: title.into(),
            path: path.display().to_string(),
            project_root: None,
            exists: path.is_file(),
            format,
            capabilities: Capabilities::READ_ONLY,
            sensitive: false,
            effect: EffectTiming::NewSession,
            effect_note: "保存后新会话生效".to_string(),
            managed_by: String::new(),
            read_only_reason: Some("内置或第三方资源，Studio 不直接改写".to_string()),
            precedence: 0,
            override_note: None,
            builtin_content: None,
        }
    }

    /// Mark the document writable through the studio.
    pub fn writable(mut self, caps: Capabilities) -> Self {
        self.capabilities = caps;
        self.read_only_reason = None;
        self
    }

    /// Attach the loader attribution string.
    pub fn managed_by(mut self, who: impl Into<String>) -> Self {
        self.managed_by = who.into();
        self
    }

    /// Mark as containing credentials.
    pub fn sensitive(mut self, yes: bool) -> Self {
        self.sensitive = yes;
        self
    }

    /// Set precedence rank (lower wins).
    pub fn precedence(mut self, rank: u32) -> Self {
        self.precedence = rank;
        self
    }

    /// Set the override explanation.
    pub fn override_note(mut self, note: impl Into<String>) -> Self {
        self.override_note = Some(note.into());
        self
    }

    /// Attach content for a document that has no file on disk.
    pub fn builtin_content(mut self, text: impl Into<String>) -> Self {
        self.builtin_content = Some(text.into());
        self
    }

    /// Set why the document is read-only.
    pub fn read_only_reason(mut self, reason: impl Into<String>) -> Self {
        self.read_only_reason = Some(reason.into());
        self
    }
}

/// Severity of a validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Blocks saving.
    Error,
    /// Does not block saving, but the runtime may behave surprisingly.
    Warning,
    /// Purely informational.
    Info,
}

/// A validation finding for a draft, mirroring what the runtime loader would do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// How serious the finding is.
    pub severity: Severity,
    /// Human-readable explanation.
    pub message: String,
    /// 1-based line number when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based column when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// Field path the finding applies to (dot path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl Diagnostic {
    /// Build an error finding.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            line: None,
            column: None,
            field: None,
        }
    }

    /// Build a warning finding.
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            message: message.into(),
            line: None,
            column: None,
            field: None,
        }
    }

    /// Build an informational finding.
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            message: message.into(),
            line: None,
            column: None,
            field: None,
        }
    }

    /// Attach a 1-based line/column location.
    pub fn at(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    /// Attach a field path.
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Whether this finding blocks saving.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Kind of control the frontend should render for a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FieldKind {
    /// Single-line text.
    Text,
    /// Multi-line text.
    Textarea,
    /// Numeric input.
    Number,
    /// Boolean toggle.
    Boolean,
    /// Fixed set of choices.
    Enum,
    /// Repeating string list.
    StringList,
    /// String→string map (MCP `env`, HTTP `headers`).
    StringMap,
    /// Credential; rendered masked with keep/replace/clear semantics.
    Secret,
    /// Identifier for a reference to another resource (shown with its resolved path).
    Reference,
}

/// Declarative description of one editable field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldSpec {
    /// Dot path from the document root, e.g. `compaction.ratio`.
    pub path: String,
    /// Display label.
    pub label: String,
    /// Control to render.
    pub kind: FieldKind,
    /// Help text shown under the control.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Allowed values for [`FieldKind::Enum`].
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub options: Vec<String>,
    /// Default used by the runtime when the field is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Minimum for numeric fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Maximum for numeric fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Whether the field only exists in newer versions; shown collapsed.
    pub advanced: bool,
}

/// Declarative description of a repeating set of entries (MCP servers, providers…).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSpec {
    /// Dot path of the collection, e.g. `mcpServers`.
    pub path: String,
    /// Display label.
    pub label: String,
    /// Label for the entry key (server name / provider id).
    pub key_label: String,
    /// Fields inside each entry.
    pub entry_fields: Vec<FieldSpec>,
    /// Optional nested collection inside each entry (e.g. provider `models`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested: Option<Box<CollectionSpec>>,
    /// Whether keys are stable identity (renaming is not offered) or free.
    pub key_immutable: bool,
}

/// Form description + current (masked) values for a structured document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormModel {
    /// Scalar fields at the document root.
    pub fields: Vec<FieldSpec>,
    /// Repeating collections at the document root.
    pub collections: Vec<CollectionSpec>,
    /// Masked current value of the whole document.
    pub value: serde_json::Value,
    /// Fields whose sensitive values are hidden behind the mask sentinel.
    pub masked_fields: Vec<String>,
}

/// Aggregated, read-only "what will actually be used" view for one module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveEntry {
    /// What is being resolved (e.g. server name, setting key).
    pub name: String,
    /// Where the winning value came from.
    pub source: String,
    /// Short rendering of the winning value.
    pub value: String,
    /// Whether an override shadowed a lower layer.
    pub overridden: bool,
}

/// Response shape for `GET /api/config/effective`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveReport {
    /// Module this report covers.
    pub module: ModuleId,
    /// Explicit note about what was NOT resolved (e.g. provider probing in P3).
    pub note: String,
    /// Resolved entries.
    pub entries: Vec<EffectiveEntry>,
    /// Sources that contributed, in precedence order.
    pub sources: Vec<String>,
    /// Environment variables / CLI flags that override file config in this run.
    pub overrides: Vec<OverrideInfo>,
}

/// One environment or CLI influence on the effective configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideInfo {
    /// Variable or flag name.
    pub name: String,
    /// Current value as seen by the studio process, if set.
    pub value: Option<String>,
    /// What it overrides.
    pub affects: String,
}

/// The studio's view of its own process context, shown at the top of the UI so
/// previews are never mistaken for other running sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudioContext {
    /// Version of the studio backend.
    pub version: String,
    /// Working directory the studio was started in.
    pub cwd: String,
    /// Effective agent home.
    pub agent_dir: String,
    /// Home directory used for cross-client compat paths.
    pub home_dir: String,
    /// All ancestor `.` directories that participate in project resolution.
    pub project_roots: Vec<String>,
    /// Whether the agent home was overridden by an environment variable.
    pub agent_dir_overridden: bool,
}
