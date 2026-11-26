#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MONOREPO_ROOT="$(cd "$REPO_ROOT/../.." && pwd)"
if [[ -z "${INF_BIN:-}" ]]; then
  if [[ -x "$MONOREPO_ROOT/target/release/infimum" ]]; then
    INF_BIN="$MONOREPO_ROOT/target/release/infimum"
  elif [[ -x "$REPO_ROOT/target/release/infimum" ]]; then
    INF_BIN="$REPO_ROOT/target/release/infimum"
  else
    echo "Building infimum compiler (release)..."
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    if [[ -x "$MONOREPO_ROOT/target/release/infimum" ]]; then
      INF_BIN="$MONOREPO_ROOT/target/release/infimum"
    else
      INF_BIN="$REPO_ROOT/target/release/infimum"
    fi
  fi
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

check_ifm() {
  local label="$1"
  local path="$2"
  local run_tests="$3"
  local do_wasm="$4"
  echo "  [verify] $label"
  "$INF_BIN" verify "$path" >/dev/null
  if [[ "$run_tests" == "1" ]]; then
    echo "  [test]   $label"
    "$INF_BIN" test "$path"
  fi
  if [[ "$do_wasm" == "1" ]]; then
    echo "  [wasm]   $label"
    "$INF_BIN" compile-wasm "$path" -o "$TMP_DIR/$(basename "$path" .ifm).ifw" >/dev/null
  fi
}

echo "=== [1/5] Verifying infimum-lang stdlib tests & examples ==="
check_ifm "tests/std_tests.ifm" "$REPO_ROOT/tests/std_tests.ifm" 1 0
check_ifm "examples/forma_ui_demo.ifm" "$REPO_ROOT/examples/forma_ui_demo.ifm" 1 1
check_ifm "examples/slices_vec_demo.ifm" "$REPO_ROOT/examples/slices_vec_demo.ifm" 1 1
check_ifm "examples/mem_regions_demo.ifm" "$REPO_ROOT/examples/mem_regions_demo.ifm" 1 1
check_ifm "showcase/ceres-drift" "$REPO_ROOT/examples/showcase/ceres-drift/main.ifm" 1 1
check_ifm "showcase/color-lines" "$REPO_ROOT/examples/showcase/color-lines/src/main.ifm" 1 1
check_ifm "showcase/gomoku" "$REPO_ROOT/examples/showcase/gomoku/src/main.ifm" 1 1
check_ifm "showcase/lion-herd" "$REPO_ROOT/examples/showcase/lion-herd/src/main.ifm" 1 1
check_ifm "showcase/slipgate" "$REPO_ROOT/examples/showcase/slipgate/src/main.ifm" 1 1

echo ""
echo "=== [2/5] Extracting & verifying Example 1 from docs/INFIMUM_LANG_GUIDE.md ==="
python3 - "$REPO_ROOT/docs/INFIMUM_LANG_GUIDE.md" "$TMP_DIR/guide_ex1.ifm" <<'PY'
import sys
from pathlib import Path
md = Path(sys.argv[1]).read_text(encoding="utf-8")
idx = md.index("### Example 1:")
s = md.index("```infimum\n", idx) + len("```infimum\n")
e = md.index("\n```", s)
Path(sys.argv[2]).write_text(md[s:e] + "\n", encoding="utf-8")
PY
check_ifm "INFIMUM_LANG_GUIDE.md Example 1" "$TMP_DIR/guide_ex1.ifm" 0 1

