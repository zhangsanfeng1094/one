//! Cross-session memory — L2 index (catalog) only.
//!
//! Layout (see `docs/memory.md`):
//! ```text
//! ~/.one/agent/memory/
//!   _global/MEMORY.md
//!   projects/<slug-hash8>/MEMORY.md
//! ```
//!
//! Bodies live next to the index and are loaded only via `read` / `grep`.
//! The catalog is injected into the system prompt at session boot and frozen
//! for the session (do not re-scan every turn).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Default max index lines from global + project combined (after parse).
pub const DEFAULT_INDEX_MAX_LINES: usize = 80;

/// Default max memory `read`/`grep` ops per user turn (M3).
pub const DEFAULT_MAX_LOOKUPS_PER_TURN: usize = 6;

/// Settings / runtime options for memory L2 load + write policy.
#[derive(Debug, Clone)]
pub struct MemoryLoadOptions {
    pub enabled: bool,
    pub index_max_lines: usize,
    /// Allow write/edit under memory roots (M2; default true).
    pub write_enabled: bool,
    /// Max memory path lookups per user turn (M3).
    pub max_lookups_per_turn: usize,
}

impl Default for MemoryLoadOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            index_max_lines: DEFAULT_INDEX_MAX_LINES,
            write_enabled: true,
            max_lookups_per_turn: DEFAULT_MAX_LOOKUPS_PER_TURN,
        }
    }
}

/// One L2 catalog entry (parsed or synthetic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndexEntry {
    pub id: String,
    pub type_name: String,
    pub scope: String,
    pub tags: String,
    pub description: String,
    /// Absolute path to body file if known.
    pub body_path: Option<PathBuf>,
}

/// Extra matching controls stored in a tool-intent body's YAML frontmatter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolIntentMetadata {
    triggers: Vec<String>,
    negative_triggers: Vec<String>,
    priority: i32,
}

