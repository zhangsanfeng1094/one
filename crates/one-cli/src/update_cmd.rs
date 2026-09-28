use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};

const DEFAULT_REPO: &str = "zhangsanfeng1094/one";

/// CLI: `one update` — check for updates and self-update the `one` binary (Grok Build style).
#[derive(Debug, Clone, Args)]
pub struct UpdateCli {
    /// Only check whether an update is available without installing.
    #[arg(long)]
    pub check: bool,

    /// Install a specific version (e.g. `0.1.0` or `v0.1.0`). Defaults to `latest`.
    #[arg(long, short = 'v', value_name = "VERSION")]
    pub version: Option<String>,

    /// Force download and reinstall even if already on the target version.
    #[arg(long = "force-reinstall", visible_alias = "force", short = 'f')]
    pub force_reinstall: bool,

    /// Print machine-readable JSON result.
    #[arg(long)]
    pub json: bool,

    /// GitHub `owner/repo` override (default: `zhangsanfeng1094/one` or `ONE_UPDATE_REPO`).
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallerKind {
    Script,
    Cargo,
    Standalone,
}

impl InstallerKind {
    pub fn detect(current_exe: Option<&Path>) -> Self {
        let Some(exe) = current_exe else {
            return Self::Script;
        };
        let s = exe.to_string_lossy();
        if s.contains(".one/bin") || s.contains(".one/downloads") || s.contains(r".one\bin") {
            Self::Script
        } else if s.contains(".cargo/bin") || s.contains(r".cargo\bin") {
            Self::Cargo
        } else {
            Self::Standalone
        }
    }

    pub fn reinstall_hint(self, repo: &str) -> String {
        match self {
            Self::Script | Self::Standalone => {
                if cfg!(windows) {
                    format!(
                        "curl -fsSL https://raw.githubusercontent.com/{repo}/main/scripts/install.sh | bash"
                    )
                } else {
                    format!(
                        "curl -fsSL https://raw.githubusercontent.com/{repo}/main/scripts/install.sh | bash"
                    )
                }
            }
            Self::Cargo => {
                format!(
                    "cargo install --git https://github.com/{repo}.git one-cli --bin one --force"
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub current_version: String,
    pub target_version: String,
    pub update_available: bool,
    pub updated: bool,
    pub platform: String,
    pub binary_path: Option<String>,
    pub installer: InstallerKind,
    pub reinstall_hint: String,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches('v').to_string()
}

pub fn normalize_tag(v: &str) -> String {
    let trimmed = v.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("latest") {
        "latest".to_string()
    } else if trimmed.starts_with('v') {
        trimmed.to_string()
    } else {
        format!("v{trimmed}")
    }
}

/// Compare two semver-like strings (`0.1.2` vs `0.1.10`).
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    let parse_parts = |s: &str| -> Vec<u64> {
        normalize_version(s)
            .split(['.', '-'])
            .take(3)
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let cur = parse_parts(current);
    let cand = parse_parts(candidate);
    cand > cur
}

pub fn detect_platform() -> Result<(&'static str, &'static str), String> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return Err(format!("unsupported OS: {}", std::env::consts::OS));
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return Err(format!(
            "unsupported architecture: {}",
            std::env::consts::ARCH
        ));
    };

    Ok((os, arch))
}

pub fn asset_name_for(os: &str, arch: &str) -> String {
    if os == "windows" {
        format!("one-{os}-{arch}.exe")
    } else {
        format!("one-{os}-{arch}")
    }
}

pub fn parse_sha256_for_asset(sums_text: &str, asset_name: &str) -> Option<String> {
    for line in sums_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        if file == asset_name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn compute_sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn one_home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("ONE_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    if let Some(parent) = one_session::agent_dir().parent() {
        return parent.to_path_buf();
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".one")
}

fn downloads_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ONE_DOWNLOAD_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    one_home_dir().join("downloads")
}

fn bin_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ONE_BIN_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    one_home_dir().join("bin")
}

async fn resolve_target_version(
    client: &reqwest::Client,
    repo: &str,
    base_url_override: Option<&str>,
    requested: Option<&str>,
) -> Result<String, String> {
    if let Some(v) = requested
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("latest"))
    {
        return Ok(normalize_version(v));
    }

