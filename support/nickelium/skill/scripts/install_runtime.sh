#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LOCAL_INSTALLER="$SCRIPT_DIR/../../install.sh"

if [[ -f "$LOCAL_INSTALLER" ]]; then
  exec "$LOCAL_INSTALLER" "$@"
fi

REPO="${NICKELIUM_REPO:-Grail-Computer/Nickelium}"
curl -fsSL "https://raw.githubusercontent.com/${REPO}/main/support/nickelium/install.sh" | bash -s -- "$@"
