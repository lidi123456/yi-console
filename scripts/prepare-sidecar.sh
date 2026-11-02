#!/usr/bin/env bash
# 编译本机用的 yi-agentd，放到 Tauri sidecar 目录（binaries/yi-agentd-<target-triple>）。
# 用法：scripts/prepare-sidecar.sh [target-triple]，默认是当前 rustc 的 host。
set -euo pipefail
cd "$(dirname "$0")/.."
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
cargo build --release -p yi-agentd --target "$target"
ext=""
[[ $target == *windows* ]] && ext=".exe"
mkdir -p app/src-tauri/binaries
cp "target/$target/release/yi-agentd$ext" "app/src-tauri/binaries/yi-agentd-$target$ext"
echo "app/src-tauri/binaries/yi-agentd-$target$ext"