    if let Some(base) = base_url_override {
        let url = format!("{}/latest/version", base.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("failed to query latest version from {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "latest version check failed ({}) at {url}",
                resp.status()
            ));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("failed to read version response: {e}"))?;
        let ver = normalize_version(&body);
        if ver.is_empty() {
            return Err(format!("empty version returned from {url}"));
        }
        return Ok(ver);
    }

    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = client
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("failed to query GitHub latest release ({api_url}): {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "GitHub release check returned HTTP {} for {repo} (has a release been published yet?)",
            resp.status()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON from GitHub releases API: {e}"))?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "GitHub release response missing `tag_name`".to_string())?;
    Ok(normalize_version(tag))
}

fn build_download_urls(
    repo: &str,
    base_url_override: Option<&str>,
    version: &str,
    asset_name: &str,
) -> (String, String) {
    let tag = normalize_tag(version);
    if let Some(base) = base_url_override {
        let base = base.trim_end_matches('/');
        (
            format!("{base}/{tag}/{asset_name}"),
            format!("{base}/{tag}/SHA256SUMS"),
        )
    } else {
        (
            format!("https://github.com/{repo}/releases/download/{tag}/{asset_name}"),
            format!("https://github.com/{repo}/releases/download/{tag}/SHA256SUMS"),
        )
    }
}