fn normalize_metadata_items(items: impl IntoIterator<Item = String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

/// One matched tool-intent rule for runtime prompt injection.
#[derive(Debug, Clone)]
pub struct ToolIntentHit {
    pub score: u32,
    pub confidence: f32,
    pub evidence: Vec<String>,
    pub entry: MemoryIndexEntry,
    pub body_excerpt: Option<String>,
}

/// Result of loading memory indexes for a workspace.
#[derive(Debug, Clone)]
pub struct MemoryCatalog {
    pub global_dir: PathBuf,
    pub project_dir: PathBuf,
    pub project_slug: String,
    pub entries: Vec<MemoryIndexEntry>,
    /// Rendered system-prompt section (empty if no entries and no dirs to mention).
    pub prompt_section: String,
}

/// Readable roots so `read` / `grep` can open memory bodies under path policy.
pub fn memory_readable_roots(agent_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let root = memory_root(agent_dir);
    let project = project_memory_dir(agent_dir, cwd);
    vec![root.clone(), root.join("_global"), project]
}

/// Writable roots for M2 agent write path (same trees as readable).
pub fn memory_writable_roots(agent_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let root = memory_root(agent_dir);
    vec![root.join("_global"), project_memory_dir(agent_dir, cwd)]
}

/// Ensure `_global` + project memory dirs exist (so path policy can grant them).
pub async fn ensure_memory_dirs(agent_dir: &Path, cwd: &Path) -> std::io::Result<()> {
    for dir in memory_writable_roots(agent_dir, cwd) {
        tokio::fs::create_dir_all(&dir).await?;
    }
    Ok(())
}

/// Format one L2 index bullet (for agent write discipline).
pub fn format_index_entry_line(
    id: &str,
    type_name: &str,
    scope: &str,
    tags: &str,
    description: &str,
) -> String {
    format!("- [{id}] type={type_name} scope={scope} tags={tags} — {description}")
}

/// Minimal body scaffold with frontmatter.
pub fn scaffold_memory_body(
    name: &str,
    type_name: &str,
    scope: &str,
    tags: &str,
    body: &str,
) -> String {
    let today = chrono_ymd_today();
    let tags_fm = normalize_tags_for_frontmatter(tags);
    format!(
        "---\n\
         name: {name}\n\
         type: {type_name}\n\
         scope: {scope}\n\
         tags: [{tags_fm}]\n\
         updated: {today}\n\
         ---\n\n\
         {body}\n"
    )
}

fn render_tool_intent_frontmatter(
    name: &str,
    type_name: &str,
    scope: &str,
    tags: &str,
    triggers: &[String],
    negative_triggers: &[String],
    priority: i32,
) -> String {
    let mut out = format!(
        "---\nname: {name}\ntype: {type_name}\nscope: {scope}\ntags: [{}]\nupdated: {}\n",
        normalize_tags_for_frontmatter(tags),
        chrono_ymd_today()
    );
    if !triggers.is_empty() {
        out.push_str(&format!("triggers: [{}]\n", render_metadata_list(triggers)));
    }
    if !negative_triggers.is_empty() {
        out.push_str(&format!(
            "negative_triggers: [{}]\n",
            render_metadata_list(negative_triggers)
        ));
    }
    if priority != 0 {
        out.push_str(&format!("priority: {priority}\n"));
    }
    out.push_str("---\n\n");
    out
}

fn render_metadata_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| {
            let escaped = item.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Input for atomic memory write (body + MEMORY.md index line).
#[derive(Debug, Clone, Default)]
pub struct MemoryUpsertInput {
    pub id: String,
    /// `global` or `project` (default project).
    pub scope: String,
    /// `feedback` | `user` | `project` | `reference` (default project).
    pub type_name: String,
    /// Comma-separated tags.
    pub tags: String,
    /// One-line L2 description (required for catalog routing).
    pub description: String,
    /// Body markdown (frontmatter optional; tool rebuilds it).
    pub body: String,
    /// Optional frontmatter `name` (defaults to description or id).
    pub name: Option<String>,
    /// Optional explicit phrases that strongly activate a tool-intent rule.
    pub triggers: Vec<String>,
    /// Optional phrases that veto a tool-intent rule.
    pub negative_triggers: Vec<String>,
    /// Optional rule priority. Higher values win ties and add a small score bonus.
    pub priority: i32,
}

/// Result of [`upsert_memory_entry`].
#[derive(Debug, Clone)]
pub struct MemoryUpsertResult {
    pub id: String,
    pub scope: String,
    pub type_name: String,
    pub tags: String,
    pub description: String,
    pub body_path: PathBuf,
    pub index_path: PathBuf,
    /// True when the body file did not exist before this write.
    pub created: bool,
    /// True when an existing index line for this id was replaced.
    pub index_updated: bool,
}

/// Validate / normalize a memory id for use as a filename stem.
pub fn validate_memory_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("id is empty".into());
    }
    if id.eq_ignore_ascii_case("MEMORY") {
        return Err("id cannot be MEMORY (reserved for the index file)".into());
    }
    if id.len() > 64 {
        return Err("id too long (max 64 chars)".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("id must be alphanumeric / `_` / `-` only".into());
    }
    Ok(id.to_string())
}

fn normalize_scope(scope: &str) -> Result<String, String> {
    match scope.trim().to_ascii_lowercase().as_str() {
        "" | "project" | "proj" | "repo" => Ok("project".into()),
        "global" | "user" | "g" => Ok("global".into()),
        other => Err(format!("scope must be global|project (got `{other}`)")),
    }
}

fn normalize_type_name(ty: &str) -> String {
    match ty.trim().to_ascii_lowercase().as_str() {
        "" | "project" => "project".into(),
        "feedback" | "pref" | "preference" => "feedback".into(),
        "user" | "profile" => "user".into(),
        "reference" | "ref" | "l4" => "reference".into(),
        other => {
            // Allow custom types but keep safe chars only.
            let safe: String = other
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(32)
                .collect();
            if safe.is_empty() {
                "project".into()
            } else {
                safe
            }
        }
    }
}

fn normalize_tags_csv(tags: &str) -> String {
    tags.split(|c: char| c == ',' || c == ';')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_tags_for_frontmatter(tags: &str) -> String {
    tags.split(|c: char| c == ',' || c == ';')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn one_line_description(desc: &str) -> Result<String, String> {
    let d = desc
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();
    if d.is_empty() {
        return Err("description is empty (need a one-line L2 summary)".into());
    }
    Ok(d)
}

/// Strip YAML frontmatter if present so we can re-scaffold cleanly.
pub fn strip_memory_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.trim();
    }
    let after = match trimmed.strip_prefix("---") {
        Some(a) => a,
        None => return content.trim(),
    };
    // Find closing --- on its own line.
    let mut rest = after;
    if rest.starts_with('\n') {
        rest = &rest[1..];
    } else if let Some(pos) = rest.find('\n') {
        rest = &rest[pos + 1..];
    }
    if let Some(end) = rest.find("\n---") {
        let after_close = &rest[end + 4..];
        return after_close.trim_start_matches('\n').trim();
    }
    content.trim()
}

/// Atomically write/update a memory body **and** matching MEMORY.md index line (M6).
///
/// L2 catalog in the running session stays frozen until `/reload` or a new session;
/// this only updates disk.
pub fn upsert_memory_entry(
    agent_dir: &Path,
    cwd: &Path,
    input: &MemoryUpsertInput,
) -> Result<MemoryUpsertResult, String> {
    let id = validate_memory_id(&input.id)?;
    let scope = normalize_scope(&input.scope)?;
    let type_name = normalize_type_name(&input.type_name);
    let tags = normalize_tags_csv(&input.tags);
    let description = one_line_description(&input.description)?;
    let body_core = strip_memory_frontmatter(&input.body);
    if body_core.trim().is_empty() {
        return Err("body is empty".into());
    }

    let dir = if scope == "global" {
        memory_root(agent_dir).join("_global")
    } else {
        project_memory_dir(agent_dir, cwd)
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("create memory dir: {e}"))?;

    let body_path = dir.join(format!("{id}.md"));
    let index_path = dir.join("MEMORY.md");
    let created = !body_path.exists();

    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(80).collect::<String>())
        .unwrap_or_else(|| description.chars().take(80).collect());

    let triggers = normalize_metadata_items(input.triggers.iter().cloned());
    let negative_triggers = normalize_metadata_items(input.negative_triggers.iter().cloned());
    let priority = input.priority.clamp(-100, 100);
    let body_text = if type_name == "tool_intent"
        && (!triggers.is_empty() || !negative_triggers.is_empty() || priority != 0)
    {
        format!(
            "{}{}",
            render_tool_intent_frontmatter(
                &name,
                &type_name,
                &scope,
                &tags,
                &triggers,
                &negative_triggers,
                priority,
            ),
            body_core
        )
    } else {
        scaffold_memory_body(&name, &type_name, &scope, &tags, body_core)
    };
    std::fs::write(&body_path, &body_text).map_err(|e| format!("write body: {e}"))?;

    let index_updated =
        upsert_memory_index_line(&index_path, &id, &type_name, &scope, &tags, &description)?;

    Ok(MemoryUpsertResult {
        id,
        scope,
        type_name,
        tags,
        description,
        body_path,
        index_path,
        created,
        index_updated,
    })
}