if [[ -d "$MONOREPO_ROOT/crates/infimum-web/capsules" ]]; then
  echo ""
  echo "=== [3/5] Verifying infimum-web cores & demos ==="
  check_ifm "web-cores/audio_core.ifm" "$MONOREPO_ROOT/crates/infimum-web/capsules/core/audio_core.ifm" 1 1
  check_ifm "web-cores/passport_core.ifm" "$MONOREPO_ROOT/crates/infimum-web/capsules/core/passport_core.ifm" 1 1
  check_ifm "web-cores/sanitizer_core.ifm" "$MONOREPO_ROOT/crates/infimum-web/capsules/core/sanitizer_core.ifm" 1 1
  check_ifm "web-cores/text_demo.ifm" "$MONOREPO_ROOT/crates/infimum-web/capsules/m2_text_demo/src/text_demo.ifm" 1 1
fi

INFIMUM_CODE_DIR=""
if [[ -d "$MONOREPO_ROOT/apps/infimum-code/capsule/src" ]]; then
  INFIMUM_CODE_DIR="$MONOREPO_ROOT/apps/infimum-code"
fi

if [[ -n "$INFIMUM_CODE_DIR" ]]; then
  echo ""
  echo "=== [4/5] Verifying infimum-code capsule, cores, templates & guide sync ==="
  check_ifm "infimum-code/infimum_code.ifm" "$INFIMUM_CODE_DIR/capsule/src/infimum_code.ifm" 1 1
  check_ifm "infimum-code/agent_core.ifm" "$INFIMUM_CODE_DIR/cores/agent_core.ifm" 1 1
  check_ifm "infimum-code/editor_core.ifm" "$INFIMUM_CODE_DIR/cores/editor_core.ifm" 1 1
  check_ifm "infimum-code/outline_core.ifm" "$INFIMUM_CODE_DIR/cores/outline_core.ifm" 1 1

  if [[ -f "$INFIMUM_CODE_DIR/daemon/src/studio_abi.rs" ]]; then
    "$INF_BIN" export-abi "$INFIMUM_CODE_DIR/capsule/src/abi.ifm" --rust -o "$TMP_DIR/studio_abi.rs" >/dev/null
    cmp -s "$TMP_DIR/studio_abi.rs" "$INFIMUM_CODE_DIR/daemon/src/studio_abi.rs" \
      || { echo "ERROR: apps/infimum-code/daemon/src/studio_abi.rs is out of sync with abi.ifm!"; exit 1; }
    echo "  [sync]   apps/infimum-code/daemon/src/studio_abi.rs matches canonical abi.ifm"
  fi
fi

SITE_DIR=""
if [[ -f "$MONOREPO_ROOT/../infimum-site/public/code/agent_worker.js" ]]; then
  SITE_DIR="$(cd "$MONOREPO_ROOT/../infimum-site" && pwd)"
fi

if [[ -n "$SITE_DIR" ]]; then
  cmp -s "$REPO_ROOT/docs/INFIMUM_LANG_GUIDE.md" "$SITE_DIR/public/code/INFIMUM_LANG_GUIDE.md" \
    || { echo "ERROR: infimum-site/public/code/INFIMUM_LANG_GUIDE.md is out of sync!"; exit 1; }
  echo "  [sync]   infimum-site/public/code/INFIMUM_LANG_GUIDE.md matches canonical guide"

  if [[ -n "$INFIMUM_CODE_DIR" && -f "$SITE_DIR/public/code/studio_abi.js" ]]; then
    "$INF_BIN" export-abi "$INFIMUM_CODE_DIR/capsule/src/abi.ifm" --js -o "$TMP_DIR/studio_abi.js" >/dev/null
    cmp -s "$TMP_DIR/studio_abi.js" "$SITE_DIR/public/code/studio_abi.js" \
      || { echo "ERROR: infimum-site/public/code/studio_abi.js is out of sync with abi.ifm!"; exit 1; }
    echo "  [sync]   infimum-site/public/code/studio_abi.js matches canonical abi.ifm"
  fi

  python3 - "$SITE_DIR/public/code/agent_worker.js" "$TMP_DIR" <<'PY'
