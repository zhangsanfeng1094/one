//! HTTP API for the config studio.
//!
//! Contract (`docs/web-config.md` §3): every document endpoint returns the
//! **document version**, **source**, **supported capabilities**, **field
//! diagnostics**, and an **effect note**. Documents are addressed by catalog id
//! only — the API never accepts a filesystem path from the client, so path
//! traversal is structurally impossible rather than merely filtered.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config_studio::adapters::{self, DocKind, ResolvedDoc, StudioPaths};
use crate::config_studio::diff::unified_diff;
use crate::config_studio::document::{
    ConfigDocument, Diagnostic, EffectiveReport, FormModel, ModuleId, StudioContext,
};
use crate::config_studio::mask::{
    dangling_sentinels, mask_text, mask_value, restore_secrets, REDACTED,
};
use crate::config_studio::save::{
    self, content_version, read_document, BackupReason, BackupRecord, BackupStore, SaveError,
    VERSION_ABSENT,
};
use one_web::{ApiHandler, HttpRequest, HttpResponse};

/// Base path of the studio API.
pub const API_PREFIX: &str = "/api/config";

/// How the client produced the draft, which decides how secrets are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DraftView {
    /// Structured form edit; the draft is JSON that may still contain mask
    /// sentinels, which are restored from disk before writing.
    Form,
    /// Raw text edit; the draft is written verbatim.
    Source,
}

/// `GET /api/config/catalog`
#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    /// Studio process context (never claims to represent other sessions).
    pub context: StudioContext,
    /// Module navigation.
    pub modules: Vec<ModuleInfo>,
    /// Every known document, without contents.
    pub documents: Vec<ConfigDocument>,
    /// Where backups live, so the user can find them outside the studio.
    pub backup_root: String,
}

/// One navigable module.
#[derive(Debug, Serialize)]
pub struct ModuleInfo {
    /// Stable module id.
    pub id: String,
    /// Chinese label.
    pub label: String,
    /// One-line scope summary.
    pub summary: String,
    /// How many documents the module currently exposes.
    pub document_count: usize,
    /// Whether any document in the module is writable in this release.
    pub writable: bool,
}

/// `GET /api/config/documents/{id}`
#[derive(Debug, Serialize)]
pub struct DocumentView {
    /// Document metadata (path, scope, capabilities, effect note).
    pub document: ConfigDocument,
    /// Content version on disk; [`VERSION_ABSENT`] when the file is missing.
    pub version: String,
    /// Full content, masked unless the caller explicitly unlocked.
    pub content: String,
    /// Whether `content` has secrets replaced by the mask sentinel.
    pub masked: bool,
    /// Whether the caller asked for unmasked content.
    pub unlocked: bool,
    /// Parsed JSON value (masked) for structured documents.
    pub parsed: Option<Value>,
    /// Findings for the content as it exists on disk.
    pub diagnostics: Vec<Diagnostic>,
    /// Declarative form description for writable structured documents.
    pub form: Option<FormModel>,
    /// Set when the file could not be read at all.
    pub read_error: Option<String>,
}

/// `POST /api/config/documents/{id}/validate`
#[derive(Debug, Deserialize)]
pub struct ValidateRequest {
    /// Draft text the user is editing.
    pub draft: String,
    /// Which view produced the draft.
    #[serde(default = "default_view")]
    pub view: DraftView,
    /// Version the client based this draft on (the document version at load).
    ///
    /// When present, a mismatch with the file on disk is a 409: validation must
    /// not stamp a newer on-disk version onto an older draft.
    #[serde(default)]
    pub version: Option<String>,
}

fn default_view() -> DraftView {
    DraftView::Source
}

/// Validation result plus the change preview.
#[derive(Debug, Serialize)]
pub struct ValidateResponse {
    /// Edit-origin version that still matches disk. Never a newer disk version
    /// used to refresh an older draft.
    pub version: String,
    /// Findings for the draft.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether the draft may be saved (no error-severity findings).
    pub valid: bool,
    /// Unified diff of the masked before/after text.
    pub diff: String,
    /// Added lines in the preview.
    pub added: usize,
    /// Removed lines in the preview.
    pub removed: usize,
    /// True when the preview is a block replacement rather than an exact diff.
    pub approximate: bool,
    /// File the save would target.
    pub target: String,
    /// When the change takes effect.
    pub effect_note: String,
    /// Preview of the text a save would write, **with secrets still masked**.
    ///
    /// Never send this back as a save payload: it is display-only. Saving must
    /// echo the caller's own draft so that mask sentinels can be restored
    /// server-side against the on-disk value.
    pub preview_text: String,
}

/// `POST /api/config/documents/{id}/save`
#[derive(Debug, Deserialize)]
pub struct SaveRequest {
    /// Draft text to write.
    pub draft: String,
    /// Version the client based its edit on.
    pub version: String,
    /// Which view produced the draft.
    #[serde(default = "default_view")]
    pub view: DraftView,
    /// Must be `true`; prevents accidental writes from a stale tab.
    #[serde(default)]
    pub confirm: bool,
    /// True when the client has explicitly revealed secrets (required to save
    /// a sensitive document from the source view).
    #[serde(default)]
    pub unlocked: bool,
}

/// Result of a successful save.
#[derive(Debug, Serialize)]
pub struct SaveResponse {
    /// Version of the newly written content.
    pub version: String,
    /// Whether the file was created by this save.
    pub created: bool,
    /// Bytes written.
    pub bytes: usize,
    /// Backup of the displaced content, when there was any.
    pub backup: Option<BackupRecord>,
    /// When the change takes effect.
    pub effect_note: String,
    /// Target file.
    pub target: String,
}

/// `GET /api/config/documents/{id}/backups`
#[derive(Debug, Serialize)]
pub struct BackupsResponse {
    /// Backups, newest first.
    pub backups: Vec<BackupRecord>,
    /// Directory holding them.
    pub directory: String,
}

/// `POST /api/config/documents/{id}/restore`
#[derive(Debug, Deserialize)]
pub struct RestoreRequest {
    /// Backup id returned by the list endpoint.
    pub backup: String,
    /// Current on-disk version, checked before overwriting.
    pub version: String,
}

/// `POST /api/config/mcp/override`
#[derive(Debug, Deserialize)]
pub struct OverrideRequest {
    /// Server name whose winning entry should be copied.
    pub server: String,
    /// Target project document id (`mcp.project.*`).
    pub target_doc: String,
}

/// Result of materialising a project-level override.
#[derive(Debug, Serialize)]
pub struct OverrideResponse {
    /// Target document id.
    pub target_doc: String,
    /// New version of the target file.
    pub version: String,
    /// The copied entry with secrets masked, for confirmation.
    pub entry: Value,
    /// Where the entry was copied from.
    pub source: String,
    /// Explain the resulting override semantics.
    pub note: String,
    /// Change preview for the target file.
    pub diff: String,
}

/// The studio backend: catalog, document service, and request router.
pub struct ConfigStudio {
    paths: StudioPaths,
    backups: BackupStore,
    global_only: bool,
}

