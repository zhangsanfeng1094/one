//! Transactional writer for config documents.
//!
//! The save sequence is fixed and non-negotiable (see `docs/web-config.md` §4):
//!
//! 1. validate the draft and produce a masked diff,
//! 2. re-read the file and refuse to continue if it changed underneath us,
//! 3. back up the current content, write a sibling temp file, `fsync`, then
//!    atomically rename,
//! 4. report the new version, the backup record, and when the change applies.
//!
//! Writes are serialized process-wide so two browser tabs cannot interleave
//! read-modify-write cycles. This is *not* a lock other editors respect, so the
//! content-version check — not the lock — is what actually prevents clobbering.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version string used when the document does not exist on disk.
pub const VERSION_ABSENT: &str = "absent";

/// Number of backups retained per document.
pub const MAX_BACKUPS: usize = 10;

/// Serializes every studio write in this process.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Why a backup was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupReason {
    /// The previous content was displaced by a save.
    Save,
    /// The content was displaced by a restore.
    Restore,
}

impl BackupReason {
    /// Stable string form.
    pub fn as_str(self) -> &'static str {
        match self {
            BackupReason::Save => "save",
            BackupReason::Restore => "restore",
        }
    }
}

/// A stored snapshot of a document's previous content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    /// Opaque id used to restore.
    pub id: String,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// Size of the backed-up content in bytes.
    pub size: u64,
    /// Content version of the backed-up bytes.
    pub version: String,
    /// Why the backup was taken.
    pub reason: String,
}

/// Result of a successful commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveOutcome {
    /// Version of the newly written content.
    pub version: String,
    /// Bytes written.
    pub bytes: usize,
    /// Whether the file was newly created.
    pub created: bool,
    /// Backup of the displaced content, when there was any.
    pub backup: Option<BackupRecord>,
}

/// Failure modes of a commit, each mapping to a distinct HTTP status.
#[derive(Debug, Clone)]
pub enum SaveError {
    /// The file changed on disk since the draft was produced.
    Conflict {
        /// Version the client based its edit on.
        expected: String,
        /// Version currently on disk.
        current: String,
    },
    /// The resolved path escaped its allowed root (path traversal or symlink).
    OutsideRoot {
        /// Rejected path.
        path: String,
        /// Root it had to stay inside.
        root: String,
    },
    /// The target is not a regular file (directory, socket, …).
    NotRegularFile(String),
    /// Any other I/O failure.
    Io(String),
}

impl SaveError {
    /// HTTP status code for this failure.
    pub fn status(&self) -> u16 {
        match self {
            SaveError::Conflict { .. } => 409,
            SaveError::OutsideRoot { .. } => 403,
            SaveError::NotRegularFile(_) => 400,
            SaveError::Io(_) => 500,
        }
    }

    /// Human-readable message safe to return to the browser.
    pub fn message(&self) -> String {
        match self {
            SaveError::Conflict { expected, current } => format!(
                "文件在编辑期间被外部修改（期望版本 {expected}，当前版本 {current}）。\
                 已停止写入以免覆盖外部改动，请重新载入后再保存。"
            ),
            SaveError::OutsideRoot { path, root } => {
                format!("拒绝写入 {path}：超出允许的配置根目录 {root}")
            }
            SaveError::NotRegularFile(p) => format!("{p} 不是普通文件，拒绝写入"),
            SaveError::Io(msg) => msg.clone(),
        }
    }
}

/// Content version = SHA-256 of the raw bytes, truncated for display.
pub fn content_version(content: Option<&str>) -> String {
    match content {
        None => VERSION_ABSENT.to_string(),
        Some(text) => hash_bytes(text.as_bytes()),
    }
}

/// Full SHA-256 hex digest of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Short form of a version for display.
pub fn short_version(version: &str) -> String {
    version.chars().take(8).collect()
}

/// Milliseconds since the Unix epoch.
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Sanitize a document id into a single safe directory name.
fn safe_dir_name(doc_id: &str) -> String {
    let mut out = String::with_capacity(doc_id.len());
    for c in doc_id.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("doc");
    }
    out
}

/// Decode a JSON string literal escape-free: reject anything that could be a
/// path separator so backup ids cannot traverse directories.
fn is_safe_backup_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !id.contains("..")
}

/// On-disk backup envelope.
#[derive(Debug, Serialize, Deserialize)]
struct BackupEnvelope {
    doc_id: String,
    created_at: String,
    reason: String,
    version: String,
    content: String,
}