/// Insert or replace one bullet in MEMORY.md; preserves a leading header block.
fn upsert_memory_index_line(
    index_path: &Path,
    id: &str,
    type_name: &str,
    scope: &str,
    tags: &str,
    description: &str,
) -> Result<bool, String> {
    let existing = if index_path.exists() {
        std::fs::read_to_string(index_path).map_err(|e| format!("read MEMORY.md: {e}"))?
    } else {
        String::new()
    };

    let (header, mut entries) =
        split_index_header_and_entries(&existing, scope, index_path.parent());
    let new_line = format_index_entry_line(id, type_name, scope, tags, description);
    let mut updated = false;
    if let Some(pos) = entries.iter().position(|(eid, _)| eid == id) {
        entries[pos] = (id.to_string(), new_line);
        updated = true;
    } else {
        entries.push((id.to_string(), new_line));
    }

    let mut out = String::new();
    if header.trim().is_empty() {
        out.push_str(
            "# Memory index (do not treat as full instructions; read bodies on demand)\n\n",
        );
    } else {
        out.push_str(&header);
        if !header.ends_with('\n') {
            out.push('\n');
        }
        if !header.ends_with("\n\n") && !entries.is_empty() {
            // ensure blank line before bullets when header has no trailing blank
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
        }
    }
    for (_, line) in &entries {
        out.push_str(line);
        out.push('\n');
    }

    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create index dir: {e}"))?;
    }
    std::fs::write(index_path, out).map_err(|e| format!("write MEMORY.md: {e}"))?;
    Ok(updated)
}

/// Split MEMORY.md into preamble (comments/titles) and parsed bullet lines (id, rendered line).
fn split_index_header_and_entries(
    text: &str,
    default_scope: &str,
    parent: Option<&Path>,
) -> (String, Vec<(String, String)>) {
    let parsed = parse_memory_index(text, default_scope, parent);
    if parsed.is_empty() {
        // Keep entire file as header if no bullets yet.
        let header = text
            .lines()
            .filter(|l| {
                let t = l.trim();
                !(t.starts_with("- ") || t.starts_with("* "))
            })
            .collect::<Vec<_>>()
            .join("\n");
        return (header, Vec::new());
    }

    // Header = lines before first list item that looks like a memory bullet.
    let mut header_lines = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if (t.starts_with("- ") || t.starts_with("* ")) && t.contains('[') {
            break;
        }
        header_lines.push(line);
    }
    // Trim trailing blank lines from header.
    while header_lines
        .last()
        .map(|l| l.trim().is_empty())
        .unwrap_or(false)
    {
        header_lines.pop();
    }
    let header = header_lines.join("\n");
    if !header.is_empty() {
        let header = format!("{header}\n");
        let entries: Vec<_> = parsed
            .into_iter()
            .map(|e| {
                let line =
                    format_index_entry_line(&e.id, &e.type_name, &e.scope, &e.tags, &e.description);
                (e.id, line)
            })
            .collect();
        return (header, entries);
    }

    let entries: Vec<_> = parsed
        .into_iter()
        .map(|e| {
            let line =
                format_index_entry_line(&e.id, &e.type_name, &e.scope, &e.tags, &e.description);
            (e.id, line)
        })
        .collect();
    (String::new(), entries)
}

fn chrono_ymd_today() -> String {
    // Avoid chrono dep: local date via system clock + UTC offset best-effort.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Approximate UTC date (good enough for scaffold timestamps).
    let days = (secs / 86_400) as i64;
    // Civil from days (Hinnant); epoch day 0 = 1970-01-01.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `~/.one/agent/memory` (or `{agent_dir}/memory`).
pub fn memory_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("memory")
}

/// Project memory directory under `memory/projects/<slug-hash8>/`.
pub fn project_memory_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let slug = project_slug(cwd);
    memory_root(agent_dir).join("projects").join(slug)
}

/// Stable project identity: `origin` org/repo when available, else path slug + hash.
pub fn project_slug(cwd: &Path) -> String {
    if let Some(remote) = git_origin_slug(cwd) {
        let hash = short_hash(&remote);
        let safe: String = remote
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let safe = safe.trim_matches('-');
        let safe = if safe.is_empty() { "repo" } else { safe };
        // Cap slug length for filesystem friendliness.
        let head: String = safe.chars().take(48).collect();
        return format!("{head}-{hash}");
    }
    let path = cwd.to_string_lossy();
    let hash = short_hash(path.as_ref());
    let base = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let safe: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{safe}-{hash}")
}

fn short_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", (h.finish() as u32))
}

fn git_origin_slug(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        return None;
    }
    // git@github.com:org/repo.git  or  https://github.com/org/repo.git
    let stripped = url.trim_end_matches('/').trim_end_matches(".git");
    if let Some(rest) = stripped.strip_prefix("git@") {
        // host:org/repo
        if let Some((_, path)) = rest.split_once(':') {
            return Some(path.to_string());
        }
    }
    if let Some(idx) = stripped.find("://") {
        let after = &stripped[idx + 3..];
        // host/org/repo
        if let Some((_, path)) = after.split_once('/') {
            return Some(path.to_string());
        }
    }
    Some(stripped.to_string())
}