fn smoke_test_binary(path: &Path) -> Result<String, String> {
    let output = Command::new(path).arg("--version").output().map_err(|e| {
        format!(
            "failed to run `--version` smoke test on {}: {e}",
            path.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "downloaded binary exited with {} during `--version` smoke test",
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn install_into_bin_dir(stored_bin: &Path, bin_dir: &Path, os: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(bin_dir)
        .map_err(|e| format!("failed to create {}: {e}", bin_dir.display()))?;

    if os == "windows" {
        let dest = bin_dir.join("one.exe");
        let old = bin_dir.join("one.exe.old");
        if dest.exists() {
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&dest, &old);
        }
        std::fs::copy(stored_bin, &dest)
            .map_err(|e| format!("failed to copy binary to {}: {e}", dest.display()))?;
        let _ = std::fs::remove_file(&old);
        Ok(dest)
    } else {
        let dest = bin_dir.join("one");
        let tmp_link = bin_dir.join(format!(".one.tmp.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp_link);

        let link_target = if bin_dir.parent().is_some()
            && bin_dir.parent() == stored_bin.parent().and_then(|p| p.parent())
        {
            let dl_name = stored_bin
                .parent()
                .and_then(|p| p.file_name())
                .unwrap_or_default();
            let file_name = stored_bin.file_name().unwrap_or_default();
            PathBuf::from("..").join(dl_name).join(file_name)
        } else {
            stored_bin.to_path_buf()
        };

        #[cfg(unix)]
        std::os::unix::fs::symlink(&link_target, &tmp_link)
            .map_err(|e| format!("failed to create symlink {}: {e}", tmp_link.display()))?;

        #[cfg(not(unix))]
        std::fs::copy(stored_bin, &tmp_link)
            .map_err(|e| format!("failed to copy binary {}: {e}", tmp_link.display()))?;

        std::fs::rename(&tmp_link, &dest)
            .map_err(|e| format!("failed to activate {}: {e}", dest.display()))?;
        Ok(dest)
    }
}

pub async fn run_update(cli: UpdateCli) -> Result<(), Box<dyn std::error::Error>> {
    let repo = cli
        .repo
        .clone()
        .or_else(|| std::env::var("ONE_UPDATE_REPO").ok())
        .or_else(|| std::env::var("ONE_INSTALL_REPO").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string());

    let base_url_override = std::env::var("ONE_RELEASE_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let (os, arch) = detect_platform()?;
    let platform = format!("{os}-{arch}");
    let asset_name = asset_name_for(os, arch);

    let current_exe = std::env::current_exe().ok();
    let installer = InstallerKind::detect(current_exe.as_deref());
    let hint = installer.reinstall_hint(&repo);
    let cur_ver = current_version().to_string();

    let client = reqwest::Client::builder()
        .user_agent(format!("one-cli/{cur_ver}"))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let target_ver = match resolve_target_version(
        &client,
        &repo,
        base_url_override.as_deref(),
        cli.version.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("reinstall hint: {hint}");
            return Err(err.into());
        }
    };

    let update_available = if cli.version.is_some() {
        normalize_version(&cur_ver) != normalize_version(&target_ver)
    } else {
        is_newer_version(&cur_ver, &target_ver)
    };

    if cli.check {
        let report = UpdateReport {
            current_version: cur_ver.clone(),
            target_version: target_ver.clone(),
            update_available,
            updated: false,
            platform,
            binary_path: current_exe.map(|p| p.display().to_string()),
            installer,
            reinstall_hint: hint,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else if update_available {
            println!("update available: {cur_ver} -> {target_ver} (run `one update` to install)");
        } else {
            println!("one {cur_ver} is up to date");
        }
        return Ok(());
    }

    let dl_dir = downloads_dir();
    let target_bin_dir = bin_dir();
    let ext = if os == "windows" { ".exe" } else { "" };
    let stored_bin = dl_dir.join(format!("one-{target_ver}-{platform}{ext}"));

    if !update_available && !cli.force_reinstall && stored_bin.is_file() {
        let report = UpdateReport {
            current_version: cur_ver.clone(),
            target_version: target_ver.clone(),
            update_available: false,
            updated: false,
            platform,
            binary_path: Some(stored_bin.display().to_string()),
            installer,
            reinstall_hint: hint,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("one {cur_ver} is already up to date (use `--force-reinstall` to reinstall)");
        }
        return Ok(());
    }

    if !update_available
        && !cli.force_reinstall
        && normalize_version(&cur_ver) == normalize_version(&target_ver)
    {
        let report = UpdateReport {
            current_version: cur_ver.clone(),
            target_version: target_ver.clone(),
            update_available: false,
            updated: false,
            platform,
            binary_path: current_exe.map(|p| p.display().to_string()),
            installer,
            reinstall_hint: hint,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("one {cur_ver} is already up to date (use `--force-reinstall` to reinstall)");
        }
        return Ok(());
    }

    std::fs::create_dir_all(&dl_dir)?;
    let (download_url, sums_url) = build_download_urls(
        &repo,
        base_url_override.as_deref(),
        &target_ver,
        &asset_name,
    );

    if !cli.json {
        eprintln!("downloading one {target_ver} ({platform}) from {download_url}...");
    }

    let resp = client.get(&download_url).send().await.map_err(|e| {
        eprintln!("reinstall hint: {hint}");
        format!("download failed: {e}")
    })?;

    if !resp.status().is_success() {
        eprintln!("reinstall hint: {hint}");
        return Err(format!(
            "download failed with HTTP {} from {download_url}",
            resp.status()
        )
        .into());
    }

    let bytes = resp.bytes().await?;

    // Optional SHA256SUMS verification when published alongside release
    if let Ok(sums_resp) = client.get(&sums_url).send().await {
        if sums_resp.status().is_success() {
            if let Ok(sums_text) = sums_resp.text().await {
                if let Some(expected) = parse_sha256_for_asset(&sums_text, &asset_name) {
                    let actual = compute_sha256_hex(&bytes);
                    if actual != expected {
                        return Err(format!(
                            "SHA256 mismatch for {asset_name} (expected {expected}, got {actual})"
                        )
                        .into());
                    }
                }
            }
        }
    }

    let tmp_file = dl_dir.join(format!(
        ".{asset_name}.part.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp_file, &bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_file, std::fs::Permissions::from_mode(0o755))?;
    }

    if let Err(err) = smoke_test_binary(&tmp_file) {
        let _ = std::fs::remove_file(&tmp_file);
        eprintln!("reinstall hint: {hint}");
        return Err(err.into());
    }

    std::fs::rename(&tmp_file, &stored_bin)?;
    let installed_link = install_into_bin_dir(&stored_bin, &target_bin_dir, os)?;

    // Also update in-place if running from a standalone binary outside ~/.one
    if let Some(ref exe) = current_exe {
        let canonical_link = installed_link.canonicalize().ok();
        let canonical_exe = exe.canonicalize().ok();
        if canonical_link != canonical_exe && !exe.starts_with(&dl_dir) {
            if let Some(parent) = exe.parent() {
                let sibling_tmp = parent.join(format!(".one.update.{}", std::process::id()));
                if std::fs::copy(&stored_bin, &sibling_tmp).is_ok() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &sibling_tmp,
                            std::fs::Permissions::from_mode(0o755),
                        );
                    }
                    if std::fs::rename(&sibling_tmp, exe).is_err() {
                        let _ = std::fs::remove_file(&sibling_tmp);
                    }
                }
            }
        }
    }

    let report = UpdateReport {
        current_version: cur_ver.clone(),
        target_version: target_ver.clone(),
        update_available,
        updated: true,
        platform,
        binary_path: Some(installed_link.display().to_string()),
        installer,
        reinstall_hint: hint,
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "updated one: {cur_ver} -> {target_ver} ({})",
            installed_link.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_normalization_and_comparison() {
        assert_eq!(normalize_version("v0.2.1"), "0.2.1");
        assert_eq!(normalize_version("0.2.1\n"), "0.2.1");
        assert_eq!(normalize_tag("0.2.1"), "v0.2.1");
        assert_eq!(normalize_tag("v0.2.1"), "v0.2.1");
        assert_eq!(normalize_tag("latest"), "latest");

        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.9", "0.2.0"));
        assert!(!is_newer_version("0.2.0", "0.2.0"));
        assert!(!is_newer_version("0.2.1", "0.2.0"));
    }

    #[test]
    fn parses_sha256sums_correctly() {
        let sample = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  one-linux-x86_64
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *one-macos-aarch64
";
        assert_eq!(
            parse_sha256_for_asset(sample, "one-linux-x86_64").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            parse_sha256_for_asset(sample, "one-macos-aarch64").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            parse_sha256_for_asset(sample, "one-windows-x86_64.exe"),
            None
        );
    }

    #[test]
    fn detects_installer_kind_and_reinstall_hint() {
        let script_exe = Path::new("/home/user/.one/bin/one");
        let cargo_exe = Path::new("/home/user/.cargo/bin/one");
        let usr_exe = Path::new("/usr/local/bin/one");

        assert_eq!(
            InstallerKind::detect(Some(script_exe)),
            InstallerKind::Script
        );
        assert_eq!(InstallerKind::detect(Some(cargo_exe)), InstallerKind::Cargo);
        assert_eq!(
            InstallerKind::detect(Some(usr_exe)),
            InstallerKind::Standalone
        );

        assert!(InstallerKind::Script
            .reinstall_hint("zhangsanfeng1094/one")
            .contains("scripts/install.sh"));
        assert!(InstallerKind::Cargo
            .reinstall_hint("zhangsanfeng1094/one")
            .contains("cargo install"));
    }

    #[cfg(unix)]
    #[test]
    fn installs_relative_symlink_into_bin_dir() {
        let tmp = std::env::temp_dir().join(format!("one-update-test-{}", uuid::Uuid::new_v4()));
        let dl_dir = tmp.join("downloads");
        let bin_dir = tmp.join("bin");
        std::fs::create_dir_all(&dl_dir).unwrap();

        let stored = dl_dir.join("one-0.2.0-linux-x86_64");
        std::fs::write(&stored, b"#!/bin/sh\necho 'one 0.2.0'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ver = smoke_test_binary(&stored).unwrap();
        assert_eq!(ver, "one 0.2.0");

        let link = install_into_bin_dir(&stored, &bin_dir, "linux").unwrap();
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, PathBuf::from("../downloads/one-0.2.0-linux-x86_64"));
        assert_eq!(smoke_test_binary(&link).unwrap(), "one 0.2.0");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