/// Verify that `path` stays inside `root`, resolving symlinks on both sides.
///
/// For an existing target the canonical file path must be under the canonical
/// root, which is what catches a symlinked file pointing outside.
///
/// For a target that does not exist yet — including one whose *directory* does
/// not exist, as when creating a fresh `.one/mcp.json` — the nearest existing
/// ancestor is canonicalized and the remaining components are re-attached before
/// the prefix check. This still catches a symlinked directory that escapes the
/// root, without refusing legitimate creation.
pub fn ensure_contained(path: &Path, root: &Path) -> Result<(), SaveError> {
    let outside = || SaveError::OutsideRoot {
        path: path.display().to_string(),
        root: root.display().to_string(),
    };

    // A first save may create the configured root itself (fresh agent home or
    // project .one). Resolve its existing ancestors exactly as for the target.
    let canonical_root = resolve_missing_path(root).ok_or_else(outside)?;

    let resolved = if path.exists() {
        if !path.is_file() {
            return Err(SaveError::NotRegularFile(path.display().to_string()));
        }
        fs::canonicalize(path).map_err(|e| SaveError::Io(format!("canonicalize failed: {e}")))?
    } else {
        resolve_missing_path(path).ok_or_else(outside)?
    };

    if resolved.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(outside())
    }
}

fn resolve_missing_path(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    while !cursor.exists() {
        // Broken symlinks must not be treated as missing directories.
        if fs::symlink_metadata(&cursor).is_ok() {
            return None;
        }
        missing.push(cursor.file_name()?.to_os_string());
        if !cursor.pop() {
            return None;
        }
    }
    let mut resolved = fs::canonicalize(cursor).ok()?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    Some(resolved)
}

/// Read a document's content, treating a missing file as `None`.
pub fn read_document(path: &Path) -> Result<Option<String>, SaveError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(SaveError::Io(format!(
            "读取 {} 失败：{err}",
            path.display()
        ))),
    }
}

/// Backup store rooted at a directory no loader scans.
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    /// Create a store under `<agent_dir>/config-backups`.
    ///
    /// Keeping backups beside `settings.json` etc. would make them visible to
    /// the runtime loaders; `config-backups` is not a discovery root for any of
    /// them (settings, mcp, models, agents, prompts, skills, hooks).
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            root: agent_dir.join("config-backups"),
        }
    }

    /// Directory holding backups for one document.
    pub fn doc_dir(&self, doc_id: &str) -> PathBuf {
        self.root.join(safe_dir_name(doc_id))
    }

    /// Root of the whole backup store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write a backup of `content` and prune to [`MAX_BACKUPS`].
    pub fn store(
        &self,
        doc_id: &str,
        content: &str,
        reason: BackupReason,
    ) -> Result<BackupRecord, SaveError> {
        let dir = self.doc_dir(doc_id);
        fs::create_dir_all(&dir).map_err(|e| SaveError::Io(format!("创建备份目录失败：{e}")))?;
        restrict_dir_mode(&self.root, BACKUP_DIR_MODE)
            .map_err(|e| SaveError::Io(format!("限制备份根目录权限失败：{e}")))?;
        restrict_dir_mode(&dir, BACKUP_DIR_MODE)
            .map_err(|e| SaveError::Io(format!("限制备份目录权限失败：{e}")))?;

        let version = hash_bytes(content.as_bytes());
        let millis = now_millis();
        let id = format!("{millis}-{}", short_version(&version));
        let envelope = BackupEnvelope {
            doc_id: doc_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: reason.as_str().to_string(),
            version: version.clone(),
            content: content.to_string(),
        };
        let body = serde_json::to_string(&envelope)
            .map_err(|e| SaveError::Io(format!("序列化备份失败：{e}")))?;

        let target = dir.join(format!("{id}.json"));
        write_atomic(&target, body.as_bytes())
            .map_err(|e| SaveError::Io(format!("写入备份失败：{e}")))?;

        self.prune(doc_id)?;

        Ok(BackupRecord {
            id,
            created_at: envelope.created_at,
            size: content.len() as u64,
            version,
            reason: reason.as_str().to_string(),
        })
    }

    /// List backups newest first.
    pub fn list(&self, doc_id: &str) -> Vec<BackupRecord> {
        let dir = self.doc_dir(doc_id);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_safe_backup_id(id) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(env) = serde_json::from_str::<BackupEnvelope>(&text) else {
                continue;
            };
            out.push(BackupRecord {
                id: id.to_string(),
                created_at: env.created_at,
                size: env.content.len() as u64,
                version: env.version,
                reason: env.reason,
            });
        }
        out.sort_by(|a, b| b.id.cmp(&a.id));
        out
    }

    /// Read the content of one backup.
    pub fn load(&self, doc_id: &str, backup_id: &str) -> Result<String, SaveError> {
        if !is_safe_backup_id(backup_id) {
            return Err(SaveError::Io("非法的备份 id".to_string()));
        }
        let path = self.doc_dir(doc_id).join(format!("{backup_id}.json"));
        let text = fs::read_to_string(&path)
            .map_err(|e| SaveError::Io(format!("读取备份 {backup_id} 失败：{e}")))?;
        let env: BackupEnvelope = serde_json::from_str(&text)
            .map_err(|e| SaveError::Io(format!("备份 {backup_id} 已损坏：{e}")))?;
        Ok(env.content)
    }

    /// Keep only the newest [`MAX_BACKUPS`] entries for a document.
    fn prune(&self, doc_id: &str) -> Result<(), SaveError> {
        let dir = self.doc_dir(doc_id);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut paths: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    return None;
                }
                let id = path.file_stem()?.to_str()?.to_string();
                Some((id, path))
            })
            .collect();
        if paths.len() <= MAX_BACKUPS {
            return Ok(());
        }
        paths.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, path) in paths.into_iter().skip(MAX_BACKUPS) {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }
}