/// Load global + project MEMORY.md indexes and render the L2 catalog section.
pub async fn load_memory_catalog(
    agent_dir: &Path,
    cwd: &Path,
    opts: &MemoryLoadOptions,
) -> Option<MemoryCatalog> {
    if !opts.enabled {
        return None;
    }
    // Async entry stays for callers; disk I/O is small and sync-safe.
    load_memory_catalog_sync(agent_dir, cwd, opts)
}

/// Sync load for harness / tools (no runtime required).
pub fn load_memory_catalog_sync(
    agent_dir: &Path,
    cwd: &Path,
    opts: &MemoryLoadOptions,
) -> Option<MemoryCatalog> {
    if !opts.enabled {
        return None;
    }

    let global_dir = memory_root(agent_dir).join("_global");
    let project_dir = project_memory_dir(agent_dir, cwd);
    let project_slug = project_slug(cwd);

    let mut entries = Vec::new();
    entries.extend(load_index_file_sync(
        &global_dir.join("MEMORY.md"),
        "global",
    ));
    entries.extend(load_index_file_sync(
        &project_dir.join("MEMORY.md"),
        "project",
    ));

    // Project entries with the same id override global (keep project last, then dedup by id).
    let mut by_id: std::collections::BTreeMap<String, MemoryIndexEntry> =
        std::collections::BTreeMap::new();
    for e in entries {
        by_id.insert(e.id.clone(), e);
    }
    let mut entries: Vec<_> = by_id.into_values().collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));

    let max = opts.index_max_lines.max(1);
    let truncated = entries.len() > max;
    if truncated {
        entries.truncate(max);
    }

    // Even with zero entries, inject a short L0 discipline + paths so the model
    // knows where to write (progressive disclosure map).
    let prompt_section = render_catalog(&entries, &global_dir, &project_dir, truncated, max);

    Some(MemoryCatalog {
        global_dir,
        project_dir,
        project_slug,
        entries,
        prompt_section,
    })
}

fn load_index_file_sync(path: &Path, default_scope: &str) -> Vec<MemoryIndexEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_memory_index(&text, default_scope, path.parent())
}

/// L4 session archive directory (`memory/sessions/`).
pub fn sessions_memory_dir(agent_dir: &Path) -> PathBuf {
    memory_root(agent_dir).join("sessions")
}

/// Search L2 indexes (+ optional L4 session file names) by free-text query (M5).
///
/// Returns ranking by simple score (id hit > tags > description). Does **not**
/// return full bodies — use `read` on `body_path` / location.
pub fn search_memory_index(
    agent_dir: &Path,
    cwd: &Path,
    query: &str,
    max_results: usize,
) -> Vec<MemorySearchHit> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let terms: Vec<&str> = q.split_whitespace().filter(|t| !t.is_empty()).collect();
    if terms.is_empty() {
        return Vec::new();
    }

    let opts = MemoryLoadOptions {
        enabled: true,
        index_max_lines: 10_000,
        ..Default::default()
    };
    let mut hits: Vec<MemorySearchHit> = Vec::new();

    if let Some(cat) = load_memory_catalog_sync(agent_dir, cwd, &opts) {
        for e in cat.entries {
            if let Some(score) = score_entry(&e, &terms) {
                hits.push(MemorySearchHit {
                    score,
                    entry: e,
                    source: MemorySearchSource::Index,
                });
            }
        }
    }

    // L4 session archive files (name + first non-empty line as description).
    let sess = sessions_memory_dir(agent_dir);
    if let Ok(rd) = std::fs::read_dir(&sess) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string();
            let file_text = std::fs::read_to_string(&path).unwrap_or_default();
            let desc = peek_session_desc_from(&file_text);
            let synthetic = MemoryIndexEntry {
                id: id.clone(),
                type_name: "reference".into(),
                scope: "session".into(),
                tags: "session,archive,l4".into(),
                description: desc,
                body_path: Some(path),
            };
            // Score id/tags/desc first; fall back to body text for L4 archives.
            let score =
                score_entry(&synthetic, &terms).or_else(|| score_text_blob(&file_text, &terms));
            if let Some(score) = score {
                hits.push(MemorySearchHit {
                    score,
                    entry: synthetic,
                    source: MemorySearchSource::SessionArchive,
                });
            }
        }
    }

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.entry.id.cmp(&b.entry.id))
    });
    let max = max_results.max(1);
    hits.truncate(max);
    hits
}

/// One search hit for `memory_search`.
#[derive(Debug, Clone)]
pub struct MemorySearchHit {
    pub score: u32,
    pub entry: MemoryIndexEntry,
    pub source: MemorySearchSource,
}

