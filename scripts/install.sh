#!/usr/bin/env bash
# One CLI installer (Grok Build style)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/zhangsanfeng1094/one/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/zhangsanfeng1094/one/main/scripts/install.sh | bash -s 0.1.0
#   ./scripts/install.sh --local

set -euo pipefail

REPO="${ONE_INSTALL_REPO:-zhangsanfeng1094/one}"
ONE_HOME="${ONE_HOME:-$HOME/.one}"
DOWNLOAD_DIR="${ONE_DOWNLOAD_DIR:-$ONE_HOME/downloads}"
BIN_DIR="${ONE_BIN_DIR:-$ONE_HOME/bin}"
VERSION="${ONE_VERSION:-}"
LOCAL_MODE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --local)
      LOCAL_MODE=1
      shift
      ;;
    --version|-v)
      VERSION="${2:-}"
      shift 2
      ;;
    --help|-h)
      cat <<'EOF'
Usage: install.sh [VERSION] [--local]

Options:
  VERSION              Install a specific release version (e.g. 0.1.0 or v0.1.0)
  --version, -v <VER>  Install a specific release version
  --local              Build (if needed) and install from current local repository

Environment variables:
  ONE_INSTALL_REPO      GitHub owner/repo (default: zhangsanfeng1094/one)
  ONE_RELEASE_BASE_URL  Custom artifact mirror URL override
  ONE_VERSION           Target version to install (default: latest)
  ONE_HOME              Base directory (default: ~/.one)
  ONE_BIN_DIR           Binary symlink directory (default: ~/.one/bin)
  ONE_DOWNLOAD_DIR      Downloaded binary cache directory (default: ~/.one/downloads)
  ONE_NO_MODIFY_PATH    Set to 1 to skip updating shell profile PATH
EOF
      exit 0
      ;;
    *)
      if [[ -z "$VERSION" ]]; then
        VERSION="$1"
      fi
      shift
      ;;
  esac
done

info() {
  printf '\033[1;34m==>\033[0m %s\n' "$*"
}

ok() {
  printf '\033[1;32m✓\033[0m %s\n' "$*"
}

warn() {
  printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2
}

err() {
  printf '\033[1;31merror:\033[0m %s\n' "$*" >&2
  exit 1
}

detect_os() {
  local uname_s
  uname_s="$(uname -s 2>/dev/null || echo unknown)"
  case "$uname_s" in
    Linux*)  echo "linux" ;;
    Darwin*) echo "macos" ;;
    MINGW*|MSYS*|CYGWIN*)
      err "Native Windows is not supported (One uses Unix sockets/PTY/process groups). Please run inside WSL2 (Ubuntu/Debian)."
      ;;
    *) err "unsupported operating system: $uname_s" ;;
  esac
}

detect_arch() {
  local os="$1"
  local uname_m
  uname_m="$(uname -m 2>/dev/null || echo unknown)"
  local arch
  case "$uname_m" in
    x86_64|amd64|AMD64) arch="x86_64" ;;
    arm64|aarch64|ARM64) arch="aarch64" ;;
    *) err "unsupported architecture: $uname_m" ;;
  esac

  # Apple Silicon running under Rosetta 2: prefer native aarch64 binary
  if [[ "$os" == "macos" && "$arch" == "x86_64" ]]; then
    if [[ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" == "1" ]]; then
      arch="aarch64"
    fi
  fi

  echo "$arch"
}

normalize_tag() {
  local ver="$1"
  if [[ -z "$ver" || "$ver" == "latest" ]]; then
    echo "latest"
  elif [[ "$ver" == v* ]]; then
    echo "$ver"
  else
    echo "v$ver"
  fi
}

download_file() {
  local url="$1"
  local dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    err "neither curl nor wget is installed"
  fi
}

verify_checksum_if_available() {
  local sums_url="$1"
  local asset_name="$2"
  local file_path="$3"
  local tmp_sums="${file_path}.sha256"

  if ! download_file "$sums_url" "$tmp_sums" 2>/dev/null; then
    rm -f "$tmp_sums"
    return 0
  fi

  local expected
  expected="$(awk -v name="$asset_name" '$2 == name || $2 == ("*" name) { print $1; exit }' "$tmp_sums")"
  rm -f "$tmp_sums"
  if [[ -z "$expected" ]]; then
    return 0
  fi

  local actual=""
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file_path" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file_path" | awk '{print $1}')"
  fi

  if [[ -n "$actual" && "$actual" != "$expected" ]]; then
    rm -f "$file_path"
    err "SHA256 mismatch for $asset_name (expected $expected, got $actual)"
  fi
}