/// Default mode for newly created studio files (configs and backups).
const NEW_FILE_MODE: u32 = 0o600;

/// Default mode for backup directories.
const BACKUP_DIR_MODE: u32 = 0o700;

/// Restrict a directory to `mode` on Unix. No-op elsewhere.
fn restrict_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Permission bits of an existing destination, if it is present.
fn destination_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Create `path` exclusively and apply `mode` before any payload is written.
fn create_exclusive_temp(path: &Path, mode: u32) -> io::Result<fs::File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `OpenOptions::mode` is still filtered by umask; force the bits before
        // writing secrets so a 0664 window never exists.
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    Ok(file)
}

/// Write bytes to `path` atomically: sibling temp file → `fsync` → rename.
///
/// The rename is the only observable transition, so a reader either sees the
/// old file or the new one, never a partial write. The parent directory is
/// synced afterwards so the rename itself is durable.
///
/// New files are created `0600`. Replacing an existing file copies its mode so
/// the write cannot widen access (a previous `0600` stays `0600`).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let mode = destination_mode(path).unwrap_or(NEW_FILE_MODE);

    for attempt in 0..16 {
        let tmp = parent.join(format!(
            ".{file_name}.one-studio-{}-{}-{attempt}.tmp",
            std::process::id(),
            now_millis()
        ));
        let mut file = match create_exclusive_temp(&tmp, mode) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        if let Err(err) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        drop(file);
        if let Err(err) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("无法创建独占临时文件：{}", path.display()),
    ))
}