/// Match user query against tool intent memories (type=tool_intent / intent / tool).
///
/// Searches both global and project MEMORY.md indexes. Returns top-scoring rules
/// along with their actionable body excerpts so the runtime can inject a JIT
/// `<system-reminder>` before LLM execution.
pub fn match_tool_intent_rules(
    agent_dir: &Path,
    cwd: &Path,
    query: &str,
    max_results: usize,
) -> Vec<ToolIntentHit> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let q_lower = q.to_ascii_lowercase();

    let opts = MemoryLoadOptions {
        enabled: true,
        index_max_lines: 10_000,
        ..Default::default()
    };

    let Some(cat) = load_memory_catalog_sync(agent_dir, cwd, &opts) else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for e in cat.entries {
        let is_intent = e.type_name.eq_ignore_ascii_case("tool_intent")
            || e.type_name.eq_ignore_ascii_case("intent")
            || e.type_name.eq_ignore_ascii_case("tool")
            || e.tags.split(',').any(|t| {
                let t = t.trim();
                t.eq_ignore_ascii_case("tool_intent")
                    || t.eq_ignore_ascii_case("intent")
                    || t.eq_ignore_ascii_case("mcp")
                    || t.eq_ignore_ascii_case("tool")
            });

        if !is_intent {
            continue;
        }

        let body = e
            .body_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        let metadata = body
            .as_deref()
            .map(parse_tool_intent_metadata)
            .unwrap_or_default();

        let mut score = 0u32;
        let mut evidence = Vec::new();
        let id_lower = e.id.to_ascii_lowercase();
        let tags_lower = e.tags.to_ascii_lowercase();
        let desc_lower = e.description.to_ascii_lowercase();

        // Explicit negative triggers veto a rule. This is intentionally checked
        // before ordinary scoring so a broad rule cannot override user context.
        if metadata
            .negative_triggers
            .iter()
            .any(|trigger| contains_phrase(&q_lower, trigger))
        {
            continue;
        }

        // Explicit triggers are stronger than inferred tag/description matches.
        for trigger in &metadata.triggers {
            if contains_phrase(&q_lower, trigger) {
                score += 30;
                evidence.push(format!("trigger:{trigger}"));
            }
        }

        for tag in tags_lower
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            if tag.len() >= 3 && contains_phrase(&q_lower, tag) {
                score += 15;
                evidence.push(format!("tag:{tag}"));
            }
        }

        for part in id_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|p| p.len() >= 3)
        {
            if contains_phrase(&q_lower, part) {
                score += 10;
                evidence.push(format!("id:{part}"));
            }
        }

        for word in desc_lower
            .split(|c: char| c.is_whitespace() || ",，;；:：|/\\()（）[]【】".contains(c))
            .filter(|w| w.len() >= 3)
        {
            if contains_phrase(&q_lower, word) {
                score += 8;
                evidence.push(format!("description:{word}"));
            }
        }

        // Keep legacy id/tag/description matching behavior intact. Explicit
        // triggers add a stronger signal, while negative triggers can veto a
        // broad legacy rule before scoring.
        let base_score = score;
        if base_score < 8 {
            continue;
        }

        let score = base_score.saturating_add(metadata.priority.max(0) as u32);
        let confidence = ((score as f32) / 70.0).clamp(0.0, 1.0);
        let body_excerpt = body
            .as_deref()
            .map(|content| {
                let trimmed = strip_memory_frontmatter(content).trim();
                if trimmed.chars().count() <= 800 {
                    trimmed.to_string()
                } else {
                    let mut excerpt: String = trimmed.chars().take(800).collect();
                    if let Some(last_nl) = excerpt.rfind('\n') {
                        excerpt.truncate(last_nl);
                    }
                    excerpt.push_str("...");
                    excerpt
                }
            })
            .filter(|text| !text.is_empty());

        hits.push(ToolIntentHit {
            score,
            confidence,
            evidence,
            entry: e,
            body_excerpt,
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| a.entry.id.cmp(&b.entry.id))
    });
    hits.truncate(max_results.max(1));
    hits
}

fn contains_phrase(query: &str, phrase: &str) -> bool {
    let phrase = phrase.trim().to_ascii_lowercase();
    !phrase.is_empty() && query.contains(&phrase)
}

/// Parse the small, intentionally lenient subset of YAML used by memory bodies.
/// Supports scalar values and inline lists such as `triggers: [foo, bar]`.
fn parse_tool_intent_metadata(content: &str) -> ToolIntentMetadata {
    let trimmed = content.trim_start();
    let Some(frontmatter) = trimmed.strip_prefix("---") else {
        return ToolIntentMetadata::default();
    };
    let Some(end) = frontmatter.find("\n---") else {
        return ToolIntentMetadata::default();
    };

    let mut metadata = ToolIntentMetadata::default();
    for line in frontmatter[..end].lines() {
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = raw.trim();
        match key {
            "triggers" => metadata.triggers = parse_metadata_list(value),
            "negative_triggers" | "negativeTriggers" => {
                metadata.negative_triggers = parse_metadata_list(value)
            }
            "priority" => metadata.priority = value.parse::<i32>().unwrap_or(0).clamp(-100, 100),
            _ => {}
        }
    }
    metadata
}

fn parse_metadata_list(value: &str) -> Vec<String> {
    let value = value.trim().trim_start_matches('[').trim_end_matches(']');
    value
        .split(',')
        .map(|item| item.trim().trim_matches(['"', '\'']))
        .filter(|item| !item.is_empty())
        .map(|item| item.replace("\\\"", "\"").replace("\\\\", "\\"))
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySearchSource {
    Index,
    SessionArchive,
}

impl MemorySearchSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::SessionArchive => "session",
        }
    }
}

fn score_entry(e: &MemoryIndexEntry, terms: &[&str]) -> Option<u32> {
    let id = e.id.to_ascii_lowercase();
    let ty = e.type_name.to_ascii_lowercase();
    let tags = e.tags.to_ascii_lowercase();
    let desc = e.description.to_ascii_lowercase();
    let mut score = 0u32;
    let mut any = false;
    for t in terms {
        let mut term_hit = false;
        if id.contains(t) {
            score += 8;
            term_hit = true;
        }
        if tags.contains(t) {
            score += 5;
            term_hit = true;
        }
        if ty.contains(t) {
            score += 3;
            term_hit = true;
        }
        if desc.contains(t) {
            score += 2;
            term_hit = true;
        }
        if term_hit {
            any = true;
        }
    }
    if any {
        Some(score)
    } else {
        None
    }
}

