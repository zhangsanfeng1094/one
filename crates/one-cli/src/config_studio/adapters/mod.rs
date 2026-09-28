//! Module adapters: one per configuration module.
//!
//! Each adapter reuses the module's **real** loader for validation and source
//! resolution so the studio can never drift from runtime behaviour. The studio
//! deliberately does *not* re-implement parsing rules; it only adds a
//! declarative field schema for form rendering.
//!
//! Raw JSON is the edit substrate rather than the typed structs. Loading a
//! document into `Settings` / `McpConfig` and serializing it back would silently
//! drop fields written by a newer version, which `docs/web-config.md` §3 forbids.

pub mod enhancers;
pub mod mcp;
pub mod models;
pub mod readonly;
pub mod settings;

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::document::{ConfigDocument, Diagnostic, FormModel};

/// Which module an adapter serves, used for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// `settings.json` — validated with `crate::settings::Settings`.
    Settings,
    /// `mcp.json` — validated with `one_mcp::config::parse_config_json`.
    Mcp,
    /// `models.json` — validated with `one_ai::try_parse_models_file`.
    Models,
    Enhancers,
    /// Anything the studio only displays.
    ReadOnly,
}

/// Filesystem context the studio resolves documents against.
#[derive(Debug, Clone)]
pub struct StudioPaths {
    /// Working directory the studio was started in.
    pub cwd: PathBuf,
    /// Effective agent home (`ONE_AGENT_DIR` honoured).
    pub agent_dir: PathBuf,
    /// User home, used for cross-client compat paths.
    pub home: PathBuf,
    /// Ancestor directories that participate in project resolution, nearest last.
    pub project_chain: Vec<PathBuf>,
}

impl StudioPaths {
    /// Detect the studio's context from the environment.
    pub fn detect(cwd: PathBuf) -> Self {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| {
            if cwd.is_absolute() {
                cwd.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&cwd)
            }
        });
        let agent_dir = one_session::agent_dir();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let project_chain = ancestor_chain(&cwd);
        Self {
            cwd,
            agent_dir,
            home,
            project_chain,
        }
    }

    /// Whether the agent home was overridden away from its default location.
    pub fn agent_dir_overridden(&self) -> bool {
        std::env::var("ONE_AGENT_DIR")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
            || std::env::var("ONE_DATA_DIR")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
    }
}

/// Ancestor directories from the outermost project root down to `cwd`.
///
/// Deliberately identical to the MCP loader's `walk_dirs_to_git_root` so the
/// studio lists exactly the files the runtime would consider.
pub fn ancestor_chain(cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut cur = cwd.to_path_buf();
    loop {
        chain.push(cur.clone());
        if cur.join(".git").exists() {
            break;
        }
        if !cur.pop() {
            break;
        }
    }
    chain.reverse();
    chain
}

/// A catalog document together with the bookkeeping the API needs.
#[derive(Debug, Clone)]
pub struct ResolvedDoc {
    /// The public document description sent to the browser.
    pub doc: ConfigDocument,
    /// Root the document must stay inside for writes (containment check).
    pub root: PathBuf,
    /// Adapter that owns validation and form rendering.
    pub kind: DocKind,
}

impl ResolvedDoc {
    /// Filesystem path of the document.
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.doc.path)
    }

    /// Whether the studio may write this document.
    pub fn writable(&self) -> bool {
        self.doc.capabilities.write || self.doc.capabilities.create
    }
}

/// Outcome of validating a draft.
#[derive(Debug, Clone)]
pub struct Validation {
    /// Findings, ordered by severity then position.
    pub diagnostics: Vec<Diagnostic>,
    /// Parsed (still unmasked) value when the draft is syntactically valid.
    pub parsed: Option<Value>,
}

impl Validation {
    /// A draft that parsed cleanly.
    pub fn ok(parsed: Value) -> Self {
        Self {
            diagnostics: Vec::new(),
            parsed: Some(parsed),
        }
    }