/// Perform the full commit sequence for one document.
///
/// `expected_version` must match the file's current version, otherwise the write
/// is refused with [`SaveError::Conflict`]. Pass [`VERSION_ABSENT`] when the
/// editor believed the file did not exist.
pub fn commit(
    backups: &BackupStore,
    doc_id: &str,
    path: &Path,
    root: &Path,
    draft: &str,
    expected_version: &str,
    reason: BackupReason,
) -> Result<SaveOutcome, SaveError> {
    // Process-wide serialization: the lock covers the read-check-write window so
    // two tabs cannot both pass the version check.
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| SaveError::Io("配置写入锁已损坏，请重启 one config".to_string()))?;

    ensure_contained(path, root)?;

    let current = read_document(path)?;
    let current_version = content_version(current.as_deref());
    if current_version != expected_version {
        return Err(SaveError::Conflict {
            expected: expected_version.to_string(),
            current: current_version,
        });
    }

    let existed = current.is_some();
    let backup = match &current {
        Some(old) if old != draft => Some(backups.store(doc_id, old, reason)?),
        _ => None,
    };

    write_atomic(path, draft.as_bytes())
        .map_err(|e| SaveError::Io(format!("写入 {} 失败：{e}", path.display())))?;

    Ok(SaveOutcome {
        version: hash_bytes(draft.as_bytes()),
        bytes: draft.len(),
        created: !existed,
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-config-save-{tag}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn version_tracks_content_and_absence() {
        assert_eq!(content_version(None), VERSION_ABSENT);
        let a = content_version(Some("{}"));
        let b = content_version(Some("{\"a\":1}"));
        assert_ne!(a, b);
        assert_eq!(a, content_version(Some("{}")));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn commit_writes_and_reports_versions() {
        let root = temp_dir("commit");
        let backups = BackupStore::new(&root);
        let path = root.join("settings.json");

        let outcome = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{\n  \"a\": 1\n}\n",
            VERSION_ABSENT,
            BackupReason::Save,
        )
        .unwrap();
        assert!(outcome.created);
        assert!(outcome.backup.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n  \"a\": 1\n}\n");

        // Second commit must carry the version returned by the first.
        let outcome2 = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{\n  \"a\": 2\n}\n",
            &outcome.version,
            BackupReason::Save,
        )
        .unwrap();
        assert!(!outcome2.created);
        let backup = outcome2.backup.expect("previous content backed up");
        assert_eq!(backup.version, outcome.version);
        assert_eq!(
            backups.load("settings.global", &backup.id).unwrap(),
            "{\n  \"a\": 1\n}\n"
        );
    }

    #[test]
    fn commit_refuses_when_file_changed_underneath() {
        let root = temp_dir("conflict");
        let backups = BackupStore::new(&root);
        let path = root.join("settings.json");

        let first = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{\"a\":1}\n",
            VERSION_ABSENT,
            BackupReason::Save,
        )
        .unwrap();

        // An external editor rewrites the file.
        fs::write(&path, "{\"a\":9}\n").unwrap();

        let err = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{\"a\":2}\n",
            &first.version,
            BackupReason::Save,
        )
        .unwrap_err();
        match err {
            SaveError::Conflict { expected, current } => {
                assert_eq!(expected, first.version);
                assert_ne!(current, first.version);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        // The external edit is preserved.
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":9}\n");
    }

    #[test]
    fn commit_rejects_paths_outside_root() {
        let root = temp_dir("root");
        let outside = temp_dir("outside");
        let backups = BackupStore::new(&root);
        let path = outside.join("settings.json");

        let err = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{}",
            VERSION_ABSENT,
            BackupReason::Save,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::OutsideRoot { .. }));
        assert_eq!(err.status(), 403);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn commit_rejects_symlinked_escape() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("symlink-root");
        let outside = temp_dir("symlink-outside");
        fs::write(outside.join("settings.json"), "outside\n").unwrap();
        symlink(outside.join("settings.json"), root.join("settings.json")).unwrap();

        let backups = BackupStore::new(&root);
        let err = commit(
            &backups,
            "settings.global",
            &root.join("settings.json"),
            &root,
            "{}",
            VERSION_ABSENT,
            BackupReason::Save,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::OutsideRoot { .. }));
        assert_eq!(
            fs::read_to_string(outside.join("settings.json")).unwrap(),
            "outside\n"
        );
    }

    #[test]
    fn backups_are_pruned_to_ten() {
        let root = temp_dir("prune");
        let backups = BackupStore::new(&root);
        for i in 0..15 {
            backups
                .store(
                    "models.global",
                    &format!("{{\"i\":{i}}}\n"),
                    BackupReason::Save,
                )
                .unwrap();
        }
        let list = backups.list("models.global");
        assert_eq!(list.len(), MAX_BACKUPS);
    }

    #[test]
    fn backup_ids_cannot_traverse_directories() {
        let root = temp_dir("traverse");
        let backups = BackupStore::new(&root);
        backups
            .store("settings.global", "{\"a\":1}\n", BackupReason::Save)
            .unwrap();
        assert!(backups.load("settings.global", "../../etc/passwd").is_err());
        assert!(backups.load("settings.global", "..").is_err());
    }

    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let root = temp_dir("atomic");
        let path = root.join("mcp.json");
        write_atomic(&path, b"{}\n").unwrap();
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".one-studio-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("mode-keep");
        let path = root.join("models.json");
        fs::write(&path, "{}\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).unwrap();

        write_atomic(&path, b"{\"a\":1}\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "replace must not widen 0600");
    }

    #[cfg(unix)]
    #[test]
    fn new_file_and_backup_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("mode-new");
        let backups = BackupStore::new(&root);
        let path = root.join("mcp.json");

        let first = commit(
            &backups,
            "mcp.user",
            &path,
            &root,
            "{\"mcpServers\":{}}\n",
            VERSION_ABSENT,
            BackupReason::Save,
        )
        .unwrap();
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);

        commit(
            &backups,
            "mcp.user",
            &path,
            &root,
            "{\"mcpServers\":{\"x\":{}}}\n",
            &first.version,
            BackupReason::Save,
        )
        .unwrap();
        let backup_root = backups.root();
        let backup_dir = backups.doc_dir("mcp.user");
        assert_eq!(
            fs::metadata(backup_root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let backup_file = fs::read_dir(&backup_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .expect("backup file");
        assert_eq!(
            fs::metadata(&backup_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn identical_draft_creates_no_backup() {
        let root = temp_dir("nochange");
        let backups = BackupStore::new(&root);
        let path = root.join("settings.json");
        let first = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{}\n",
            VERSION_ABSENT,
            BackupReason::Save,
        )
        .unwrap();
        let second = commit(
            &backups,
            "settings.global",
            &path,
            &root,
            "{}\n",
            &first.version,
            BackupReason::Save,
        )
        .unwrap();
        assert!(second.backup.is_none());
    }
}