import re, sys
from pathlib import Path
js = Path(sys.argv[1]).read_text(encoding="utf-8")
out_dir = Path(sys.argv[2])
for kind in ("ui", "game2d", "game3d"):
    m = re.search(rf"\b{kind}:\s*\{{[^}}]*?source:\s*`([\s\S]*?)`,", js)
    assert m, f"Template '{kind}' not found in agent_worker.js"
    (out_dir / f"tpl_{kind}.ifm").write_text(m.group(1), encoding="utf-8")
PY
  check_ifm "templates/ui" "$TMP_DIR/tpl_ui.ifm" 1 1
  check_ifm "templates/game2d" "$TMP_DIR/tpl_game2d.ifm" 1 1
  check_ifm "templates/game3d" "$TMP_DIR/tpl_game3d.ifm" 1 1
fi

echo ""
echo "=== [5/5] Verifying license headers & copyright hygiene ==="
python3 - "$MONOREPO_ROOT" <<'PY'
import sys
from pathlib import Path

root = Path(sys.argv[1])
SCAN_EXTS = {".rs", ".ifm", ".wgsl", ".sh", ".md", ".toml"}
SKIP_DIRS = {".git", "target", "node_modules"}

APACHE_ALLOWED_PREFIXES = (
    "crates/infimum-lang/std/",
    "crates/infimum-lang/examples/",
    "crates/infimum-web/capsules/m2_text_demo/",
)
APACHE_ALLOWED_EXACT = {
    "crates/infimum-lang/src/forma/widgets_rt.ifm",
    "LICENSE-APACHE",
}
DOC_MENTION_ALLOWED = {
    "README.md",
    "LICENSE",
    "CONTRIBUTING.md",
    "crates/infimum-lang/README.md",
    "crates/infimum-lang/docs/INFIMUM_LANG_GUIDE.md",
}

FORBIDDEN_POLYFORM = "Poly" + "Form"
LEGACY_COPYRIGHT = "Infimum Language " + "Contributors"
LEGACY_APACHE_HDR = "License: " + "Apache-2.0"

errors = []
for p in sorted(root.rglob("*")):
    if not p.is_file():
        continue
    rel = p.relative_to(root).as_posix()
    if any(part in SKIP_DIRS for part in p.parts):
        continue
    if p.suffix not in SCAN_EXTS and p.name not in ("LICENSE", "LICENSE-APACHE"):
        continue

    text = p.read_text(encoding="utf-8", errors="replace")
    head = "\n".join(text.splitlines()[:12])

    if FORBIDDEN_POLYFORM in text:
        errors.append(f"{rel}: forbidden legacy license '{FORBIDDEN_POLYFORM}'")
    if LEGACY_COPYRIGHT in head:
        errors.append(f"{rel}: legacy copyright '{LEGACY_COPYRIGHT}' (use 'Yehuda Aminadav')")
    if LEGACY_APACHE_HDR in head:
        errors.append(f"{rel}: use 'SPDX-License-Identifier:' instead of '{LEGACY_APACHE_HDR}'")

    is_apache_allowed = (
        rel in APACHE_ALLOWED_EXACT
        or any(rel.startswith(pref) for pref in APACHE_ALLOWED_PREFIXES)
    )
    if not is_apache_allowed and rel not in DOC_MENTION_ALLOWED:
        if "SPDX-License-Identifier: Apache-2.0" in head or "Apache-2.0" in head:
            errors.append(f"{rel}: Apache-2.0 header outside allowed std/examples/widgets_rt paths")

    if is_apache_allowed and p.suffix == ".ifm":
        if "AGPL-3.0" in head:
            errors.append(f"{rel}: AGPL-3.0 header inside Apache-2.0 exception path")

if errors:
    print("LICENSE HYGIENE CHECK FAILED:")
    for e in errors:
        print("  -", e)
    sys.exit(1)

print("  [license] All monorepo license & copyright headers verified clean")
PY

echo ""
echo "=== ALL CORPUS, TEMPLATES, GUIDE & LICENSE CHECKS PASSED ==="