    /// Whether any finding blocks saving.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Build the full document catalog for the current context.
pub fn resolve_documents(paths: &StudioPaths) -> Vec<ResolvedDoc> {
    let mut docs = Vec::new();
    docs.extend(settings::documents(paths));
    docs.extend(mcp::documents(paths));
    docs.extend(models::documents(paths));
    docs.extend(enhancers::documents(paths));
    docs.extend(readonly::documents(paths));
    docs.sort_by(|a, b| {
        (a.doc.module, a.doc.precedence, &a.doc.id).cmp(&(
            b.doc.module,
            b.doc.precedence,
            &b.doc.id,
        ))
    });
    docs
}

/// Look up a document by its catalog id.
pub fn find<'a>(docs: &'a [ResolvedDoc], id: &str) -> Option<&'a ResolvedDoc> {
    docs.iter().find(|d| d.doc.id == id)
}

/// Validate a draft with the adapter that owns `kind`.
pub fn validate(kind: DocKind, draft: &str) -> Validation {
    match kind {
        DocKind::Settings => settings::validate(draft),
        DocKind::Mcp => mcp::validate(draft),
        DocKind::Models => models::validate(draft),
        DocKind::Enhancers => enhancers::validate(draft),
        DocKind::ReadOnly => {
            // Read-only documents have no schema; treat any text as acceptable
            // but never expose a form.
            Validation {
                diagnostics: Vec::new(),
                parsed: None,
            }
        }
    }
}

/// Build the form model for a parsed value, or `None` when the module has no
/// declarative form (read-only documents, or formats without structure).
pub fn form(kind: DocKind, value: &Value) -> Option<FormModel> {
    match kind {
        DocKind::Settings => Some(settings::form(value)),
        DocKind::Mcp => Some(mcp::form(value)),
        DocKind::Models => Some(models::form(value)),
        DocKind::ReadOnly | DocKind::Enhancers => None,
    }
}

/// Locate the 1-based line/column of a serde error position, when reported.
pub fn locate(text: &str, offset: Option<usize>) -> Option<(usize, usize)> {
    let offset = offset?;
    let mut line = 1usize;
    let mut column = 1usize;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Some((line, column))
}

/// Sort findings so errors surface first and locations read top-to-bottom.
pub fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by_key(|d| {
        let severity_rank = match d.severity {
            super::document::Severity::Error => 0,
            super::document::Severity::Warning => 1,
            super::document::Severity::Info => 2,
        };
        (severity_rank, d.line.unwrap_or(usize::MAX))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn relative_studio_cwd_matches_runtime_canonical_project_chain() {
        let paths = StudioPaths::detect(PathBuf::from("."));
        assert!(paths.cwd.is_absolute());
        assert_eq!(paths.cwd, std::fs::canonicalize(".").unwrap());
        assert_eq!(
            paths.project_chain,
            crate::runtime::prompt_enhancer::config::project_roots(&paths.cwd)
        );
    }

    #[test]
    fn ancestor_chain_stops_at_git_root() {
        let root = std::env::temp_dir().join(format!("one-config-chain-{}", std::process::id()));
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();

        let chain = ancestor_chain(&nested);
        assert_eq!(chain.last(), Some(&nested));
        assert_eq!(chain.first(), Some(&root));
        assert!(chain.len() >= 3);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ancestor_chain_handles_relative_cwd() {
        let chain = ancestor_chain(Path::new("."));
        assert!(!chain.is_empty());
    }

    #[test]
    fn locate_maps_offsets_to_line_and_column() {
        let text = "{\n  \"a\": 1,\n  \"b\":\n}\n";
        // Offset of the `1`.
        let offset = text.find('1').unwrap();
        assert_eq!(locate(text, Some(offset)), Some((2, 8)));
        assert_eq!(locate(text, None), None);
    }

    #[test]
    fn diagnostics_sort_errors_first() {
        use super::super::document::Diagnostic;
        let mut d = vec![
            Diagnostic::warning("w").at(1, 1),
            Diagnostic::info("i"),
            Diagnostic::error("e").at(9, 1),
        ];
        sort_diagnostics(&mut d);
        assert_eq!(d[0].message, "e");
        assert_eq!(d[1].message, "w");
        assert_eq!(d[2].message, "i");
    }
}
