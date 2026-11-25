#!/usr/bin/env bash
# Copyright (c) 2026 Yehuda Aminadav
# SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial
#
# Full rebuild & verification pipeline for Infimum Code Native (.ifw) + Rust host.

set -euo pipefail

WEB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MONOREPO_ROOT="$(cd "$WEB_DIR/../.." && pwd)"
STUDIO_DIR="$MONOREPO_ROOT/apps/infimum-code"
INFIMUM_BIN="${INFIMUM_BIN:-$MONOREPO_ROOT/target/release/infimum}"

cd "$MONOREPO_ROOT"

if [[ ! -x "$INFIMUM_BIN" ]]; then
  echo "[0/5] Building infimum compiler (release)..."
  cargo build --release -p infimum-lang
fi

echo "[1/5] Exporting shared Rust & JS ABI (abi.ifm -> studio_abi.rs / studio_abi.js)..."
"$INFIMUM_BIN" export-abi "$STUDIO_DIR/capsule/src/abi.ifm" --rust -o "$STUDIO_DIR/daemon/src/studio_abi.rs"
if [[ -d "$MONOREPO_ROOT/../infimum-site/public/code" ]]; then
  "$INFIMUM_BIN" export-abi "$STUDIO_DIR/capsule/src/abi.ifm" --js -o "$MONOREPO_ROOT/../infimum-site/public/code/studio_abi.js"
fi

echo "[2/5] Verifying Infimum Code capsule (Z3 Telos + Flux + Forma)..."
"$INFIMUM_BIN" verify "$STUDIO_DIR/capsule/src/infimum_code.ifm"

echo "[3/5] Compiling Strata Wasm capsule (infimum_code.ifm -> infimum_code.ifw)..."
"$INFIMUM_BIN" compile-wasm "$STUDIO_DIR/capsule/src/infimum_code.ifm" -o "$STUDIO_DIR/capsule/infimum_code.ifw"

echo "[4/5] Building native workspace release binaries (infimum, infimum-web, infimum-agentd)..."
cargo build --release --workspace

echo "[5/5] Running workspace tests and headless capsule smoke-check..."
cargo test --workspace
"$MONOREPO_ROOT/target/release/infimum-web" --headless-check "$STUDIO_DIR/capsule/infimum_code.ifw" \
  --allow-clipboard \
  --allow-net=127.0.0.1 \
  --allow-net=infimum-lang.org