install_binary_into_bin_dir() {
  local stored_bin="$1"
  local os="$2"

  mkdir -p "$BIN_DIR"

  if [[ "$os" == "windows" ]]; then
    local target_exe="$BIN_DIR/one.exe"
    if [[ -f "$target_exe" ]]; then
      mv -f "$target_exe" "${target_exe}.old" 2>/dev/null || true
    fi
    cp -f "$stored_bin" "$target_exe"
    rm -f "${target_exe}.old" 2>/dev/null || true
  else
    local link_target="$stored_bin"
    # Use relative symlink when BIN_DIR and DOWNLOAD_DIR share the same parent
    if [[ "$(dirname "$BIN_DIR")" == "$(dirname "$DOWNLOAD_DIR")" ]]; then
      link_target="../$(basename "$DOWNLOAD_DIR")/$(basename "$stored_bin")"
    fi
    local tmp_link="$BIN_DIR/.one.tmp.$$"
    ln -sf "$link_target" "$tmp_link"
    mv -Tf "$tmp_link" "$BIN_DIR/one" 2>/dev/null || mv -f "$tmp_link" "$BIN_DIR/one"
  fi
}

ensure_path_configured() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) return 0 ;;
  esac

  if [[ "${ONE_NO_MODIFY_PATH:-0}" == "1" ]]; then
    warn "$BIN_DIR is not in your PATH"
    return 0
  fi

  local export_line="export PATH=\"$BIN_DIR:\$PATH\""
  local updated_files=()

  for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
    if [[ -f "$rc" ]]; then
      if ! grep -Fq "$BIN_DIR" "$rc" 2>/dev/null; then
        printf '\n# Added by One installer\n%s\n' "$export_line" >> "$rc"
        updated_files+=("$rc")
      fi
    fi
  done

  local fish_config="$HOME/.config/fish/config.fish"
  if [[ -f "$fish_config" ]]; then
    if ! grep -Fq "$BIN_DIR" "$fish_config" 2>/dev/null; then
      printf '\n# Added by One installer\nfish_add_path "%s"\n' "$BIN_DIR" >> "$fish_config"
      updated_files+=("$fish_config")
    fi
  fi

  if [[ ${#updated_files[@]} -gt 0 ]]; then
    info "Added $BIN_DIR to PATH in: ${updated_files[*]}"
    info "Restart your shell or run: export PATH=\"$BIN_DIR:\$PATH\""
  else
    info "Add $BIN_DIR to your PATH: export PATH=\"$BIN_DIR:\$PATH\""
  fi
}

main() {
  local os arch platform ext asset_name
  os="$(detect_os)"
  arch="$(detect_arch "$os")"
  platform="${os}-${arch}"
  ext=""
  if [[ "$os" == "windows" ]]; then
    ext=".exe"
  fi
  asset_name="one-${platform}${ext}"

  mkdir -p "$DOWNLOAD_DIR" "$BIN_DIR"

  local tmp_file="$DOWNLOAD_DIR/.${asset_name}.part.$$"
  trap 'rm -f "$tmp_file"' EXIT

  if [[ "$LOCAL_MODE" -eq 1 ]]; then
    info "Installing One from local workspace (${platform})..."
    local local_bin="target/release/one${ext}"
    if [[ ! -f "$local_bin" ]]; then
      if [[ -d "crates/one-web/web" ]] && command -v npm >/dev/null 2>&1; then
        info "Building embedded Web UI..."
        (cd crates/one-web/web && npm run build)
      fi
      info "Building release binary with cargo..."
      cargo build --release -p one-cli --bin one
    fi
    cp -f "$local_bin" "$tmp_file"
  else
    local tag base_url download_url sums_url
    tag="$(normalize_tag "$VERSION")"
    if [[ -n "${ONE_RELEASE_BASE_URL:-}" ]]; then
      base_url="${ONE_RELEASE_BASE_URL%/}"
      if [[ "$tag" == "latest" ]]; then
        download_url="${base_url}/latest/${asset_name}"
        sums_url="${base_url}/latest/SHA256SUMS"
      else
        download_url="${base_url}/${tag}/${asset_name}"
        sums_url="${base_url}/${tag}/SHA256SUMS"
      fi
    else
      if [[ "$tag" == "latest" ]]; then
        download_url="https://github.com/${REPO}/releases/latest/download/${asset_name}"
        sums_url="https://github.com/${REPO}/releases/latest/download/SHA256SUMS"
      else
        download_url="https://github.com/${REPO}/releases/download/${tag}/${asset_name}"
        sums_url="https://github.com/${REPO}/releases/download/${tag}/SHA256SUMS"
      fi
    fi

    info "Detected platform: ${platform}"
    info "Downloading ${download_url}..."
    download_file "$download_url" "$tmp_file"
    verify_checksum_if_available "$sums_url" "$asset_name" "$tmp_file"
  fi

  chmod +x "$tmp_file"

  # Smoke test downloaded binary before activating
  local version_out resolved_version
  if ! version_out="$("$tmp_file" --version 2>/dev/null)"; then
    err "downloaded binary failed verification (--version smoke check)"
  fi
  resolved_version="$(awk '{print $2}' <<<"$version_out")"
  if [[ -z "$resolved_version" ]]; then
    resolved_version="${VERSION:-latest}"
  fi

  local stored_bin="$DOWNLOAD_DIR/one-${resolved_version}-${platform}${ext}"
  mv -f "$tmp_file" "$stored_bin"
  trap - EXIT

  install_binary_into_bin_dir "$stored_bin" "$os"
  ok "Installed one ${resolved_version} to ${BIN_DIR}/one${ext}"

  ensure_path_configured
}

main "$@"
