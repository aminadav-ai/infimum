#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

mkdir -p logs
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"

# Preserve previous latest.log if it exists (never overwrite experiment logs)
if [[ -f "logs/latest.log" ]]; then
  PREV_MTIME="$(date -r logs/latest.log +%Y%m%d_%H%M%S)"
  mv "logs/latest.log" "logs/archived_${PREV_MTIME}_$$.log"
fi

LOG_FILE="logs/check_${TIMESTAMP}.log"

{
  echo "=== INFIMUM-LANG FULL VERIFICATION SUITE (${TIMESTAMP}) ==="
  rustc --version
  cargo --version
  z3 --version

  echo ""
  echo "--- [1/6] Strict Clippy Check (no todo!, no unimplemented!, no dead_code) ---"
  cargo clippy --all-targets -- -D warnings

  echo ""
  echo "--- [2/6] Full Automated Test Suite + Standard Library std/ Suite ---"
  cargo test -- --nocapture
  cargo run --quiet -- test tests/std_tests.ifm

  echo ""
  echo "--- [3/6] End-to-End Verified Graph Ingestion & Security Passport (examples/payment_demo.ifm) ---"
  rm -rf .infimum
  cargo run --quiet -- ingest examples/payment_demo.ifm
  echo ""
  cargo run --quiet -- passport checkout
  rm -rf .infimum

  echo ""
  echo "--- [4/6] Infimum.Strata Direct Zero-rustc Wasm (.ifw) + Passport v2 (examples/strata_framebuffer_demo.ifm) ---"
  cargo run --quiet -- compile-wasm examples/strata_framebuffer_demo.ifm -o .infimum/demo_strata.ifw
  rm -rf .infimum

  echo ""
  echo "--- [5/6] Anti-Stub Z3 Rejection Demo (examples/anti_stub_demo.ifm) ---"
  if cargo run --quiet -- verify examples/anti_stub_demo.ifm; then
    echo "ERROR: anti_stub_demo.ifm should have been rejected by Z3!"
    exit 1
  else
    echo "SUCCESS: Z3 caught the fake stub and produced a concrete counterexample."
  fi

  echo ""
  echo "--- [6/6] Secret Exfiltration Taint-Block Demo (examples/secret_leak_demo.ifm) ---"
  if cargo run --quiet -- verify examples/secret_leak_demo.ifm; then
    echo "ERROR: secret_leak_demo.ifm should have been blocked by taint checker!"
    exit 1
  else
    echo "SUCCESS: Taint checker blocked unauthorized secret exfiltration."
  fi

  echo ""
  echo "=== ALL INFIMUM CHECKS PASSED ==="
} 2>&1 | tee "$LOG_FILE"

cp "$LOG_FILE" "logs/latest.log"
echo "Saved log to $LOG_FILE"
