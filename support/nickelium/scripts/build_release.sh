#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../../.." && pwd)"
DIST_DIR="${DIST_DIR:-$REPO_ROOT/dist}"

detect_os() {
  case "$(uname -s)" in
    Darwin) printf 'darwin' ;;
    Linux) printf 'linux' ;;
    *) printf 'unsupported' ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    arm64|aarch64) printf 'arm64' ;;
    x86_64|amd64) printf 'x86_64' ;;
    *) printf 'unsupported' ;;
  esac
}

OS="$(detect_os)"
ARCH="$(detect_arch)"
if [[ "$OS" == "unsupported" || "$ARCH" == "unsupported" ]]; then
  printf 'Unsupported release target: %s/%s\n' "$(uname -s)" "$(uname -m)" >&2
  exit 1
fi

PLATFORM="${OS}-${ARCH}"
ASSET_BASENAME="nickelium-${PLATFORM}"
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT

mkdir -p "$DIST_DIR"

cd "$REPO_ROOT"
cargo build -p servoshell --bin nickelium --release --no-default-features --features agent-runtime

BUNDLE_DIR="$STAGE_DIR/$ASSET_BASENAME"
mkdir -p "$BUNDLE_DIR"

install -m 755 "$REPO_ROOT/target/release/nickelium" "$BUNDLE_DIR/nickelium"
cp -R "$REPO_ROOT/resources" "$BUNDLE_DIR/resources"
install -m 755 "$REPO_ROOT/support/nickelium/install.sh" "$BUNDLE_DIR/install.sh"
cp -R "$REPO_ROOT/support/nickelium/skill" "$BUNDLE_DIR/skill"
cp "$REPO_ROOT/LICENSE" "$BUNDLE_DIR/LICENSE"
cp "$REPO_ROOT/README.md" "$BUNDLE_DIR/README.md"

tar -czf "$DIST_DIR/${ASSET_BASENAME}.tar.gz" -C "$STAGE_DIR" "$ASSET_BASENAME"

SKILL_STAGE="$STAGE_DIR/nickelium-skill"
mkdir -p "$SKILL_STAGE"
cp -R "$REPO_ROOT/support/nickelium/skill/." "$SKILL_STAGE/"
tar -czf "$DIST_DIR/nickelium-skill.tar.gz" -C "$STAGE_DIR" "nickelium-skill"

printf 'Created release assets:\n'
printf '  %s\n' "$DIST_DIR/${ASSET_BASENAME}.tar.gz"
printf '  %s\n' "$DIST_DIR/nickelium-skill.tar.gz"
