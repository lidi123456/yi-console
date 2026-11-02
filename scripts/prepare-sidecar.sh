#!/usr/bin/env bash
# Build yi-agentd for this machine and put it in the Tauri sidecar directory (binaries/yi-agentd-<target-triple>).
# Usage: scripts/prepare-sidecar.sh [target-triple]; defaults to the current rustc host.
set -euo pipefail
cd "$(dirname "$0")/.."
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
cargo build --release -p yi-agentd --target "$target"
ext=""
[[ $target == *windows* ]] && ext=".exe"
mkdir -p app/src-tauri/binaries
cp "target/$target/release/yi-agentd$ext" "app/src-tauri/binaries/yi-agentd-$target$ext"
echo "app/src-tauri/binaries/yi-agentd-$target$ext"