impl ConfigStudio {
    /// Build a studio around a working directory, detecting paths from the
    /// environment (`ONE_AGENT_DIR` honoured).
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_paths(StudioPaths::detect(cwd))
    }

    /// Build a studio against explicitly supplied paths.
    ///
    /// Used by tests so they never mutate process-global environment variables,
    /// which would otherwise leak into unrelated parallel tests.
    pub fn with_paths(paths: StudioPaths) -> Self {
        let backups = BackupStore::new(&paths.agent_dir);
        Self {
            paths,
            backups,
            global_only: false,
        }
    }

    /// Filesystem context.
    pub fn paths(&self) -> &StudioPaths {
        &self.paths
    }

    /// Current document catalog.
    pub fn documents(&self) -> Vec<ResolvedDoc> {
        adapters::resolve_documents(&self.paths)
            .into_iter()
            .filter(|d| {
                !self.global_only
                    || d.doc.scope != crate::config_studio::document::Scope::Project
                    || d.doc.id.starts_with("prompts.ref.")
            })
            .collect()
    }

    /// Find a document by id.
    fn resolve(&self, id: &str) -> Result<ResolvedDoc, HttpResponse> {
        let docs = self.documents();
        adapters::find(&docs, id)
            .cloned()
            .ok_or_else(|| HttpResponse::error(404, format!("未知的配置文档 id：{id}")))
    }

    /// Studio context for the UI header.
    pub fn context(&self) -> StudioContext {
        StudioContext {
            version: env!("CARGO_PKG_VERSION").to_string(),
            cwd: self.paths.cwd.display().to_string(),
            agent_dir: self.paths.agent_dir.display().to_string(),
            home_dir: self.paths.home.display().to_string(),
            project_roots: self
                .paths
                .project_chain
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            agent_dir_overridden: self.paths.agent_dir_overridden(),
        }
    }

    /// Build the catalog response.
    pub fn catalog(&self) -> CatalogResponse {
        let documents = self.documents();
        let modules = ModuleId::ALL
            .iter()
            .map(|module| {
                let owned: Vec<&ResolvedDoc> = documents
                    .iter()
                    .filter(|d| d.doc.module == *module)
                    .collect();
                ModuleInfo {
                    id: module.as_str().to_string(),
                    label: module.label().to_string(),
                    summary: module.summary().to_string(),
                    document_count: owned.len(),
                    writable: owned.iter().any(|d| d.writable()),
                }
            })
            .collect();

        CatalogResponse {
            context: self.context(),
            modules,
            documents: documents.into_iter().map(|d| d.doc).collect(),
            backup_root: self.backups.root().display().to_string(),
        }
    }

    /// Read a document, optionally unmasking secrets.
    pub fn view(&self, id: &str, unlock: bool) -> Result<DocumentView, HttpResponse> {
        let resolved = self.resolve(id)?;
        let path = resolved.path();

        // Built-ins have no file; their text is compiled into the binary and
        // attached to the catalog entry instead.
        let content = match resolved.doc.builtin_content.clone() {
            Some(text) => Some(text),
            None => match read_document(&path) {
                Ok(c) => c,
                Err(err) => {
                    return Ok(DocumentView {
                        version: VERSION_ABSENT.to_string(),
                        content: String::new(),
                        masked: resolved.doc.sensitive,
                        unlocked: false,
                        parsed: None,
                        diagnostics: vec![Diagnostic::error(err.message())],
                        form: None,
                        read_error: Some(err.message()),
                        document: resolved.doc,
                    })
                }
            },
        };

        let version = content_version(content.as_deref());
        let raw = content.clone().unwrap_or_else(|| scaffold(&resolved));
        let read_error = None;

        // Masking applies to sensitive documents; unlock is an explicit,
        // per-request decision and is never persisted.
        let (content_out, masked) = if resolved.doc.sensitive && !unlock {
            (mask_text(&raw), true)
        } else {
            (raw.clone(), false)
        };

        let (diagnostics, parsed) = if resolved.doc.format.is_structured() {
            let v = adapters::validate(resolved.kind, &content_out);
            (v.diagnostics, v.parsed)
        } else {
            (Vec::new(), None)
        };

        let form = if resolved.doc.capabilities.form {
            parsed
                .as_ref()
                .and_then(|value| adapters::form(resolved.kind, value))
        } else if resolved.doc.capabilities.write {
            // Source-only documents still get field metadata when parseable.
            parsed
                .as_ref()
                .and_then(|value| adapters::form(resolved.kind, value))
        } else {
            None
        };

        Ok(DocumentView {
            document: resolved.doc,
            version,
            content: content_out,
            masked,
            unlocked: unlock,
            parsed,
            diagnostics,
            form,
            read_error,
        })
    }

    /// Validate a draft and produce a masked change preview.
    pub fn validate(
        &self,
        id: &str,
        draft: &str,
        view: DraftView,
        expected_version: Option<&str>,
    ) -> Result<ValidateResponse, HttpResponse> {
        let resolved = self.resolve(id)?;
        let path = resolved.path();
        let existing = read_document(&path).map_err(save_error_response)?;
        let version = content_version(existing.as_deref());
        if let Some(expected) = expected_version {
            if expected != version {
                return Err(save_error_response(SaveError::Conflict {
                    expected: expected.to_string(),
                    current: version,
                }));
            }
        }

        let normalized = self.normalize_draft(&resolved, &existing, draft, view)?;
        let validation = adapters::validate(resolved.kind, &normalized);

        let old_text = if resolved.doc.sensitive {
            mask_text(existing.as_deref().unwrap_or(""))
        } else {
            existing.clone().unwrap_or_default()
        };
        // The preview is masked too: the normalized text has real secrets merged
        // back in, and validate responses are rendered in the browser.
        let new_text = if resolved.doc.sensitive {
            mask_text(&normalized)
        } else {
            normalized.clone()
        };
        let diff = unified_diff(&old_text, &new_text);

        Ok(ValidateResponse {
            version,
            valid: !validation.has_errors(),
            diagnostics: validation.diagnostics,
            diff: diff.render(),
            added: diff.added,
            removed: diff.removed,
            approximate: diff.approximate,
            target: resolved.doc.path.clone(),
            effect_note: resolved.doc.effect_note.clone(),
            preview_text: new_text,
        })
    }

    /// Commit a validated draft.
    pub fn save(&self, id: &str, req: &SaveRequest) -> Result<SaveResponse, HttpResponse> {
        let resolved = self.resolve(id)?;
        if !resolved.doc.capabilities.write && !resolved.doc.capabilities.create {
            return Err(HttpResponse::error(
                403,
                format!(
                    "该文档在 P1 阶段只读：{}",
                    resolved
                        .doc
                        .read_only_reason
                        .clone()
                        .unwrap_or_else(|| "未开放写入".to_string())
                ),
            ));
        }
        if !req.confirm {
            return Err(HttpResponse::error(
                400,
                "缺少 confirm：保存需要先确认变更预览",
            ));
        }

        let path = resolved.path();
        let existing = read_document(&path).map_err(save_error_response)?;

        // Order matters: a draft that still carries mask sentinels gets the more
        // specific 422 from `normalize_draft`, which is only reachable while the
        // file is locked. Only a draft with real values but no explicit unlock
        // falls through to the 403 below.
        let normalized = self.normalize_draft(&resolved, &existing, &req.draft, req.view)?;

        // Editing a sensitive file as raw text only makes sense once the real
        // values are visible; anything else would silently rewrite the file from
        // a copy the user never actually saw. Form edits are exempt because
        // sentinels there are restorable against the on-disk value.
        if resolved.doc.sensitive && req.view == DraftView::Source && !req.unlocked {
            return Err(HttpResponse::error(
                403,
                "敏感文档的源码视图需要先显式解锁，或改用表单视图编辑",
            ));
        }

        // Refuse to write a document the runtime would reject: a broken config
        // file is worse than no change at all.
        let validation = adapters::validate(resolved.kind, &normalized);
        if validation.has_errors() {
            return Err(HttpResponse::json(
                422,
                &serde_json::json!({
                    "error": "草稿未通过校验，已拒绝写入",
                    "diagnostics": validation.diagnostics,
                }),
            ));
        }

        let outcome = save::commit(
            &self.backups,
            &resolved.doc.id,
            &path,
            &resolved.root,
            &normalized,
            &req.version,
            BackupReason::Save,
        )
        .map_err(save_error_response)?;

        Ok(SaveResponse {
            version: outcome.version,
            created: outcome.created,
            bytes: outcome.bytes,
            backup: outcome.backup,
            effect_note: resolved.doc.effect_note.clone(),
            target: resolved.doc.path.clone(),
        })
    }

    /// List backups for a document.
    pub fn backups(&self, id: &str) -> Result<BackupsResponse, HttpResponse> {
        let resolved = self.resolve(id)?;
        Ok(BackupsResponse {
            backups: self.backups.list(&resolved.doc.id),
            directory: self.backups.doc_dir(&resolved.doc.id).display().to_string(),
        })
    }

    /// Restore a document from a backup, through the same validation and version
    /// checks as a normal save.
    pub fn restore(&self, id: &str, req: &RestoreRequest) -> Result<SaveResponse, HttpResponse> {
        let resolved = self.resolve(id)?;
        if !resolved.doc.capabilities.restore {
            return Err(HttpResponse::error(403, "该文档不支持从备份恢复"));
        }

        let content = self
            .backups
            .load(&resolved.doc.id, &req.backup)
            .map_err(save_error_response)?;

        let validation = adapters::validate(resolved.kind, &content);
        if validation.has_errors() {
            return Err(HttpResponse::json(
                422,
                &serde_json::json!({
                    "error": "备份内容未通过当前校验规则，已拒绝恢复（规则可能已变化）",
                    "diagnostics": validation.diagnostics,
                }),
            ));
        }

        let path = resolved.path();
        let outcome = save::commit(
            &self.backups,
            &resolved.doc.id,
            &path,
            &resolved.root,
            &content,
            &req.version,
            BackupReason::Restore,
        )
        .map_err(save_error_response)?;

        Ok(SaveResponse {
            version: outcome.version,
            created: outcome.created,
            bytes: outcome.bytes,
            backup: outcome.backup,
            effect_note: resolved.doc.effect_note.clone(),
            target: resolved.doc.path.clone(),
        })
    }

    /// Resolve the effective (read-only) configuration for a module.
    pub fn effective(&self, module: &str) -> Result<EffectiveReport, HttpResponse> {
        match module {
            "mcp" if self.global_only => Ok(adapters::mcp::effective_global(&self.paths)),
            "mcp" => Ok(adapters::mcp::effective(&self.paths)),
            "providers" => Ok(adapters::models::effective(&self.paths)),
            "settings" => Ok(settings_effective()),
            "prompts" => Ok(self.prompts_effective()),
            other => Err(HttpResponse::error(
                400,
                format!(
                    "模块 `{other}` 暂不提供生效视图（覆盖 settings / providers / mcp / prompts）"
                ),
            )),
        }
    }

    /// Copy the winning entry for `server` into a project document as an override.
    pub fn create_mcp_override(
        &self,
        req: &OverrideRequest,
    ) -> Result<OverrideResponse, HttpResponse> {
        let target = self.resolve(&req.target_doc)?;
        if target.doc.module != ModuleId::Mcp
            || target.doc.scope != crate::config_studio::document::Scope::Project
        {
            return Err(HttpResponse::error(
                400,
                "目标必须是项目级 mcp.json 文档（mcp.project.*）",
            ));
        }
        if !target.writable() {
            return Err(HttpResponse::error(403, "目标文档不可写"));
        }

        let loaded = one_mcp::config::load_effective(&self.paths.cwd)
            .map_err(|e| HttpResponse::error(500, format!("加载 MCP 配置失败：{e}")))?;

        // The winning layer is the last source listing this server name.
        let source = loaded
            .sources
            .iter()
            .rev()
            .find(|s| s.server_names.iter().any(|n| n == &req.server))
            .ok_or_else(|| {
                HttpResponse::error(404, format!("找不到服务 `{}` 的定义来源", req.server))
            })?;

        // Copy the raw JSON entry so fields the studio does not model survive.
        let raw_text = std::fs::read_to_string(&source.path).map_err(|e| {
            HttpResponse::error(500, format!("读取 {} 失败：{e}", source.path.display()))
        })?;
        let raw_value: Value = serde_json::from_str(&raw_text).map_err(|e| {
            HttpResponse::error(500, format!("{} 不是合法 JSON：{e}", source.path.display()))
        })?;
        let entry = raw_value
            .get("mcpServers")
            .or_else(|| raw_value.get("mcp_servers"))
            .and_then(|v| v.get(&req.server))
            .ok_or_else(|| {
                HttpResponse::error(
                    404,
                    format!(
                        "{} 中没有服务 `{}` 的条目",
                        source.path.display(),
                        req.server
                    ),
                )
            })?;

        let target_path = target.path();
        let existing = read_document(&target_path).map_err(save_error_response)?;
        let version = content_version(existing.as_deref());

        let mut doc_value: Value = match existing.as_deref() {
            Some(text) if !text.trim().is_empty() => serde_json::from_str(text).map_err(|e| {
                HttpResponse::error(
                    409,
                    format!(
                        "{} 当前不是合法 JSON（{e}），请先修复再创建覆盖，避免覆盖掉你的内容",
                        target_path.display()
                    ),
                )
            })?,
            _ => serde_json::json!({ "mcpServers": {} }),
        };
        if doc_value.get("mcpServers").is_none() {
            if let Some(obj) = doc_value.as_object_mut() {
                obj.insert("mcpServers".to_string(), Value::Object(Default::default()));
            }
        }
        doc_value
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| HttpResponse::error(500, "目标文件的 mcpServers 不是对象"))?
            .insert(req.server.clone(), entry.clone());

        let normalized = pretty_json(&doc_value);
        let validation = adapters::validate(DocKind::Mcp, &normalized);
        if validation.has_errors() {
            return Err(HttpResponse::json(
                422,
                &serde_json::json!({
                    "error": "生成的项目覆盖未通过校验",
                    "diagnostics": validation.diagnostics,
                }),
            ));
        }

        let old_text = mask_text(existing.as_deref().unwrap_or(""));
        let diff = unified_diff(&old_text, &mask_text(&normalized));

        let outcome = save::commit(
            &self.backups,
            &target.doc.id,
            &target_path,
            &target.root,
            &normalized,
            &version,
            BackupReason::Save,
        )
        .map_err(save_error_response)?;

        let (masked_entry, _) = mask_value(entry);

        Ok(OverrideResponse {
            target_doc: target.doc.id.clone(),
            version: outcome.version,
            entry: masked_entry,
            source: source.path.display().to_string(),
            note: format!(
                "已把 `{}` 的完整条目复制到项目文件（来源：{}）。\
                 项目层同名服务会整条替换上层配置；\
                 删除这个条目即可恢复继承上层定义，而不是逐字段回退。",
                req.server,
                source.path.display()
            ),
            diff: diff.render(),
        })
    }

    /// Normalize a draft for validation and writing.
    fn normalize_draft(
        &self,
        resolved: &ResolvedDoc,
        existing: &Option<String>,
        draft: &str,
        view: DraftView,
    ) -> Result<String, HttpResponse> {
        match view {
            DraftView::Source => {
                if resolved.doc.sensitive {
                    // The masked source view cannot round-trip: a sentinel has no
                    // information about the real value's shape. Requiring an
                    // explicit unlock keeps the semantics unambiguous.
                    if draft.contains(REDACTED) {
                        return Err(HttpResponse::error(
                            422,
                            "草稿中仍包含脱敏占位符：请先显式解锁查看完整源码，或改用表单视图编辑敏感字段",
                        ));
                    }
                }
                Ok(draft.to_string())
            }
            DraftView::Form => {
                let mut value: Value = serde_json::from_str(draft)
                    .map_err(|e| HttpResponse::error(400, format!("表单草稿不是合法 JSON：{e}")))?;
                if resolved.doc.sensitive {
                    let base: Value = existing
                        .as_deref()
                        .and_then(|t| serde_json::from_str(t).ok())
                        .unwrap_or(Value::Null);
                    restore_secrets(&mut value, &base);
                }
                let dangling = dangling_sentinels(&value);
                if !dangling.is_empty() {
                    return Err(HttpResponse::json(
                        422,
                        &serde_json::json!({
                            "error": "草稿中的脱敏占位符无法还原（原文件中已无对应值）",
                            "fields": dangling,
                        }),
                    ));
                }
                Ok(pretty_json(&value))
            }
        }
    }

    /// Route one API request.
    pub fn dispatch(&self, req: &HttpRequest) -> HttpResponse {
        // Opt-in UI scope; legacy API clients and runtime retain their source chain.
        if !self.global_only && req.query("studio_global").as_deref() == Some("1") {
            let mut paths = self.paths.clone();
            paths.project_chain.clear();
            let mut studio = Self::with_paths(paths);
            studio.global_only = true;
            return studio.dispatch(req);
        }
        let rest = req.path.strip_prefix(API_PREFIX).unwrap_or("");
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        let method = req.method.as_str();

        match (method, segments.as_slice()) {
            ("GET", ["prompts", "enhancers"]) => self.enhancer_view(req).unwrap_or_else(|e| e),
            ("POST", ["prompts", "enhancers"]) => self.enhancer_save(req).unwrap_or_else(|e| e),
            ("POST", ["prompts", "enhancers", "apply"]) => {
                self.enhancer_apply(req).unwrap_or_else(|e| e)
            }
            ("GET", ["health"]) => HttpResponse::json(
                200,
                &serde_json::json!({ "ok": true, "prefix": API_PREFIX }),
            ),
            ("GET", ["session"]) => HttpResponse::json(
                200,
                &serde_json::json!({
                    "context": self.context(),
                    "modules": self.catalog().modules,
                }),
            ),
            ("GET", ["catalog"]) => HttpResponse::json(
                200,
                &serde_json::to_value(self.catalog()).unwrap_or(Value::Null),
            ),
            ("GET", ["documents"]) => HttpResponse::json(
                200,
                &serde_json::json!({ "documents": self.catalog().documents }),
            ),
            ("GET", ["documents", id]) => {
                let unlock = req.query("unlock").as_deref() == Some("1")
                    || req.query("unlock").as_deref() == Some("true");
                match self.view(id, unlock) {
                    Ok(view) => no_store(HttpResponse::json(
                        200,
                        &serde_json::to_value(view).unwrap_or(Value::Null),
                    )),
                    Err(resp) => resp,
                }
            }
            ("POST", ["documents", id, "validate"]) => {
                let body: ValidateRequest = match parse_body(req) {
                    Ok(b) => b,
                    Err(resp) => return resp,
                };
                match self.validate(id, &body.draft, body.view, body.version.as_deref()) {
                    Ok(result) => no_store(HttpResponse::json(
                        200,
                        &serde_json::to_value(result).unwrap_or(Value::Null),
                    )),
                    Err(resp) => resp,
                }
            }
            ("POST", ["documents", id, "save"]) => {
                let body: SaveRequest = match parse_body(req) {
                    Ok(b) => b,
                    Err(resp) => return resp,
                };
                match self.save(id, &body) {
                    Ok(result) => no_store(HttpResponse::json(
                        200,
                        &serde_json::to_value(result).unwrap_or(Value::Null),
                    )),
                    Err(resp) => resp,
                }
            }
            ("GET", ["documents", id, "backups"]) => match self.backups(id) {
                Ok(result) => {
                    HttpResponse::json(200, &serde_json::to_value(result).unwrap_or(Value::Null))
                }
                Err(resp) => resp,
            },
            ("POST", ["documents", id, "restore"]) => {
                let body: RestoreRequest = match parse_body(req) {
                    Ok(b) => b,
                    Err(resp) => return resp,
                };
                match self.restore(id, &body) {
                    Ok(result) => no_store(HttpResponse::json(
                        200,
                        &serde_json::to_value(result).unwrap_or(Value::Null),
                    )),
                    Err(resp) => resp,
                }
            }
            ("GET", ["effective"]) => {
                let module = req.query("module").unwrap_or_default();
                match self.effective(&module) {
                    Ok(report) => HttpResponse::json(
                        200,
                        &serde_json::to_value(report).unwrap_or(Value::Null),
                    ),
                    Err(resp) => resp,
                }
            }
            ("GET", ["prompts", "preview"]) => {
                let preset = req.query("preset").unwrap_or_else(|| "code".to_string());
                let provider = req
                    .query("provider")
                    .unwrap_or_else(|| "openai".to_string());
                let model = req.query("model").unwrap_or_else(|| "gpt-4o".to_string());
                let preview = if let Some(document) = req.query("document") {
                    let resolved = match self.resolve(&document) {
                        Ok(doc) if doc.doc.id.starts_with("prompts.ref.") => doc,
                        _ => {
                            return HttpResponse::json(
                                400,
                                &serde_json::json!({"error": "请选择目录中的 Agent 工作提示词"}),
                            )
                        }
                    };
                    let agent = match crate::runtime::presets::load_spec_file(&resolved.path()) {
                        Ok(agent) => agent,
                        Err(err) => {
                            return HttpResponse::json(
                                200,
                                &serde_json::json!({"error": err.message}),
                            )
                        }
                    };
                    let prompt = match agent.prompt.load(&self.paths.cwd) {
                        Ok(prompt) => prompt,
                        Err(err) => {
                            return HttpResponse::json(
                                200,
                                &serde_json::json!({"error": err.message}),
                            )
                        }
                    };
                    self.preview_prompt_config(
                        &prompt.preset,
                        &provider,
                        &model,
                        &agent.prompt,
                        crate::runtime::harness::preview_tool_names(&agent),
                    )
                } else {
                    self.preview_effective_prompt(&preset, &provider, &model)
                };
                HttpResponse::json(200, &preview)
            }
            ("POST", ["mcp", "override"]) => {
                let body: OverrideRequest = match parse_body(req) {
                    Ok(body) => body,
                    Err(resp) => return resp,
                };
                match self.create_mcp_override(&body) {
                    Ok(result) => no_store(HttpResponse::json(
                        200,
                        &serde_json::to_value(result).unwrap_or(Value::Null),
                    )),
                    Err(resp) => resp,
                }
            }
            (_, []) => HttpResponse::json(
                200,
                &serde_json::json!({
                    "service": "one config studio",
                    "endpoints": [
                        "GET  /api/config/health",
                        "GET  /api/config/session",
                        "GET  /api/config/catalog",
                        "GET  /api/config/documents",
                        "GET  /api/config/documents/{id}?unlock=1",
                        "POST /api/config/documents/{id}/validate",
                        "POST /api/config/documents/{id}/save",
                        "GET  /api/config/documents/{id}/backups",
                        "POST /api/config/documents/{id}/restore",
                        "GET  /api/config/effective?module=settings|providers|mcp|prompts",
                        "GET  /api/config/prompts/preview?preset=...&provider=...&model=...",
                        "POST /api/config/mcp/override",
                    ]
                }),
            ),
            _ => HttpResponse::error(
                404,
                format!("未知的配置 API 路径：{} {}", req.method, req.path),
            ),
        }
    }

    /// Build the router closure handed to `one_web`.
    pub fn api_handler(self: &std::rc::Rc<Self>) -> ApiHandler {
        let this = std::rc::Rc::clone(self);
        std::rc::Rc::new(move |req: HttpRequest| {
            let this = std::rc::Rc::clone(&this);
            let is_api = req.path.starts_with(API_PREFIX);
            Box::pin(async move {
                if is_api {
                    Some(this.dispatch(&req))
                } else {
                    None
                }
            })
        })
    }
}