fn peek_session_desc_from(text: &str) -> String {
    let mut in_fm = false;
    let mut saw_fm = false;
    for line in text.lines() {
        let t = line.trim();
        if !saw_fm && t == "---" {
            in_fm = true;
            saw_fm = true;
            continue;
        }
        if in_fm {
            if t == "---" {
                in_fm = false;
            }
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let head: String = t.chars().take(120).collect();
        return head;
    }
    "(session archive)".into()
}

fn score_text_blob(text: &str, terms: &[&str]) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    // Cap scan cost for huge archives.
    let slice = if lower.len() > 16_000 {
        let mut end = 16_000;
        while !lower.is_char_boundary(end) {
            end -= 1;
        }
        &lower[..end]
    } else {
        &lower
    };
    let mut score = 0u32;
    let mut any = false;
    for t in terms {
        if slice.contains(t) {
            score += 1;
            any = true;
        }
    }
    if any {
        Some(score)
    } else {
        None
    }
}

/// Archive a compaction / session summary into L4 (`memory/sessions/…`) (M5).
///
/// Does **not** update L2 MEMORY.md (never auto-inject into system).
pub fn archive_session_summary(
    agent_dir: &Path,
    cwd: &Path,
    session_id: &str,
    summary: &str,
) -> std::io::Result<PathBuf> {
    let dir = sessions_memory_dir(agent_dir);
    std::fs::create_dir_all(&dir)?;
    let date = chrono_ymd_today();
    let short: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    let short = if short.is_empty() {
        "session".into()
    } else {
        short
    };
    let slug = project_slug(cwd);
    let name = format!("{date}-{short}");
    let path = dir.join(format!("{name}.md"));
    let body = format!(
        "---\n\
         name: Session archive {short}\n\
         type: reference\n\
         scope: session\n\
         tags: [session, compaction, archive]\n\
         updated: {date}\n\
         project: {slug}\n\
         session_id: {session_id}\n\
         ---\n\n\
         # Session summary (L4 archive)\n\n\
         Point-in-time compaction/session notes. **Verify** before treating as fact.\n\n\
         {summary}\n"
    );
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Parse MEMORY.md list entries.
///
/// Accepted forms:
/// ```markdown
/// - [id] type=feedback scope=global tags=a,b
///   Description on next indented line
/// - [id] type=project scope=project tags=x — one-line description
/// ```
pub fn parse_memory_index(
    text: &str,
    default_scope: &str,
    parent_dir: Option<&Path>,
) -> Vec<MemoryIndexEntry> {
    let mut entries = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Bullet list item
        let item = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .unwrap_or("");
        if item.is_empty() {
            continue;
        }

        // [id] rest
        let (id, rest) = if let Some(rest) = item.strip_prefix('[') {
            if let Some((id, after)) = rest.split_once(']') {
                (id.trim().to_string(), after.trim())
            } else {
                continue;
            }
        } else {
            // Fallback: first token as id
            let mut parts = item.splitn(2, char::is_whitespace);
            let id = parts.next().unwrap_or("").trim().to_string();
            if id.is_empty() {
                continue;
            }
            (id, parts.next().unwrap_or("").trim())
        };
        if id.is_empty() {
            continue;
        }

        let mut type_name = "project".to_string();
        let mut scope = default_scope.to_string();
        let mut tags = String::new();
        let mut description = String::new();

        // Split "meta — description" or "meta - description"
        let (meta, inline_desc) = if let Some((m, d)) = rest.split_once(" — ") {
            (m.trim(), d.trim())
        } else if let Some((m, d)) = rest.split_once(" - ") {
            // only if left side looks like key=value
            if m.contains('=') {
                (m.trim(), d.trim())
            } else {
                (rest, "")
            }
        } else {
            (rest, "")
        };

        for tok in meta.split_whitespace() {
            if let Some((k, v)) = tok.split_once('=') {
                match k {
                    "type" => type_name = v.to_string(),
                    "scope" => scope = v.to_string(),
                    "tags" => tags = v.to_string(),
                    _ => {}
                }
            }
        }
        if !inline_desc.is_empty() {
            description = inline_desc.to_string();
        }

        // Continuation lines (indented description)
        while let Some(next) = lines.peek() {
            let n = *next;
            if n.starts_with(' ') || n.starts_with('\t') {
                let cont = n.trim();
                lines.next();
                if cont.is_empty() {
                    continue;
                }
                if description.is_empty() {
                    description = cont.to_string();
                } else {
                    description.push(' ');
                    description.push_str(cont);
                }
            } else {
                break;
            }
        }

        if description.is_empty() {
            description = format!("(see body for `{id}`)");
        }

        let body_path = parent_dir.map(|p| {
            // Prefer id.md next to MEMORY.md
            let candidate = p.join(format!("{id}.md"));
            if candidate.exists() {
                candidate
            } else {
                // Also allow type_id.md patterns already named
                candidate
            }
        });

        entries.push(MemoryIndexEntry {
            id,
            type_name,
            scope,
            tags,
            description,
            body_path,
        });
    }

    entries
}

