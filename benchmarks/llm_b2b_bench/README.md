# Infimum B2B Capsule LLM Benchmark (25 Tasks)

Reproducible public benchmark measuring how accurately LLMs synthesize **formally verified (Z3)**, **zero-heap**, **Ed25519-signed** `.ifw` B2B capsules executed inside the zero-GPU [`infimum-rt`](../../crates/infimum-rt/) sandbox.

## Task Suite (`tasks.json`)

| Category | Count | Focus |
| :--- | :---: | :--- |
| `fintech_dec64` | 8 | Exact decimal money (`dec64`), taxes, FX spread, tiered fees, bill splitting, VAT |
| `json_validator` | 7 | Zero-copy UTF-8 JSON parsing (`std/json.ifm` v2), nested objects, arrays, safe escaping `JsonWriter` |
| `rbac_quota` | 5 | RBAC permissions, daily quotas, multi-tier wire approvals, token buckets, compliance gates |
| `zero_trust_secret` | 5 | Vault `Secret<str>` taint tracking + domain-pinned `Net("...")` + zero-leak verification |
| **Total** | **25** | Includes normal and adversarial/out-of-bounds test cases |

## Quick Start

1. Build the compiler and zero-GPU runtime CLI:

```bash
cargo build --release -p infimum-lang --bin infimum
cargo build --release -p infimum-rt --bin infimum-rt-exec
```

2. Validate the benchmark schema and run the 3 reference B2B capsules offline (`--dry-run`):

```bash
python3 benchmarks/llm_b2b_bench/run_bench.py --dry-run
```

3. Run against any LLM provider (3-attempt self-repair loop using `infimum verify --json` structured `repair_hints` v2):

```bash
# OpenRouter
OPENROUTER_API_KEY=sk-or-... python3 benchmarks/llm_b2b_bench/run_bench.py \
  --provider openrouter --model anthropic/claude-sonnet-4

# Anthropic
ANTHROPIC_API_KEY=sk-ant-... python3 benchmarks/llm_b2b_bench/run_bench.py \
  --provider anthropic --model claude-sonnet-4-20250514

# OpenAI
OPENAI_API_KEY=sk-... python3 benchmarks/llm_b2b_bench/run_bench.py \
  --provider openai --model gpt-4.1

# Google Gemini
GEMINI_API_KEY=... python3 benchmarks/llm_b2b_bench/run_bench.py \
  --provider gemini --model gemini-2.5-pro
```

## Reported Metrics

- **Pass@1**: First-shot Z3 verification + Ed25519 compilation + all hidden tests passing.
- **Pass@3**: Passing within 3 attempts using `infimum verify --json` structured `repair_hints` v2.
- **Z3 Caught Bugs**: Number of overflow / division-by-zero / precondition / bounds bugs caught statically by Z3 before execution.
- **Avg Verify + Compile (ms)**: End-to-end Z3 proof + direct Wasm `.ifw` compilation latency.
- **Avg Execute (us)**: Sub-millisecond sandboxed execution latency inside `infimum-rt`.
