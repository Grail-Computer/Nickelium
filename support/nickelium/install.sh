#!/usr/bin/env bash
set -euo pipefail

log() {
  printf '[nickelium] %s\n' "$*" >&2
}

fail() {
  log "$*"
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

detect_os() {
  case "$(uname -s)" in
    Darwin) printf 'darwin' ;;
    Linux) printf 'linux' ;;
    *) fail "Unsupported operating system: $(uname -s)" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    arm64|aarch64) printf 'arm64' ;;
    x86_64|amd64) printf 'x86_64' ;;
    *) fail "Unsupported architecture: $(uname -m)" ;;
  esac
}

download_bundle() {
  local temp_dir="$1"
  local asset_name="$2"
  local repo="${NICKELIUM_REPO:-Grail-Computer/Nickelium}"
  local asset_path="$temp_dir/$asset_name"
  local url="https://github.com/${repo}/releases/latest/download/${asset_name}"

  log "Downloading ${asset_name} from GitHub releases"
  if ! curl -fsSL "$url" -o "$asset_path"; then
    fail "Failed to download ${url}. The latest release may not publish this platform yet."
  fi

  tar -xzf "$asset_path" -C "$temp_dir"
}

resolve_bundle_dir() {
  local root="$1"
  if [[ -x "$root/nickelium" && -d "$root/resources" ]]; then
    printf '%s\n' "$root"
    return 0
  fi

  local candidate
  candidate="$(find "$root" -maxdepth 2 -type f -name nickelium -print -quit 2>/dev/null || true)"
  if [[ -n "$candidate" ]]; then
    candidate="$(cd -- "$(dirname -- "$candidate")" && pwd)"
    if [[ -d "$candidate/resources" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  fi

  return 1
}

install_skill() {
  local bundle_dir="$1"
  local skill_source="$bundle_dir/skill"
  local codex_home="${NICKELIUM_CODEX_HOME:-${CODEX_HOME:-$HOME/.codex}}"
  local skill_dir="$codex_home/skills/nickelium"

  [[ -d "$skill_source" ]] || fail "Release bundle is missing the Nickelium skill payload"

  mkdir -p "$(dirname "$skill_dir")"
  rm -rf "$skill_dir"
  cp -R "$skill_source" "$skill_dir"
  chmod +x "$skill_dir/scripts/install_runtime.sh"

  log "Installed Codex skill at $skill_dir"
}

install_runtime() {
  local bundle_dir="$1"
  local install_dir="${NICKELIUM_INSTALL_DIR:-$HOME/.local/share/nickelium}"
  local runtime_dir="$install_dir/runtime"
  local bin_dir="${NICKELIUM_BIN_DIR:-$HOME/.local/bin}"

  mkdir -p "$runtime_dir" "$bin_dir"
  rm -rf "$runtime_dir"
  mkdir -p "$runtime_dir"

  cp "$bundle_dir/nickelium" "$runtime_dir/nickelium"
  chmod +x "$runtime_dir/nickelium"
  cp -R "$bundle_dir/resources" "$runtime_dir/resources"

  if [[ -d "$bundle_dir/skill" ]]; then
    cp -R "$bundle_dir/skill" "$runtime_dir/skill"
    chmod +x "$runtime_dir/skill/scripts/install_runtime.sh"
  fi

  if [[ -f "$bundle_dir/install.sh" ]]; then
    cp "$bundle_dir/install.sh" "$runtime_dir/install.sh"
    chmod +x "$runtime_dir/install.sh"
  fi

  if [[ -f "$bundle_dir/LICENSE" ]]; then
    cp "$bundle_dir/LICENSE" "$runtime_dir/LICENSE"
  fi

  if [[ -f "$bundle_dir/README.md" ]]; then
    cp "$bundle_dir/README.md" "$runtime_dir/README.md"
  fi

  ln -sf "$runtime_dir/nickelium" "$bin_dir/nickelium"
  ln -sf "$runtime_dir/nickelium" "$bin_dir/servo-agent"

  log "Installed Nickelium runtime at $runtime_dir"
  log "Installed CLI at $bin_dir/nickelium"
}

main() {
  need_cmd curl
  need_cmd tar
  need_cmd find

  local os arch asset_name temp_dir bundle_dir script_path script_dir
  os="$(detect_os)"
  arch="$(detect_arch)"
  asset_name="nickelium-${os}-${arch}.tar.gz"
  temp_dir="$(mktemp -d)"
  trap 'rm -rf "${temp_dir:-}"' EXIT

  script_path="${BASH_SOURCE[0]:-}"
  script_dir=""
  if [[ -n "$script_path" && -f "$script_path" ]]; then
    script_dir="$(cd -- "$(dirname -- "$script_path")" && pwd)"
  fi

  if [[ -n "$script_dir" ]] && bundle_dir="$(resolve_bundle_dir "$script_dir")"; then
    log "Using local Nickelium bundle at $bundle_dir"
  else
    download_bundle "$temp_dir" "$asset_name"
    bundle_dir="$(resolve_bundle_dir "$temp_dir")" || fail "Failed to locate Nickelium runtime inside the downloaded bundle"
  fi

  install_runtime "$bundle_dir"

  if [[ "${NICKELIUM_INSTALL_SKILL:-1}" != "0" ]]; then
    install_skill "$bundle_dir"
  fi

  log "Nickelium is ready. If $HOME/.local/bin is not on PATH, add it before running \`nickelium\`."
}

main "$@"