fn render_catalog(
    entries: &[MemoryIndexEntry],
    global_dir: &Path,
    project_dir: &Path,
    truncated: bool,
    max_lines: usize,
) -> String {
    let mut out = String::new();
    out.push_str("## Memory (L2 index — progressive disclosure)\n\n");
    out.push_str(
        "Cross-session notes. This is a **map only** — not full instructions.\n\
         - To use an entry: `read` its body path (prefer under the dirs below).\n\
         - Memory is point-in-time: **verify against current code** before asserting as fact.\n\
         - Prefer `memory_search` for index lookup; then `read` 0–few bodies.\n\
         - Limit memory `read`/`grep` this turn (budget); do not scan the whole library.\n\
         - Do **not** dump whole memory files into replies.\n\
         - Session compaction archives (if enabled) live under `memory/sessions/` (L4 only).\n\n",
    );
    out.push_str("### Write discipline (M2/M6)\n");
    out.push_str(
        "- Default **NO-OP**: only write when a future agent would clearly benefit.\n\
         - Skip trivial / one-off corrections, facts already in AGENTS.md, and live metrics.\n\
         - Prefer **updating** an existing id (`memory_search` first) over duplicate entries.\n\
         - Prefer `memory_write` (when available) — atomic body + MEMORY.md index in one call.\n\
         - Fallback: write body `{id}.md` **and** a matching `MEMORY.md` line in the same dir.\n\
         - Body frontmatter: `name`, `type`, `scope`, `tags`, `updated: YYYY-MM-DD`.\n\
         - Index line: `- [id] type=… scope=… tags=… — one-line description`.\n\
         - L2 catalog is **session-frozen**; new index lines appear after `/reload` or a new session.\n\n",
    );
    out.push_str(&format!(
        "Dirs (read+write when memory write is enabled):\n- global: `{}`\n- project: `{}`\n\n",
        global_dir.display(),
        project_dir.display()
    ));

    if entries.is_empty() {
        out.push_str(
            "Index is empty. Optional write via `memory_write`:\n\
             id=`tip` scope=project tags=build description=`…` body=`…`\n\
             (Or manually write MEMORY.md + `{id}.md` under the dirs above.)\n",
        );
        return out;
    }

    out.push_str("<memory-catalog>\n");
    for e in entries {
        let loc = e
            .body_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("(body: {id}.md next to MEMORY.md)", id = e.id));
        out.push_str(&format!(
            "<memory id=\"{}\" type=\"{}\" scope=\"{}\" tags=\"{}\" location=\"{}\">{}</memory>\n",
            xml_escape(&e.id),
            xml_escape(&e.type_name),
            xml_escape(&e.scope),
            xml_escape(&e.tags),
            xml_escape(&loc),
            xml_escape(&e.description),
        ));
    }
    out.push_str("</memory-catalog>\n");
    if truncated {
        out.push_str(&format!(
            "\n(Index truncated to {max_lines} entries; use `ls`/`grep` under memory dirs for more.)\n"
        ));
    }
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_index_lines() {
        let text = r#"
# Memory index

- [feedback_no_hyphens] type=feedback scope=global tags=writing,style
  Never use hyphens in written content
- [project_oauth] type=project scope=project tags=auth,oauth — Staging uses device code
"#;
        let entries = parse_memory_index(text, "global", Some(Path::new("/tmp/mem")));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "feedback_no_hyphens");
        assert!(entries[0].description.contains("hyphens"));
        assert_eq!(entries[1].id, "project_oauth");
        assert!(entries[1].description.contains("device code"));
    }

    #[test]
    fn project_slug_is_stable() {
        let a = project_slug(Path::new("/tmp/foo-bar"));
        let b = project_slug(Path::new("/tmp/foo-bar"));
        assert_eq!(a, b);
        assert!(a.contains("foo-bar") || a.len() > 8);
    }

    #[tokio::test]
    async fn load_empty_still_renders_map() {
        let tmp = std::env::temp_dir().join(format!("one-mem-empty-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(&tmp).await.unwrap();
        let cat = load_memory_catalog(&tmp, Path::new("/tmp/proj"), &MemoryLoadOptions::default())
            .await
            .unwrap();
        assert!(cat.prompt_section.contains("Memory (L2 index"));
        assert!(cat.entries.is_empty());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn load_disabled_is_none() {
        let tmp = std::env::temp_dir().join(format!("one-mem-off-{}", std::process::id()));
        let opts = MemoryLoadOptions {
            enabled: false,
            ..Default::default()
        };
        assert!(load_memory_catalog(&tmp, Path::new("/tmp/p"), &opts)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn load_parses_project_index() {
        let tmp = std::env::temp_dir().join(format!("one-mem-idx-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let cwd = Path::new("/tmp/one-mem-proj-cwd");
        let proj = project_memory_dir(&tmp, cwd);
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::write(
            proj.join("MEMORY.md"),
            "- [tip] type=project scope=project tags=build — Use cargo test\n",
        )
        .await
        .unwrap();
        let cat = load_memory_catalog(&tmp, cwd, &MemoryLoadOptions::default())
            .await
            .unwrap();
        assert_eq!(cat.entries.len(), 1);
        assert_eq!(cat.entries[0].id, "tip");
        assert!(cat.prompt_section.contains("tip"));
        assert!(cat.prompt_section.contains("<memory-catalog>"));
        assert!(cat.prompt_section.contains("Write discipline"));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn scaffold_has_frontmatter() {
        let s = scaffold_memory_body("Tip", "project", "project", "build", "Use cargo test");
        assert!(s.starts_with("---"));
        assert!(s.contains("updated:"));
        assert!(s.contains("Use cargo test"));
    }

    #[test]
    fn format_index_line() {
        let line = format_index_entry_line("tip", "project", "project", "build", "Use cargo test");
        assert!(line.starts_with("- [tip]"));
        let entries = parse_memory_index(&format!("{line}\n"), "project", None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "tip");
    }

    #[test]
    fn search_finds_tags() {
        let tmp = std::env::temp_dir().join(format!("one-mem-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cwd = tmp.join("c");
        std::fs::create_dir_all(&cwd).unwrap();
        let proj = project_memory_dir(&tmp, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        let line = format_index_entry_line(
            "oauth",
            "project",
            "project",
            "auth,oauth",
            "device code flow",
        );
        std::fs::write(proj.join("MEMORY.md"), format!("{line}\n")).unwrap();
        let hits = search_memory_index(&tmp, &cwd, "oauth", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.id, "oauth");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn archive_session_writes_l4() {
        let tmp = std::env::temp_dir().join(format!("one-mem-arch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cwd = tmp.join("p");
        std::fs::create_dir_all(&cwd).unwrap();
        let path =
            archive_session_summary(&tmp, &cwd, "sess-abc", "We fixed the flaky test.").unwrap();
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("flaky test"));
        assert!(text.contains("type: reference"));
        let hits = search_memory_index(&tmp, &cwd, "flaky", 5);
        assert!(!hits.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn upsert_writes_body_and_index() {
        let tmp = std::env::temp_dir().join(format!("one-mem-upsert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cwd = tmp.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let r = upsert_memory_entry(
            &tmp,
            &cwd,
            &MemoryUpsertInput {
                id: "no_hyphens".into(),
                scope: "global".into(),
                type_name: "feedback".into(),
                tags: "writing,style".into(),
                description: "Never use hyphens in drafts".into(),
                body: "Prefer alternative phrasing.".into(),
                name: Some("No hyphens".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r.created);
        assert!(!r.index_updated);
        assert!(r.body_path.exists());
        assert!(r.index_path.exists());
        let body = std::fs::read_to_string(&r.body_path).unwrap();
        assert!(body.contains("updated:"));
        assert!(body.contains("Prefer alternative"));
        let idx = std::fs::read_to_string(&r.index_path).unwrap();
        assert!(idx.contains("[no_hyphens]"));
        assert!(idx.contains("Never use hyphens"));

        // Update same id
        let r2 = upsert_memory_entry(
            &tmp,
            &cwd,
            &MemoryUpsertInput {
                id: "no_hyphens".into(),
                scope: "global".into(),
                type_name: "feedback".into(),
                tags: "writing".into(),
                description: "Avoid hyphens and em dashes".into(),
                body: "Updated body.".into(),
                name: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!r2.created);
        assert!(r2.index_updated);
        let idx2 = std::fs::read_to_string(&r2.index_path).unwrap();
        assert!(idx2.contains("Avoid hyphens"));
        assert!(!idx2.contains("Never use hyphens"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_memory_id_rejects_bad() {
        assert!(validate_memory_id("ok_id-1").is_ok());
        assert!(validate_memory_id("MEMORY").is_err());
        assert!(validate_memory_id("../x").is_err());
        assert!(validate_memory_id("").is_err());
    }

    #[test]
    fn strip_frontmatter_keeps_body() {
        let raw = "---\nname: x\n---\n\nHello world\n";
        assert_eq!(strip_memory_frontmatter(raw), "Hello world");
    }

    #[test]
    fn match_tool_intent_rules_finds_mcp_intents() {
        let tmp = std::env::temp_dir().join(format!("one-mem-intent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cwd = tmp.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        // Write a tool_intent memory entry
        upsert_memory_entry(
            &tmp,
            &cwd,
            &MemoryUpsertInput {
                id: "tool-intent-deepwiki".into(),
                scope: "global".into(),
                type_name: "tool_intent".into(),
                tags: "docs,library,opensource,第三方库,开源库,文档".into(),
                description: "询问开源库/第三方库文档与源码时使用 deepwiki 工具".into(),
                body: "调用 search_tool(query=\"deepwiki\") 获取工具并查询文档".into(),
                name: Some("DeepWiki Tool Intent".into()),
                triggers: vec![
                    "开源库".into(),
                    "第三方库".into(),
                    "opensource library".into(),
                ],
                negative_triggers: vec!["当前项目源码".into()],
                priority: 5,
            },
        )
        .unwrap();

        // 1. Chinese query matching
        let hits = match_tool_intent_rules(&tmp, &cwd, "这个第三方库怎么使用？", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.id, "tool-intent-deepwiki");
        assert!(hits[0].confidence > 0.0);
        assert!(!hits[0].evidence.is_empty());
        assert!(hits[0].body_excerpt.as_ref().unwrap().contains("deepwiki"));

        // 2. English query matching
        let hits_en = match_tool_intent_rules(&tmp, &cwd, "show me the opensource library docs", 5);
        assert_eq!(hits_en.len(), 1);
        assert_eq!(hits_en[0].entry.id, "tool-intent-deepwiki");

        // 3. Unrelated query
        let hits_none = match_tool_intent_rules(&tmp, &cwd, "今天天气怎么样", 5);
        assert!(hits_none.is_empty());

        let vetoed = match_tool_intent_rules(&tmp, &cwd, "查看当前项目源码，不要查开源库文档", 5);
        assert!(vetoed.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
