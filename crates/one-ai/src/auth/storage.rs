//! `~/.one/agent/auth.json` credential storage (Pi AuthStorage shape).
//!
//! Priority for [`AuthStorage::resolve_api_key`]:
//! 1. Runtime override (`--api-key`)
//! 2. Stored API key
//! 3. Stored OAuth (auto-refresh when expired)
//! 4. Environment variables

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::login_opencode;
use super::oauth_codex;
use super::oauth_xai;
use super::types::{
    AuthCredential, AuthStatus, ModelAuth, OAuthCredential, PROVIDER_OPENAI_CODEX,
    PROVIDER_OPENCODE, PROVIDER_OPENCODE_GO, PROVIDER_XAI,
};

const AUTH_FILE: &str = "auth.json";
const LOCK_STALE_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("auth storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("auth storage parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("auth storage: {0}")]
    Msg(String),
}

pub type AuthResult<T> = Result<T, AuthError>;

/// File-backed auth store.
pub struct AuthStorage {
    path: PathBuf,
    data: Mutex<BTreeMap<String, AuthCredential>>,
    runtime: Mutex<BTreeMap<String, String>>,
}

impl AuthStorage {
    pub fn default_path() -> PathBuf {
        agent_dir().join(AUTH_FILE)
    }

    pub fn create() -> AuthResult<Self> {
        Self::open(Self::default_path())
    }