/// Settings effective view: resolved values from the real loader plus the
/// environment overrides that take precedence over the file.
fn settings_effective() -> EffectiveReport {
    let settings = crate::settings::load();
    let mut entries = Vec::new();

    entries.push(simple_entry(
        "provider",
        settings
            .provider
            .clone()
            .unwrap_or_else(|| "(未设置)".into()),
    ));
    entries.push(simple_entry(
        "model",
        settings
            .model
            .clone()
            .unwrap_or_else(|| "(provider 默认)".into()),
    ));
    entries.push(simple_entry(
        "thinking",
        settings
            .thinking
            .clone()
            .unwrap_or_else(|| "(默认)".to_string()),
    ));
    entries.push(simple_entry(
        "permissionMode",
        settings
            .permission_mode
            .clone()
            .unwrap_or_else(|| "default".to_string()),
    ));
    entries.push(simple_entry(
        "sandbox",
        settings
            .sandbox
            .clone()
            .unwrap_or_else(|| "workspace-write".to_string()),
    ));
    entries.push(simple_entry(
        "maxTurns",
        if settings.max_turns() == 0 {
            "unlimited".to_string()
        } else {
            settings.max_turns().to_string()
        },
    ));
    entries.push(simple_entry(
        "empty_response_retries",
        settings.empty_response_retries().to_string(),
    ));
    entries.push(simple_entry(
        "compaction",
        settings
            .compaction
            .as_ref()
            .map(|c| c.summary_line())
            .unwrap_or_else(|| "默认（auto 85% · keep 2 · prune）".to_string()),
    ));
    entries.push(simple_entry(
        "memory",
        settings
            .memory
            .as_ref()
            .map(|m| m.summary_line())
            .unwrap_or_else(|| "默认（L2 on）".to_string()),
    ));
    entries.push(simple_entry(
        "skills_config",
        match settings.skills_config.as_ref().map(|v| v.len()) {
            Some(n) if n > 0 => format!("{n} 条显式启停记录"),
            _ => "无（全部默认启用）".to_string(),
        },
    ));

    let mut overrides = Vec::new();
    for (name, affects) in [
        ("ONE_MAX_TURNS", "max_turns"),
        ("ONE_EMPTY_RESPONSE_RETRIES", "empty_response_retries"),
        ("ONE_AGENT_DIR", "整个配置目录位置"),
        ("ONE_DATA_DIR", "整个配置目录位置"),
    ] {
        if let Ok(value) = std::env::var(name) {
            overrides.push(crate::config_studio::document::OverrideInfo {
                name: name.to_string(),
                value: Some(value),
                affects: affects.to_string(),
            });
        }
    }

    EffectiveReport {
        module: ModuleId::Settings,
        note: "来自 settings.json 的实际解析结果（只读）。\
               环境变量与启动参数优先级高于文件，下面单独列出。"
            .to_string(),
        entries,
        sources: vec![crate::settings::path_display()],
        overrides,
    }
}

