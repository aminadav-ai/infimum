#!/usr/bin/env bash
# Copyright (c) 2026 Infimum Language Contributors
# License: Apache-2.0
#
# Full rebuild & verification pipeline for Infimum Code Native (.ifw) + Rust host.

set -euo pipefail

WEB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INFIMUM_BIN="${INFIMUM_BIN:-/home/aminadav/Projects/infimum-lang/target/release/infimum}"

cd "$WEB_DIR"

echo "[1/5] Exporting shared Rust ABI (abi.ifm -> src/studio_abi.rs)..."
"$INFIMUM_BIN" export-abi capsules/studio/src/abi.ifm --rust -o src/studio_abi.rs

echo "[2/5] Verifying Infimum Code capsule (Z3 Telos + Flux + Forma)..."
"$INFIMUM_BIN" verify capsules/studio/src/infimum_code.ifm

echo "[3/5] Compiling Strata Wasm capsule (infimum_code.ifm -> infimum_code.ifw)..."
"$INFIMUM_BIN" compile-wasm capsules/studio/src/infimum_code.ifm -o capsules/studio/infimum_code.ifw

echo "[4/5] Building native infimum-web release binary..."
cargo build --release

echo "[5/5] Running cargo test and headless capsule smoke-check..."
cargo test
./target/release/infimum-web --headless-check capsules/studio/infimum_code.ifw \
  --allow-user-workspaces \
  --allow-net=generativelanguage.googleapis.com \
  --allow-net=api.deepseek.com \
  --allow-net=openrouter.ai \
  --allow-net=api.groq.com \
  --allow-net=api.z.ai \
  --allow-net=api.mistral.ai \
  --allow-net=infimum-lang.org
