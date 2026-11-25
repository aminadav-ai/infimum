#!/usr/bin/env python3
# Copyright (c) 2026 Yehuda Aminadav
# SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial
"""Deterministic repository source bundle builder for Infimum Code AI.

Reads tracked files strictly via `git ls-files -z` (never untracked secrets or build outputs),
filters to a strict text whitelist, and emits:
  - <out-dir>/<bundle-name>  (default: repo_bundle.json.gz, gzip level 9, mtime=0)
  - <out-dir>/<meta-name>    (default: repo_meta.json)
"""

from __future__ import annotations

import argparse
import datetime as dt
import gzip
import json
import subprocess
import sys
from pathlib import Path

ALLOWED_EXTS = {
    ".rs",
    ".ifm",
    ".wgsl",
    ".toml",
    ".md",
    ".sh",
    ".yml",
    ".yaml",
    ".json",
    ".js",
    ".html",
    ".css",
    ".py",
    ".txt",
}

ALLOWED_EXACT_NAMES = {
    "LICENSE",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "NOTICE",
    "_headers",
    ".gitignore",
}

SKIP_EXTS = {
    ".ifw",
    ".wasm",
    ".png",
    ".jpg",
    ".jpeg",
    ".webp",
    ".gif",
    ".ttf",
    ".otf",
    ".woff",
    ".woff2",
    ".ico",
    ".gz",
    ".zip",
    ".pdf",
}

MAX_FILE_BYTES = 512 * 1024


def git_output(repo_root: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", *args], cwd=repo_root)


def is_allowed_path(rel_path: str) -> bool:
    p = Path(rel_path)
    ext = p.suffix.lower()
    if ext in SKIP_EXTS:
        return False
    if p.name in ALLOWED_EXACT_NAMES:
        return True
    return ext in ALLOWED_EXTS


def main() -> int:
    parser = argparse.ArgumentParser(description="Build gzipped JSON repository snapshot.")
    parser.add_argument("--repo-root", default=".", help="Path to git repository root")
    parser.add_argument("--out-dir", required=True, help="Output directory")
    parser.add_argument("--repo-name", default="aminadav-ai/infimum", help="Repository identifier")
    parser.add_argument(
        "--repo-prefix",
        default="",
        help="Optional virtual path prefix to prepend to every file key (e.g. 'site')",
    )
    parser.add_argument(
        "--bundle-name",
        default="repo_bundle.json.gz",
        help="Output gzipped bundle filename",
    )
    parser.add_argument(
        "--meta-name",
        default="repo_meta.json",
        help="Output metadata JSON filename (pass empty string to skip)",
    )
    parser.add_argument(
        "--exclude-prefix",
        action="append",
        default=[],
        help="Optional relative path prefix to skip (repeatable)",
    )
    args = parser.parse_args()

    repo_root = Path(args.repo_root).resolve()
    out_dir = Path(args.out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    sha = git_output(repo_root, "rev-parse", "HEAD").decode("utf-8").strip()
    commit_iso = git_output(repo_root, "show", "-s", "--format=%cI", "HEAD").decode("utf-8").strip()
    built_at = commit_iso or dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    raw_list = git_output(repo_root, "ls-files", "-z")
    tracked = sorted(
        entry.decode("utf-8")
        for entry in raw_list.split(b"\0")
        if entry
    )

    prefix = args.repo_prefix.strip("/")
    exclude_prefixes = [e.strip("/") + "/" for e in args.exclude_prefix if e.strip("/")]

    files: dict[str, str] = {}
    skipped: list[str] = []

    for rel in tracked:
        if any(rel.startswith(ep) or rel == ep.rstrip("/") for ep in exclude_prefixes):
            continue
        if not is_allowed_path(rel):
            continue
        full = repo_root / rel
        if not full.is_file():
            continue
        data = full.read_bytes()
        if len(data) > MAX_FILE_BYTES:
            skipped.append(f"{rel} ({len(data)} bytes > {MAX_FILE_BYTES})")
            continue
        if b"\0" in data:
            skipped.append(f"{rel} (contains NUL byte)")
            continue
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            skipped.append(f"{rel} (non-UTF-8)")
            continue
        key = f"{prefix}/{rel}" if prefix else rel
        files[key] = text

    bundle_obj = {
        "version": 1,
        "repo": args.repo_name,
        "sha": sha,
        "built_at": built_at,
        "files": files,
    }
    raw_json = json.dumps(bundle_obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    gz_bytes = gzip.compress(raw_json, compresslevel=9, mtime=0)

    bundle_path = out_dir / args.bundle_name
    bundle_path.write_bytes(gz_bytes)

    meta_obj = {
        "version": 1,
        "repo": args.repo_name,
        "sha": sha,
        "short_sha": sha[:12],
        "built_at": built_at,
        "file_count": len(files),
        "bytes_raw": len(raw_json),
        "bytes_gzip": len(gz_bytes),
    }
    if args.meta_name:
        meta_path = out_dir / args.meta_name
        meta_path.write_text(json.dumps(meta_obj, indent=2) + "\n", encoding="utf-8")

    print(
        f"[bundle] {args.repo_name}@{sha[:12]}: {len(files)} files, "
        f"{len(raw_json):,} raw bytes -> {len(gz_bytes):,} gzip bytes -> {bundle_path}"
    )
    if skipped:
        print(f"[bundle] skipped {len(skipped)} file(s):")
        for item in skipped:
            print(f"  - {item}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
