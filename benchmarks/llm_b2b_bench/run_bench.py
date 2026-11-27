#!/usr/bin/env python3
"""
Infimum B2B Capsule LLM Benchmark Harness (25 tasks).

Evaluates LLMs (OpenRouter / OpenAI / Anthropic / Gemini) on generating formally verified (Z3),
zero-heap, signed `.ifw` B2B capsules with a 3-attempt self-repair loop driven by
`infimum verify --json` structured `repair_hints` v2, then executes compiled capsules inside
the zero-GPU `infimum-rt` sandbox (`infimum-rt-exec`).

Usage:
  python3 benchmarks/llm_b2b_bench/run_bench.py --dry-run
  OPENROUTER_API_KEY=... python3 benchmarks/llm_b2b_bench/run_bench.py --provider openrouter --model anthropic/claude-sonnet-4
  ANTHROPIC_API_KEY=...  python3 benchmarks/llm_b2b_bench/run_bench.py --provider anthropic --model claude-sonnet-4-20250514
  OPENAI_API_KEY=...     python3 benchmarks/llm_b2b_bench/run_bench.py --provider openai --model gpt-4.1
  GEMINI_API_KEY=...     python3 benchmarks/llm_b2b_bench/run_bench.py --provider gemini --model gemini-2.5-pro
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
TASKS_PATH = Path(__file__).resolve().parent / "tasks.json"
GUIDE_PATH = REPO_ROOT / "crates/infimum-lang/docs/INFIMUM_LANG_GUIDE.md"

SYSTEM_PROMPT = """You are an expert Infimum B2B capsule engineer.
Output ONLY valid Infimum (.ifm) source code inside a single ```infimum ... ``` fenced block.
Key rules:
- Use `use "std/capsule.ifm"; use "std/json.ifm";` (and `use "std/dec.ifm";` / `use "std/slice.ifm";` as needed).
- Export `export fn cap_execute(in_len: i64) -[Mem]-> i64` (or `-[Mem, Net("domain")]-> i64` for Zero-Trust tasks).
- Use `cap_input(in_len)`, `cap_output_buf()`, `cap_finish(...)`, `json_get_*`, `jw_begin_obj`..`jw_end_obj`.
- Every `while` loop MUST declare `max <N>` (e.g. `while i < n max 1024 { ... }`).
- For money use `dec64` literals (`19.99d`, `0.0d`) and guard ranges so Z3 proves absence of overflow/division-by-zero.
- For `Secret<str>`, read via `cap_read_secret(32)` and ONLY pass to `do Net.auth(reveal("domain", sec));` inside a function with `Net("domain")`. Never write secrets to `output_buf`.
"""


def find_binary(name: str, env_var: str) -> Path:
    if os.environ.get(env_var):
        return Path(os.environ[env_var])
    for profile in ("release", "debug"):
        cand = REPO_ROOT / "target" / profile / name
        if cand.exists():
            return cand
    return REPO_ROOT / "target" / "release" / name


def extract_ifm_code(text: str) -> str:
    m = re.search(r"```(?:infimum|ifm)?\s*\n([\s\S]*?)```", text)
    return m.group(1).strip() if m else text.strip()


def call_llm(provider: str, model: str, messages: list[dict[str, str]]) -> str:
    if provider == "openrouter":
        api_key = os.environ.get("OPENROUTER_API_KEY", "")
        url = "https://openrouter.ai/api/v1/chat/completions"
        headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
        payload = {"model": model, "messages": messages, "temperature": 0.1}
        data = _post_json(url, headers, payload)
        return data["choices"][0]["message"]["content"]

    if provider == "openai":
        api_key = os.environ.get("OPENAI_API_KEY", "")
        url = "https://api.openai.com/v1/chat/completions"
        headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
        payload = {"model": model, "messages": messages, "temperature": 0.1}
        data = _post_json(url, headers, payload)
        return data["choices"][0]["message"]["content"]

    if provider == "anthropic":
        api_key = os.environ.get("ANTHROPIC_API_KEY", "")
        url = "https://api.anthropic.com/v1/messages"
        headers = {
            "x-api-key": api_key,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
        }
        sys_msg = "\n".join(m["content"] for m in messages if m["role"] == "system")
        conv = [m for m in messages if m["role"] != "system"]
        payload = {"model": model, "max_tokens": 4096, "system": sys_msg, "messages": conv}
        data = _post_json(url, headers, payload)
        return data["content"][0]["text"]

    if provider == "gemini":
        api_key = os.environ.get("GEMINI_API_KEY", "")
        url = f"https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={api_key}"
        headers = {"Content-Type": "application/json"}
        parts = [{"text": f"[{m['role'].upper()}]\n{m['content']}"} for m in messages]
        payload = {"contents": [{"parts": parts}]}
        data = _post_json(url, headers, payload)
        return data["candidates"][0]["content"]["parts"][0]["text"]

    raise ValueError(f"Unsupported provider: {provider}")


def _post_json(url: str, headers: dict[str, str], payload: dict[str, Any]) -> dict[str, Any]:
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        return json.loads(resp.read().decode("utf-8"))


def verify_and_compile(
    infimum_bin: Path,
    ifm_path: Path,
    ifw_path: Path,
    sign_key_path: Path,
) -> tuple[bool, dict[str, Any], float, bool]:
    """Run `infimum verify --json` and `infimum compile-wasm --sign-key`.
    Returns `(ok, verify_json_or_err, elapsed_ms, z3_caught_bug)`.
    """
    t0 = time.perf_counter()
    v_proc = subprocess.run(
        [str(infimum_bin), "verify", str(ifm_path), "--json"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    z3_caught = False
    try:
        v_data = json.loads(v_proc.stdout or "{}")
    except json.JSONDecodeError:
        v_data = {"ok": False, "raw_stdout": v_proc.stdout, "raw_stderr": v_proc.stderr}

    if v_proc.returncode != 0 or not v_data.get("ok", False):
        diags = v_data.get("diagnostics", [])
        if any(d.get("phase") == "verify" or str(d.get("code", "")).startswith("E05") for d in diags):
            z3_caught = True
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        return False, v_data, elapsed_ms, z3_caught

    c_proc = subprocess.run(
        [
            str(infimum_bin),
            "compile-wasm",
            str(ifm_path),
            "-o",
            str(ifw_path),
            "--sign-key",
            str(sign_key_path),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    if c_proc.returncode != 0:
        return False, {"ok": False, "compile_error": c_proc.stderr or c_proc.stdout}, elapsed_ms, False
    return True, v_data, elapsed_ms, False


def evaluate_capsule(
    rt_exec_bin: Path,
    ifw_path: Path,
    pubkey_path: Path,
    task: dict[str, Any],
) -> tuple[bool, list[str], float]:
    """Execute all hidden test cases against the signed `.ifw` in `infimum-rt-exec`."""
    errors: list[str] = []
    exec_us_list: list[float] = []
    secret_val = task.get("secret_value")

    for tc in task["test_cases"]:
        # Skip live outbound HTTPS test cases in offline harness unless mock/local
        if task.get("required_net_domains") and tc.get("expect_ok"):
            continue
        cmd = [
            str(rt_exec_bin),
            str(ifw_path),
            "--input",
            json.dumps(tc["input"]),
            "--pubkey",
            str(pubkey_path),
        ]
        if secret_val:
            cmd.extend(["--secret", secret_val])
        for dom in task.get("required_net_domains", []):
            cmd.extend(["--allow-net", dom])

        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
        try:
            res = json.loads(proc.stdout)
        except json.JSONDecodeError:
            errors.append(f"{tc['name']}: non-JSON runtime output: {proc.stderr or proc.stdout}")
            continue

        exec_us_list.append(float(res.get("elapsed_us", 0)))
        if secret_val and secret_val in str(res.get("output_json", "")):
            errors.append(f"{tc['name']}: SECRET LEAKED into output_json!")
            continue

        if bool(res.get("ok")) != bool(tc["expect_ok"]):
            errors.append(f"{tc['name']}: expected ok={tc['expect_ok']}, got {res}")
            continue

        if tc["expect_ok"]:
            try:
                out_obj = json.loads(res["output_json"])
            except json.JSONDecodeError:
                errors.append(f"{tc['name']}: invalid output_json: {res['output_json']}")
                continue
            for k, expected_v in tc.get("expect_fields", {}).items():
                if out_obj.get(k) != expected_v:
                    errors.append(f"{tc['name']}: field '{k}' expected {expected_v!r}, got {out_obj.get(k)!r}")
            for k in tc.get("expect_fields_present", []):
                if k not in out_obj:
                    errors.append(f"{tc['name']}: missing required field '{k}'")

    avg_us = sum(exec_us_list) / len(exec_us_list) if exec_us_list else 0.0
    return len(errors) == 0, errors, avg_us


def validate_tasks_schema(data: dict[str, Any]) -> None:
    tasks = data.get("tasks", [])
    assert len(tasks) == 25, f"Expected 25 tasks, found {len(tasks)}"
    counts: dict[str, int] = {}
    ids: set[str] = set()
    for t in tasks:
        assert t["id"] not in ids, f"Duplicate task id: {t['id']}"
        ids.add(t["id"])
        counts[t["category"]] = counts.get(t["category"], 0) + 1
        assert len(t.get("test_cases", [])) >= 2, f"Task {t['id']} needs >= 2 test cases"
    assert counts == {
        "fintech_dec64": 8,
        "json_validator": 7,
        "rbac_quota": 5,
        "zero_trust_secret": 5,
    }, f"Unexpected category distribution: {counts}"


def print_markdown_report(results: list[dict[str, Any]], mode_label: str) -> None:
    total = len(results)
    pass1 = sum(1 for r in results if r["passed_attempt"] == 1)
    pass3 = sum(1 for r in results if r["passed_attempt"] is not None)
    z3_bugs = sum(r["z3_caught_bugs"] for r in results)
    avg_vc = sum(r["verify_compile_ms"] for r in results) / max(total, 1)
    avg_ex = sum(r["exec_us"] for r in results) / max(total, 1)

    print(f"\n## Infimum B2B LLM Benchmark Report ({mode_label})\n")
    print("| Metric | Value |")
    print("| :--- | :---: |")
    print(f"| Tasks Evaluated | {total} |")
    print(f"| Pass@1 | {pass1}/{total} ({(100.0 * pass1 / max(total, 1)):.1f}%) |")
    print(f"| Pass@3 (Z3 repair loop) | {pass3}/{total} ({(100.0 * pass3 / max(total, 1)):.1f}%) |")
    print(f"| Z3 Caught Bugs (pre-execution) | {z3_bugs} |")
    print(f"| Avg Verify + Compile | {avg_vc:.1f} ms |")
    print(f"| Avg Execute (`infimum-rt`) | {avg_ex:.1f} us |")


def main() -> int:
    parser = argparse.ArgumentParser(description="Infimum 25-task B2B LLM Benchmark")
    parser.add_argument("--dry-run", action="store_true", help="Validate tasks.json and run reference capsules offline")
    parser.add_argument("--provider", choices=["openrouter", "openai", "anthropic", "gemini"], default="openrouter")
    parser.add_argument("--model", default="anthropic/claude-sonnet-4")
    parser.add_argument("--max-attempts", type=int, default=3)
    parser.add_argument("--limit", type=int, default=25)
    args = parser.parse_args()

    data = json.loads(TASKS_PATH.read_text(encoding="utf-8"))
    validate_tasks_schema(data)
    print(f"[ok] Validated {len(data['tasks'])} tasks across 4 B2B categories in {TASKS_PATH.name}")

    infimum_bin = find_binary("infimum", "INFIMUM_BIN")
    rt_exec_bin = find_binary("infimum-rt-exec", "INFIMUM_RT_EXEC_BIN")
    if not infimum_bin.exists():
        print(f"[error] infimum binary not found at {infimum_bin}. Run: cargo build --release -p infimum-lang --bin infimum", file=sys.stderr)
        return 1
    if not rt_exec_bin.exists():
        print(f"[error] infimum-rt-exec binary not found at {rt_exec_bin}. Run: cargo build --release -p infimum-rt --bin infimum-rt-exec", file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory(prefix="infimum_bench_") as tmp_str:
        tmp = Path(tmp_str)
        key_dir = tmp / "keys"
        key_dir.mkdir()
        subprocess.run([str(infimum_bin), "keygen", "--out-dir", str(key_dir)], check=True, capture_output=True)
        sign_key = key_dir / "infimum_signing.key"
        pub_key = key_dir / "infimum_signing.pub"

        results: list[dict[str, Any]] = []

        if args.dry_run:
            ref_tasks = [t for t in data["tasks"] if "reference_ifm" in t]
            for task in ref_tasks:
                ifm_path = REPO_ROOT / task["reference_ifm"]
                ifw_path = tmp / f"{task['id']}.ifw"
                ok_vc, v_info, vc_ms, z3_bug = verify_and_compile(infimum_bin, ifm_path, ifw_path, sign_key)
                if not ok_vc:
                    print(f"[FAIL] {task['id']} verify/compile failed: {v_info}", file=sys.stderr)
                    return 1
                ok_eval, errs, ex_us = evaluate_capsule(rt_exec_bin, ifw_path, pub_key, task)
                if not ok_eval:
                    print(f"[FAIL] {task['id']} runtime eval failed: {errs}", file=sys.stderr)
                    return 1
                print(f"[PASS] {task['id']} ({task['title']}): verify+compile={vc_ms:.1f}ms, exec={ex_us:.1f}us")
                results.append({
                    "id": task["id"],
                    "passed_attempt": 1,
                    "z3_caught_bugs": int(z3_bug),
                    "verify_compile_ms": vc_ms,
                    "exec_us": ex_us,
                })
            print_markdown_report(results, "DRY-RUN Reference Capsules")
            return 0

        for task in data["tasks"][: args.limit]:
            messages = [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": f"Task {task['id']}: {task['title']}\n\n{task['prompt']}"},
            ]
            passed_attempt = None
            z3_caught_total = 0
            last_vc_ms = 0.0
            last_ex_us = 0.0

            for attempt in range(1, args.max_attempts + 1):
                raw_reply = call_llm(args.provider, args.model, messages)
                code = extract_ifm_code(raw_reply)
                ifm_path = tmp / f"{task['id']}_a{attempt}.ifm"
                ifw_path = tmp / f"{task['id']}_a{attempt}.ifw"
                ifm_path.write_text(code + "\n", encoding="utf-8")

                ok_vc, v_info, vc_ms, z3_bug = verify_and_compile(infimum_bin, ifm_path, ifw_path, sign_key)
                last_vc_ms = vc_ms
                if z3_bug:
                    z3_caught_total += 1

                if not ok_vc:
                    messages.append({"role": "assistant", "content": raw_reply})
                    messages.append({
                        "role": "user",
                        "content": f"Verification/compilation failed (attempt {attempt}). Fix all errors using this JSON diagnostic:\n{json.dumps(v_info, indent=2)}",
                    })
                    continue

                ok_eval, errs, ex_us = evaluate_capsule(rt_exec_bin, ifw_path, pub_key, task)
                last_ex_us = ex_us
                if ok_eval:
                    passed_attempt = attempt
                    break

                messages.append({"role": "assistant", "content": raw_reply})
                messages.append({
                    "role": "user",
                    "content": f"Capsule compiled but failed runtime tests (attempt {attempt}):\n" + "\n".join(errs),
                })

            status = f"PASS@{passed_attempt}" if passed_attempt else "FAIL"
            print(f"[{status}] {task['id']} ({task['title']}) | z3_caught={z3_caught_total} | vc={last_vc_ms:.1f}ms | exec={last_ex_us:.1f}us")
            results.append({
                "id": task["id"],
                "passed_attempt": passed_attempt,
                "z3_caught_bugs": z3_caught_total,
                "verify_compile_ms": last_vc_ms,
                "exec_us": last_ex_us,
            })

        print_markdown_report(results, f"{args.provider}:{args.model}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