impl ConfigStudio {
    /// Resolve the effective prompts view: profiles, model quirks, and active enhancers.
    pub fn prompts_effective(&self) -> EffectiveReport {
        let registry = one_prompt::ComponentRegistry::with_builtins();
        let mut entries = Vec::new();

        // 1. Available Prompt Profiles (Presets)
        let presets = registry.preset_names();
        entries.push(crate::config_studio::document::EffectiveEntry {
            name: "prompt_profiles".to_string(),
            source: "one-prompt ComponentRegistry".to_string(),
            value: format!("可用预设: {}", presets.join(", ")),
            overridden: false,
        });

        // 2. Active Models & Their Quirks
        let models_cfg = match self.enhancer_models() {
            Ok(config) => config,
            Err(_) => {
                return EffectiveReport {
                    module: ModuleId::Prompts,
                    note: "models.json 无法解析，请先修复模型配置".into(),
                    entries,
                    sources: vec![],
                    overrides: vec![],
                }
            }
        };
        for provider in &models_cfg.providers {
            for m in models_cfg.registry.list_by_provider(&provider.id) {
                let quirks_str = if m.quirks.is_empty() {
                    "无".to_string()
                } else {
                    m.quirks
                        .iter()
                        .map(|q| q.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                entries.push(crate::config_studio::document::EffectiveEntry {
                    name: format!("model_behavior:{}/{}", provider.id, m.id),
                    source: "models.json".to_string(),
                    value: format!("Quirks: [{quirks_str}]"),
                    overridden: !m.quirks.is_empty(),
                });
            }
        }

        EffectiveReport {
            module: ModuleId::Prompts,
            note: "提示词系统生效视图：展示 PromptProfile 预设、已配置的模型行为 quirks 与动态 Enhancer 状态。".to_string(),
            entries,
            sources: vec!["ComponentRegistry::builtin".to_string(), "models.json".to_string()],
            overrides: vec![],
        }
    }

    /// Preview effective compiled prompt for a given preset, provider, and model.
    pub fn preview_effective_prompt(&self, preset: &str, provider: &str, model: &str) -> Value {
        let prompt_cfg = crate::prompt_config::PromptConfig {
            preset: Some(preset.to_string()),
            ..Default::default()
        };
        self.preview_prompt_config(preset, provider, model, &prompt_cfg,
            crate::runtime::harness::preview_tool_names(&crate::protocol::AgentSpec::builtin_main()))
    }

    fn preview_prompt_config(
        &self,
        preset: &str,
        provider: &str,
        model: &str,
        prompt_cfg: &crate::prompt_config::PromptConfig,
        tools: Vec<String>,
    ) -> Value {
        let models_cfg = match self.enhancer_models() {
            Ok(config) => config,
            Err(_) => return serde_json::json!({"error": "models.json 无法解析，请先修复模型配置"}),
        };
        let quirks = models_cfg
            .find_model(provider, model)
            .map(|m| m.quirks.clone())
            .unwrap_or_default();

        let env = crate::runtime::env_context::build_env_context(&self.paths.cwd);
        let compile_res = crate::runtime::prompt_compose::compile_host_scoped(
            crate::runtime::prompt_compose::HostPromptInput {
                prompt: prompt_cfg,
                cwd: &self.paths.cwd,
                provider,
                model,
                mode: crate::runtime::AgentMode::Act,
                plan_path: None,
                tool_names: tools.clone(),
                resources: "",
                env_context: Some(&env),
                memory_catalog: None,
                quirks: quirks.clone(),
            },
            &self.paths.agent_dir,
            self.global_only,
        );

        match compile_res {
            Ok((compiled, enhancers)) => {
                let applied_quirks: Vec<&str> = enhancers
                    .iter()
                    .filter(|e| e.enabled)
                    .map(|e| e.id.as_str())
                    .collect();
                let slots_summary: Vec<Value> = compiled
                    .slots
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "slot": s.id,
                            "component": s.component,
                            "emitted": s.emitted,
                            "text": s.text,
                            "operations": s.operations.len(),
                            "trace": s.operations.iter().map(|op| serde_json::json!({"source": op.source, "operation": op.operation, "rule": op.rule})).collect::<Vec<_>>(),
                        })
                    })
                    .collect();

                let config_path = self
                    .paths
                    .agent_dir
                    .join(crate::runtime::prompt_enhancer::config::RELATIVE_PATH);
                let config =
                    crate::runtime::prompt_enhancer::config::load(&config_path).unwrap_or_default();
                let presets: Vec<String> = one_prompt::ComponentRegistry::with_builtins()
                    .preset_names()
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                let model_rows: Vec<_> = models_cfg
                    .registry
                    .list()
                    .iter()
                    .map(|m| (m.provider.clone(), m.id.clone(), m.quirks.clone()))
                    .collect();
                let matches = crate::runtime::prompt_enhancer::config::explain(
                    &config,
                    &model_rows,
                    &presets,
                    provider,
                    model,
                    preset,
                    &quirks,
                );
                serde_json::json!({
                    "preset": preset,
                    "provider": provider,
                    "model": model,
                    "quirks": applied_quirks,
                    "registry_quirks": quirks,
                    "enhancers": enhancers,
                    "matches": matches,
                    "context": {"mode": "act", "tools": tools, "cwd": self.paths.cwd, "note": "使用所选工作提示词、对应 Agent 工具目录与当前目录环境进行真实编译。未附加会话专属 resources、memory、MCP 或扩展，不代表某个运行中会话的完整快照。"},
                    "compiled_prompt": compiled.text,
                    "slots": slots_summary,
                    "matched_rules": compiled.matched_rules,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "preset": preset,
                    "provider": provider,
                    "model": model,
                    "error": e.to_string(),
                })
            }
        }
    }
}