    pub fn open(path: impl Into<PathBuf>) -> AuthResult<Self> {
        let path = path.into();
        let data = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            if raw.trim().is_empty() {
                BTreeMap::new()
            } else {
                serde_json::from_str(&raw)?
            }
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
            runtime: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_runtime_api_key(&self, provider: &str, key: impl Into<String>) {
        self.runtime
            .lock()
            .expect("auth runtime lock")
            .insert(provider.to_string(), key.into());
    }

    pub fn remove_runtime_api_key(&self, provider: &str) {
        self.runtime
            .lock()
            .expect("auth runtime lock")
            .remove(provider);
    }

    pub fn get(&self, provider: &str) -> Option<AuthCredential> {
        self.data
            .lock()
            .expect("auth data lock")
            .get(provider)
            .cloned()
    }

    pub fn list(&self) -> Vec<String> {
        self.data
            .lock()
            .expect("auth data lock")
            .keys()
            .cloned()
            .collect()
    }

    pub fn has(&self, provider: &str) -> bool {
        self.data
            .lock()
            .expect("auth data lock")
            .contains_key(provider)
    }

    pub fn has_auth(&self, provider: &str) -> bool {
        if self
            .runtime
            .lock()
            .expect("auth runtime lock")
            .contains_key(provider)
        {
            return true;
        }
        if self.has(provider) || self.get_shared_opencode(provider).is_some() {
            return true;
        }
        env_api_key(provider).is_some()
    }

    pub fn get_auth_status(&self, provider: &str) -> AuthStatus {
        if self.has(provider) {
            let kind = self
                .get(provider)
                .map(|c| if c.is_oauth() { "oauth" } else { "api_key" })
                .unwrap_or("stored");
            return AuthStatus {
                configured: true,
                source: Some("stored"),
                label: Some(kind.into()),
            };
        }
        if self
            .runtime
            .lock()
            .expect("auth runtime lock")
            .contains_key(provider)
        {
            return AuthStatus {
                configured: true,
                source: Some("runtime"),
                label: Some("--api-key".into()),
            };
        }
        if let Some(env) = env_api_key_name(provider) {
            if std::env::var_os(env).is_some() {
                return AuthStatus {
                    configured: true,
                    source: Some("environment"),
                    label: Some(env.into()),
                };
            }
        }
        AuthStatus {
            configured: false,
            source: None,
            label: None,
        }
    }

    pub fn set(&self, provider: &str, credential: AuthCredential) -> AuthResult<()> {
        self.with_file_lock(|data| {
            data.insert(provider.to_string(), credential);
            Ok(())
        })
    }

    pub fn remove(&self, provider: &str) -> AuthResult<bool> {
        self.with_file_lock(|data| Ok(data.remove(provider).is_some()))
    }

    pub fn logout(&self, provider: &str) -> AuthResult<bool> {
        self.remove(provider)
    }

    /// Blocking resolve — safe inside a multi-thread Tokio runtime via `block_in_place`.
    pub fn resolve_api_key_blocking(&self, provider: &str) -> AuthResult<Option<ModelAuth>> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(self.resolve_api_key(provider)))
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| AuthError::Msg(e.to_string()))?;
            rt.block_on(self.resolve_api_key(provider))
        }
    }

    /// Resolve an access token / API key for a provider, refreshing OAuth if needed.
    pub async fn resolve_api_key(&self, provider: &str) -> AuthResult<Option<ModelAuth>> {
        if let Some(key) = self
            .runtime
            .lock()
            .expect("auth runtime lock")
            .get(provider)
            .cloned()
        {
            return Ok(Some(opencode_model_auth(provider, key, "--api-key")));
        }

        let cred = self.get(provider).or_else(|| self.get_shared_opencode(provider));
        if let Some(AuthCredential::ApiKey(k)) = &cred {
            return Ok(Some(opencode_model_auth(
                provider,
                k.key.clone(),
                "auth.json",
            )));
        }

        if let Some(AuthCredential::OAuth(oauth)) = cred {
            let oauth = self.ensure_fresh_oauth(provider, oauth).await?;
            return Ok(Some(oauth_to_model_auth(provider, &oauth)));
        }

        if let Some(key) = env_api_key(provider) {
            let source = env_api_key_name(provider).unwrap_or("env");
            return Ok(Some(opencode_model_auth(provider, key, source)));
        }

        Ok(None)
    }

    /// OpenCode Zen/Go share one subscription key — fall back to the sibling id.
    fn get_shared_opencode(&self, provider: &str) -> Option<AuthCredential> {
        let sibling = match provider {
            PROVIDER_OPENCODE => PROVIDER_OPENCODE_GO,
            PROVIDER_OPENCODE_GO => PROVIDER_OPENCODE,
            _ => return None,
        };
        self.get(sibling)
    }

    async fn ensure_fresh_oauth(
        &self,
        provider: &str,
        mut oauth: OAuthCredential,
    ) -> AuthResult<OAuthCredential> {
        let now = now_ms();
        if now + 60_000 < oauth.expires {
            return Ok(oauth);
        }
        // Refresh under file lock so concurrent one processes don't double-refresh.
        let refreshed = refresh_oauth(provider, &oauth).await?;
        self.with_file_lock(|data| {
            // Another process may have refreshed already.
            if let Some(AuthCredential::OAuth(current)) = data.get(provider) {
                if now_ms() + 60_000 < current.expires && current.access != oauth.access {
                    oauth = current.clone();
                    return Ok(oauth);
                }
            }
            data.insert(
                provider.to_string(),
                AuthCredential::OAuth(refreshed.clone()),
            );
            Ok(refreshed)
        })
    }

    fn with_file_lock<T>(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, AuthCredential>) -> AuthResult<T>,
    ) -> AuthResult<T> {
        let lock_path = self.path.with_extension("json.lock");
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let _guard = FileLock::acquire(&lock_path)?;

        // Reload from disk under lock.
        let mut disk: BTreeMap<String, AuthCredential> = if self.path.exists() {
            let raw = fs::read_to_string(&self.path)?;
            if raw.trim().is_empty() {
                BTreeMap::new()
            } else {
                serde_json::from_str(&raw)?
            }
        } else {
            BTreeMap::new()
        };

        let result = f(&mut disk)?;

        let json = serde_json::to_string_pretty(&disk)?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }

        *self.data.lock().expect("auth data lock") = disk;
        Ok(result)
    }
}

fn oauth_to_model_auth(provider: &str, oauth: &OAuthCredential) -> ModelAuth {
    let mut headers = BTreeMap::new();
    let mut base_url = None;
    if provider == PROVIDER_OPENAI_CODEX {
        if let Some(id) = oauth.account_id.as_ref() {
            headers.insert("chatgpt-account-id".into(), id.clone());
        }
        base_url = Some(oauth_codex::DEFAULT_BASE_URL.to_string());
    } else if matches!(provider, PROVIDER_XAI | "grok" | "xai-oauth" | "supergrok") {
        // SuperGrok subscription traffic goes through the CLI chat proxy.
        headers.extend(oauth_xai::cli_identity_headers());
        base_url = Some(oauth_xai::DEFAULT_BASE_URL.to_string());
    }
    ModelAuth {
        api_key: Some(oauth.access.clone()),
        headers,
        base_url,
        source: Some("oauth".into()),
    }
}

fn opencode_model_auth(provider: &str, key: String, source: &str) -> ModelAuth {
    let mut headers = BTreeMap::new();
    let mut base_url = None;
    if matches!(provider, PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO) {
        headers.insert("x-opencode-client".into(), "one".into());
        base_url = Some(login_opencode::base_url_for(provider).to_string());
    }
    ModelAuth {
        api_key: Some(key),
        headers,
        base_url,
        source: Some(source.into()),
    }
}

async fn refresh_oauth(provider: &str, oauth: &OAuthCredential) -> AuthResult<OAuthCredential> {
    match provider {
        PROVIDER_OPENAI_CODEX => oauth_codex::refresh(oauth)
            .await
            .map_err(AuthError::Msg),
        PROVIDER_XAI | "grok" | "xai-oauth" | "supergrok" => {
            oauth_xai::refresh(oauth).await.map_err(AuthError::Msg)
        }
        other => Err(AuthError::Msg(format!(
            "no OAuth refresh implementation for `{other}`"
        ))),
    }
}

fn agent_dir() -> PathBuf {
    // Mirror one-session without adding a crate dependency.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".one/agent")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn env_api_key_name(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" | PROVIDER_OPENAI_CODEX => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "gemini" => Some("GEMINI_API_KEY"),
        PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO => Some("OPENCODE_API_KEY"),
        PROVIDER_XAI | "grok" => Some("XAI_API_KEY"),
        _ => None,
    }
}

fn env_api_key(provider: &str) -> Option<String> {
    match provider {
        "openai" | PROVIDER_OPENAI_CODEX => std::env::var("OPENAI_API_KEY").ok(),
        "anthropic" => std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_OAUTH_TOKEN").ok()),
        "openrouter" => std::env::var("OPENROUTER_API_KEY").ok(),
        "deepseek" => std::env::var("DEEPSEEK_API_KEY").ok(),
        "gemini" => std::env::var("GEMINI_API_KEY")
            .ok()
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok()),
        PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO => std::env::var("OPENCODE_API_KEY")
            .ok()
            .or_else(|| std::env::var("OPENCODE_ZEN_API_KEY").ok()),
        PROVIDER_XAI | "grok" => std::env::var("XAI_API_KEY")
            .ok()
            .or_else(|| std::env::var("XAI_OAUTH_TOKEN").ok()),
        _ => None,
    }
}

/// Cross-process advisory lock via exclusive create + stale recovery.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: &Path) -> AuthResult<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let deadline = SystemTime::now() + Duration::from_secs(15);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Stale lock recovery.
                    if let Ok(meta) = fs::metadata(path) {
                        if let Ok(modified) = meta.modified() {
                            if let Ok(age) = SystemTime::now().duration_since(modified) {
                                if age.as_millis() as u64 > LOCK_STALE_MS {
                                    let _ = fs::remove_file(path);
                                    continue;
                                }
                            }
                        }
                    }
                    if SystemTime::now() > deadline {
                        return Err(AuthError::Msg(format!(
                            "timed out waiting for auth lock {}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_auth() -> (PathBuf, AuthStorage) {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "one-auth-{}-{}-{}",
            std::process::id(),
            n,
            now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let storage = AuthStorage::open(&path).unwrap();
        (dir, storage)
    }

    #[test]
    fn set_get_roundtrip_oauth() {
        let (dir, storage) = temp_auth();
        let mut oauth = AuthCredential::oauth("access-tok", "refresh-tok", now_ms() + 3_600_000);
        if let AuthCredential::OAuth(ref mut o) = oauth {
            o.account_id = Some("acct_123".into());
        }
        storage.set(PROVIDER_OPENAI_CODEX, oauth.clone()).unwrap();
        let got = storage.get(PROVIDER_OPENAI_CODEX).unwrap();
        assert_eq!(got.as_oauth().unwrap().access, "access-tok");
        assert_eq!(
            got.as_oauth().unwrap().account_id.as_deref(),
            Some("acct_123")
        );
        let raw = fs::read_to_string(storage.path()).unwrap();
        assert!(raw.contains("\"type\": \"oauth\""));
        assert!(raw.contains("accountId"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_override_beats_stored() {
        let (dir, storage) = temp_auth();
        storage
            .set("openai", AuthCredential::api_key("stored-key"))
            .unwrap();
        storage.set_runtime_api_key("openai", "runtime-key");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let auth = rt.block_on(storage.resolve_api_key("openai")).unwrap().unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("runtime-key"));
        assert_eq!(auth.source.as_deref(), Some("--api-key"));
        let _ = fs::remove_dir_all(dir);
    }
}