fn simple_entry(name: &str, value: String) -> crate::config_studio::document::EffectiveEntry {
    crate::config_studio::document::EffectiveEntry {
        name: name.to_string(),
        source: crate::settings::path_display(),
        value,
        overridden: false,
    }
}

/// Starter content for a document that does not exist yet.
fn scaffold(resolved: &ResolvedDoc) -> String {
    match resolved.kind {
        DocKind::Enhancers => "{\n  \"version\": 1,\n  \"bindings\": []\n}\n".into(),
        DocKind::Mcp => "{\n  \"mcpServers\": {}\n}\n".to_string(),
        DocKind::Settings | DocKind::Models | DocKind::ReadOnly => "{}\n".to_string(),
    }
}

/// Serialize a value the way the studio writes JSON files.
fn pretty_json(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
    text.push('\n');
    text
}

/// Parse a JSON request body, mapping failures to 400s.
fn parse_body<T: for<'de> Deserialize<'de>>(req: &HttpRequest) -> Result<T, HttpResponse> {
    let value = req.json_body().map_err(|e| HttpResponse::error(400, e))?;
    serde_json::from_value(value)
        .map_err(|e| HttpResponse::error(400, format!("请求体字段不正确：{e}")))
}

/// Mark responses that may contain credentials as non-cacheable.
fn no_store(resp: HttpResponse) -> HttpResponse {
    resp.with_header("Cache-Control", "no-store")
}

/// Map a save failure to its HTTP status and message.
fn save_error_response(err: SaveError) -> HttpResponse {
    HttpResponse::error(err.status(), err.message())
}

/// Convenience for tests and callers that only have a path.
pub fn path_of(resolved: &ResolvedDoc) -> &Path {
    Path::new(&resolved.doc.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-config-api-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `target` is a request target such as `/api/config/effective?module=mcp`.
    fn request(method: &str, target: &str, body: Option<Value>) -> HttpRequest {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("host".to_string(), "127.0.0.1:3333".to_string());
        let (path, raw_query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.to_string(), String::new()),
        };
        HttpRequest {
            method: method.to_string(),
            path,
            raw_query,
            headers,
            body: body
                .map(|v| serde_json::to_vec(&v).unwrap())
                .unwrap_or_default(),
        }
    }

    /// Studio over a nested project so ancestor project layers are exercised
    /// without touching the user-level MCP file (which is process-global).
    fn nested_studio(tag: &str) -> (ConfigStudio, PathBuf, PathBuf, PathBuf) {
        let agent = temp_dir(&format!("{tag}-agent"));
        let root = temp_dir(&format!("{tag}-root"));
        let nested = root.join("pkg").join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        let paths = StudioPaths {
            cwd: nested.clone(),
            agent_dir: agent.clone(),
            home: agent.clone(),
            project_chain: crate::config_studio::adapters::ancestor_chain(&nested),
        };
        (ConfigStudio::with_paths(paths), agent, root, nested)
    }

    /// Build a studio over throwaway directories.
    ///
    /// Paths are injected rather than read from the environment, so these tests
    /// run in parallel without touching process-global state.
    fn with_studio<T>(tag: &str, f: impl FnOnce(&ConfigStudio, &Path) -> T) -> T {
        let agent = temp_dir(&format!("{tag}-agent"));
        let project = temp_dir(&format!("{tag}-project"));
        fs::create_dir_all(project.join(".git")).unwrap();
        let paths = StudioPaths {
            cwd: project.clone(),
            agent_dir: agent.clone(),
            home: agent.clone(),
            project_chain: crate::config_studio::adapters::ancestor_chain(&project),
        };
        let studio = ConfigStudio::with_paths(paths);
        f(&studio, &agent)
    }

    fn body_json(resp: &HttpResponse) -> Value {
        serde_json::from_slice(&resp.body).expect("json body")
    }

    #[test]
    fn catalog_lists_modules_and_documents() {
        with_studio("catalog", |studio, _| {
            let resp = studio.dispatch(&request("GET", "/api/config/catalog", None));
            assert_eq!(resp.status, 200);
            let value = body_json(&resp);
            assert_eq!(value["modules"].as_array().unwrap().len(), 6);
            let docs = value["documents"].as_array().unwrap();
            assert!(docs.iter().any(|d| d["id"] == "settings.global"));
            assert!(docs.iter().any(|d| d["id"] == "models.global"));
            assert!(docs.iter().any(|d| d["id"] == "mcp.user"));
            // Every document carries provenance and an effect note.
            for doc in docs {
                assert!(doc["managed_by"].as_str().unwrap().len() > 0);
                assert!(doc["effect_note"].as_str().unwrap().len() > 0);
            }
        });
    }

    #[test]
    fn unknown_document_is_404() {
        with_studio("unknown", |studio, _| {
            let resp = studio.dispatch(&request("GET", "/api/config/documents/nope", None));
            assert_eq!(resp.status, 404);
        });
    }

    #[test]
    fn view_of_missing_file_returns_scaffold_and_absent_version() {
        with_studio("missing", |studio, _| {
            let resp = studio.dispatch(&request(
                "GET",
                "/api/config/documents/settings.global",
                None,
            ));
            assert_eq!(resp.status, 200);
            let value = body_json(&resp);
            assert_eq!(value["version"], VERSION_ABSENT);
            assert_eq!(value["content"], "{}\n");
            assert_eq!(value["document"]["exists"], false);
            assert!(value["form"]["fields"].as_array().unwrap().len() > 10);
        });
    }

    #[test]
    fn sensitive_document_is_masked_until_unlocked() {
        with_studio("mask", |studio, agent| {
            fs::write(
                agent.join("models.json"),
                r#"{"providers":{"x":{"apiKey":"sk-live-secret","models":[{"id":"a"}]}}}"#,
            )
            .unwrap();

            let locked = body_json(&studio.dispatch(&request(
                "GET",
                "/api/config/documents/models.global",
                None,
            )));
            assert_eq!(locked["masked"], true);
            assert!(locked["content"].as_str().unwrap().contains(REDACTED));
            assert!(!locked["content"]
                .as_str()
                .unwrap()
                .contains("sk-live-secret"));
            assert_eq!(
                locked["parsed"]["providers"]["x"]["apiKey"], REDACTED,
                "parsed form value must be masked too"
            );
            assert!(locked["form"]["masked_fields"].as_array().unwrap().len() == 1);

            let mut unlock_req = request("GET", "/api/config/documents/models.global", None);
            unlock_req.raw_query = "unlock=1".to_string();
            let unlocked = body_json(&studio.dispatch(&unlock_req));
            assert_eq!(unlocked["masked"], false);
            assert!(unlocked["content"]
                .as_str()
                .unwrap()
                .contains("sk-live-secret"));
        });
    }

    #[test]
    fn validate_reports_errors_without_writing() {
        with_studio("validate", |studio, agent| {
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/validate",
                Some(serde_json::json!({"draft": "{ broken", "view": "source"})),
            ));
            assert_eq!(resp.status, 200);
            let value = body_json(&resp);
            assert_eq!(value["valid"], false);
            assert!(!value["diagnostics"].as_array().unwrap().is_empty());
            assert!(!agent.join("settings.json").exists());
        });
    }

    #[test]
    fn validate_produces_masked_diff() {
        with_studio("diff", |studio, agent| {
            fs::write(
                agent.join("mcp.json"),
                "{\n  \"mcpServers\": {\n    \"fs\": {\n      \"command\": \"old\",\n      \"env\": {\"K\": \"sk-live-secret\"}\n    }\n  }\n}\n",
            )
            .unwrap();
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/mcp.user/validate",
                Some(serde_json::json!({
                    "view": "source",
                    "draft": "{\n  \"mcpServers\": {\n    \"fs\": {\n      \"command\": \"new\",\n      \"env\": {\"K\": \"sk-live-secret\"}\n    }\n  }\n}\n"
                })),
            ));
            let value = body_json(&resp);
            assert_eq!(value["valid"], true, "{value}");
            let diff = value["diff"].as_str().unwrap();
            assert!(diff.contains("-      \"command\": \"old\""));
            assert!(diff.contains("+      \"command\": \"new\""));
            assert!(!diff.contains("sk-live-secret"));
            assert!(diff.contains(REDACTED));
        });
    }

    #[test]
    fn save_requires_confirmation() {
        with_studio("confirm", |studio, _| {
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/save",
                Some(serde_json::json!({"draft": "{}\n", "version": "absent", "view": "source"})),
            ));
            assert_eq!(resp.status, 400);
        });
    }

    #[test]
    fn save_rejects_invalid_draft() {
        with_studio("save-invalid", |studio, agent| {
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/save",
                Some(serde_json::json!({
                    "draft": "{\"permissionMode\":\"yolo\"}",
                    "version": "absent",
                    "view": "source",
                    "confirm": true
                })),
            ));
            assert_eq!(resp.status, 422);
            assert!(!agent.join("settings.json").exists());
        });
    }

    #[test]
    fn save_writes_and_reports_version() {
        with_studio("save-ok", |studio, agent| {
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/save",
                Some(serde_json::json!({
                    "draft": "{\n  \"provider\": \"xai\"\n}\n",
                    "version": VERSION_ABSENT,
                    "view": "source",
                    "confirm": true
                })),
            ));
            assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
            let value = body_json(&resp);
            assert_eq!(value["created"], true);
            assert_eq!(
                fs::read_to_string(agent.join("settings.json")).unwrap(),
                "{\n  \"provider\": \"xai\"\n}\n"
            );
            assert_eq!(value["version"].as_str().unwrap().len(), 64);
        });
    }

    #[test]
    fn save_conflicts_when_file_changed() {
        with_studio("save-conflict", |studio, agent| {
            let path = agent.join("settings.json");
            fs::write(&path, "{}\n").unwrap();
            let stale = content_version(Some("{}\n"));

            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/save",
                Some(serde_json::json!({
                    "draft": "{\n  \"provider\": \"xai\"\n}\n",
                    "version": stale,
                    "view": "source",
                    "confirm": true
                })),
            ));
            assert_eq!(resp.status, 200);

            // An external editor changes the file, then the stale tab retries.
            fs::write(&path, "{\n  \"provider\": \"manual\"\n}\n").unwrap();
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/save",
                Some(serde_json::json!({
                    "draft": "{\n  \"provider\": \"xai\"\n}\n",
                    "version": stale,
                    "view": "source",
                    "confirm": true
                })),
            ));
            assert_eq!(resp.status, 409);
            assert!(fs::read_to_string(&path).unwrap().contains("manual"));
        });
    }

    #[test]
    fn validate_conflicts_when_file_changed_under_the_edit() {
        with_studio("validate-conflict", |studio, agent| {
            let path = agent.join("settings.json");
            let original = "{\n  \"provider\": \"openai\"\n}\n";
            fs::write(&path, original).unwrap();
            let base = content_version(Some(original));

            fs::write(&path, "{\n  \"provider\": \"anthropic\"\n}\n").unwrap();

            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/validate",
                Some(serde_json::json!({
                    "view": "source",
                    "version": base,
                    "draft": "{\n  \"provider\": \"openai\",\n  \"model\": \"gpt-4o\"\n}\n"
                })),
            ));
            assert_eq!(resp.status, 409);
            assert!(fs::read_to_string(&path).unwrap().contains("anthropic"));
        });
    }

    #[test]
    fn form_rename_of_masked_entry_is_rejected() {
        with_studio("form-rename", |studio, agent| {
            let path = agent.join("mcp.json");
            let original = "{\n  \"mcpServers\": {\n    \"old\": {\n      \"command\": \"npx\",\n      \"env\": {\"SECRET\": \"sk-live-secret\"}\n    }\n  }\n}\n";
            fs::write(&path, original).unwrap();

            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/mcp.user/save",
                Some(serde_json::json!({
                    "view": "form",
                    "version": content_version(Some(original)),
                    "confirm": true,
                    "draft": serde_json::json!({
                        "mcpServers": {
                            "renamed": {
                                "command": "npx",
                                "env": { "SECRET": REDACTED }
                            }
                        }
                    }).to_string(),
                })),
            ));
            assert_eq!(resp.status, 422, "{:?}", body_json(&resp));
            let written = fs::read_to_string(&path).unwrap();
            assert!(written.contains("sk-live-secret"));
            assert!(written.contains("\"old\""));
            assert!(!written.contains("renamed"));
        });
    }

    #[test]
    fn form_save_keeps_untouched_secret() {
        with_studio("form-keep", |studio, agent| {
            let path = agent.join("models.json");
            let original = "{\n  \"providers\": {\n    \"x\": {\n      \"apiKey\": \"sk-live-secret\",\n      \"baseUrl\": \"https://a\"\n    }\n  }\n}\n";
            fs::write(&path, original).unwrap();
            let version = content_version(Some(original));

            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/models.global/save",
                Some(serde_json::json!({
                    "view": "form",
                    "version": version,
                    "confirm": true,
                    "draft": serde_json::json!({
                        "providers": { "x": { "apiKey": REDACTED, "baseUrl": "https://b" } }
                    }).to_string(),
                })),
            ));
            assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
            let written = fs::read_to_string(&path).unwrap();
            assert!(
                written.contains("sk-live-secret"),
                "secret must survive: {written}"
            );
            assert!(written.contains("https://b"));
            assert!(!written.contains(REDACTED));
        });
    }

    #[test]
    fn form_save_can_clear_a_secret() {
        with_studio("form-clear", |studio, agent| {
            let path = agent.join("models.json");
            let original = "{\n  \"providers\": {\n    \"x\": {\n      \"apiKey\": \"sk-live-secret\"\n    }\n  }\n}\n";
            fs::write(&path, original).unwrap();

            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/models.global/save",
                Some(serde_json::json!({
                    "view": "form",
                    "version": content_version(Some(original)),
                    "confirm": true,
                    "draft": serde_json::json!({ "providers": { "x": {} } }).to_string(),
                })),
            ));
            assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
            let written = fs::read_to_string(&path).unwrap();
            assert!(!written.contains("sk-live-secret"));
            assert!(!written.contains("apiKey"));
        });
    }

    #[test]
    fn sensitive_source_save_requires_unlock() {
        with_studio("source-unlock", |studio, agent| {
            let original = "{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret\"}}}\n";
            fs::write(agent.join("models.json"), original).unwrap();
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/models.global/save",
                Some(serde_json::json!({
                    "view": "source",
                    "version": content_version(Some(original)),
                    "confirm": true,
                    // No `unlocked`, and the draft still holds the real secret.
                    "draft": "{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret-2\"}}}\n",
                })),
            ));
            assert_eq!(resp.status, 403);
            assert_eq!(
                fs::read_to_string(agent.join("models.json")).unwrap(),
                original
            );
        });
    }

    #[test]
    fn unlocked_source_save_is_allowed() {
        with_studio("source-unlocked", |studio, agent| {
            let original = "{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret\"}}}\n";
            fs::write(agent.join("models.json"), original).unwrap();
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/models.global/save",
                Some(serde_json::json!({
                    "view": "source",
                    "version": content_version(Some(original)),
                    "confirm": true,
                    "unlocked": true,
                    "draft": "{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret-2\"}}}\n",
                })),
            ));
            assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
            assert!(fs::read_to_string(agent.join("models.json"))
                .unwrap()
                .contains("sk-live-secret-2"));
        });
    }

    #[test]
    fn validate_never_returns_real_secrets() {
        with_studio("validate-secret", |studio, agent| {
            let original = "{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret\",\"baseUrl\":\"https://a\"}}}\n";
            fs::write(agent.join("models.json"), original).unwrap();
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/models.global/validate",
                Some(serde_json::json!({
                    "view": "form",
                    "draft": serde_json::json!({
                        "providers": { "x": { "apiKey": REDACTED, "baseUrl": "https://b" } }
                    }).to_string(),
                })),
            ));
            assert_eq!(resp.status, 200);
            let raw = String::from_utf8_lossy(&resp.body);
            assert!(
                !raw.contains("sk-live-secret"),
                "validate leaked a secret: {raw}"
            );
            assert!(raw.contains(REDACTED));
            assert!(body_json(&resp)["preview_text"]
                .as_str()
                .unwrap()
                .contains("https://b"));
        });
    }

    #[test]
    fn source_save_of_masked_draft_is_rejected() {
        with_studio("source-masked", |studio, agent| {
            fs::write(
                agent.join("models.json"),
                "{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret\"}}}\n",
            )
            .unwrap();
            let resp = studio.dispatch(&request(
                "POST",
                "/api/config/documents/models.global/save",
                Some(serde_json::json!({
                    "view": "source",
                    "version": content_version(Some("{\"providers\":{\"x\":{\"apiKey\":\"sk-live-secret\"}}}\n")),
                    "confirm": true,
                    "draft": format!("{{\"providers\":{{\"x\":{{\"apiKey\":\"{REDACTED}\"}}}}}}"),
                })),
            ));
            assert_eq!(resp.status, 422);
        });
    }

    #[test]
    fn read_only_documents_cannot_be_saved() {
        with_studio("readonly", |studio, _| {
            let docs = studio.documents();
            let ro = docs
                .iter()
                .find(|d| d.doc.module == ModuleId::Extensions && !d.writable())
                .expect("a read-only extension document");
            let resp = studio.dispatch(&request(
                "POST",
                &format!("/api/config/documents/{}/save", ro.doc.id),
                Some(serde_json::json!({
                    "draft": "{}\n", "version": VERSION_ABSENT, "view": "source", "confirm": true
                })),
            ));
            assert_eq!(resp.status, 403);
        });
    }

    #[test]
    fn backups_can_be_listed_and_restored() {
        with_studio("restore", |studio, agent| {
            let path = agent.join("settings.json");
            fs::write(&path, "{\n  \"provider\": \"first\"\n}\n").unwrap();

            let v1 = content_version(Some("{\n  \"provider\": \"first\"\n}\n"));
            let save = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/save",
                Some(serde_json::json!({
                    "draft": "{\n  \"provider\": \"second\"\n}\n",
                    "version": v1, "view": "source", "confirm": true
                })),
            ));
            assert_eq!(save.status, 200);
            let v2 = body_json(&save)["version"].as_str().unwrap().to_string();

            let list = body_json(&studio.dispatch(&request(
                "GET",
                "/api/config/documents/settings.global/backups",
                None,
            )));
            let backups = list["backups"].as_array().unwrap();
            assert_eq!(backups.len(), 1);
            let backup_id = backups[0]["id"].as_str().unwrap();

            let restore = studio.dispatch(&request(
                "POST",
                "/api/config/documents/settings.global/restore",
                Some(serde_json::json!({ "backup": backup_id, "version": v2 })),
            ));
            assert_eq!(restore.status, 200, "{:?}", body_json(&restore));
            assert!(fs::read_to_string(&path).unwrap().contains("first"));
            // The displaced content is itself backed up by the restore.
            assert!(body_json(&restore)["backup"].is_object());
        });
    }

    #[test]
    fn effective_settings_are_available() {
        with_studio("effective", |studio, _| {
            let resp = studio.dispatch(&request(
                "GET",
                "/api/config/effective?module=settings",
                None,
            ));
            assert_eq!(resp.status, 200);
            let value = body_json(&resp);
            assert!(value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "sandbox"));
            assert!(value["note"].as_str().unwrap().contains("只读"));
        });
    }

    #[test]
    fn effective_rejects_unsupported_module() {
        with_studio("effective-bad", |studio, _| {
            let resp =
                studio.dispatch(&request("GET", "/api/config/effective?module=agents", None));
            assert_eq!(resp.status, 400);
        });
    }

    const SRV: &str = "one-studio-test-srv";

    #[test]
    fn mcp_override_copies_entry_into_project_document() {
        let (studio, _agent, root, nested) = nested_studio("override");

        // The winning entry lives in the ancestor project layer, so this needs no
        // user-level file and stays hermetic.
        let ancestor = crate::config_studio::adapters::mcp::project_path(&root);
        fs::create_dir_all(ancestor.parent().unwrap()).unwrap();
        fs::write(
            &ancestor,
            format!(
                "{{\n  \"mcpServers\": {{\n    \"{SRV}\": {{\n      \"command\": \"ancestor-cmd\",\n      \"env\": {{\"K\": \"v\"}},\n      \"customField\": 7\n    }}\n  }}\n}}\n"
            ),
        )
        .unwrap();

        let project_doc = studio
            .documents()
            .into_iter()
            .find(|d| {
                d.doc.path
                    == crate::config_studio::adapters::mcp::project_path(&nested)
                        .display()
                        .to_string()
            })
            .expect("cwd project mcp document");

        let resp = studio.dispatch(&request(
            "POST",
            "/api/config/mcp/override",
            Some(serde_json::json!({
                "server": SRV,
                "target_doc": project_doc.doc.id
            })),
        ));
        assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
        let value = body_json(&resp);
        assert!(value["note"].as_str().unwrap().contains("整条替换"));
        assert!(value["note"].as_str().unwrap().contains("继承"));
        assert_eq!(value["entry"]["command"], "ancestor-cmd");

        let written = fs::read_to_string(path_of(&project_doc)).unwrap();
        assert!(written.contains("ancestor-cmd"));
        // Fields the studio does not model survive the copy verbatim.
        assert!(written.contains("customField"));
        assert!(value["diff"].as_str().unwrap().contains("ancestor-cmd"));
    }

    #[test]
    fn mcp_override_rejects_unknown_server() {
        let (studio, _agent, _root, _nested) = nested_studio("override-missing");
        let project_doc = studio
            .documents()
            .into_iter()
            .find(|d| {
                d.doc.scope == crate::config_studio::document::Scope::Project
                    && d.doc.module == ModuleId::Mcp
            })
            .unwrap();
        let resp = studio.dispatch(&request(
            "POST",
            "/api/config/mcp/override",
            Some(serde_json::json!({
                "server": "does-not-exist",
                "target_doc": project_doc.doc.id
            })),
        ));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn api_handler_ignores_non_config_paths() {
        with_studio("fallthrough", |studio, _agent| {
            let studio = std::rc::Rc::new(ConfigStudio::with_paths(studio.paths().clone()));
            let handler = studio.api_handler();
            let fut = handler(request("GET", "/api/info", None));
            let out = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(fut);
            assert!(out.is_none());
        });
    }
}
