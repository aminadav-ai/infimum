# Infimum Security Threat Model

> **Scope:** Infimum Language compiler & Z3 verifier (`crates/infimum-lang`), `.ifw` WebAssembly capsule artifacts + Passport v2 (`blake3` + `Ed25519`), and the sandboxed capsule runtimes (`crates/infimum-web` and `crates/infimum-rt`).

---

## 1. Trust Boundaries

```text
+---------------------------+
|  Untrusted Code Author    |  (LLMs, third-party developers, external PRs)
|  (.ifm source code)       |
+-------------+-------------+
              |  [Boundary 1: Parse + Typecheck + Taint + Z3 SMT (fail-closed)]
              v
+-------------+-------------+
|  Trusted Verifier / Build |  (`infimum-lang` + Z3 SMT solver + Ed25519 SigningKey)
|  -> .ifw + Passport v2    |
+-------------+-------------+
              |  [Boundary 2: Tamper-Evident Artifact (.ifw)]
              |  - `wasm_code_hash` = blake3(non-custom Wasm sections)
              |  - Optional Ed25519 `"infimum.signature"` over passport JSON
              |  - Duplicate custom section rejection (anti-section-smuggling)
              v
+-------------+-------------+
|  Trusted Capsule Runtime  |  (`infimum-web` / `infimum-rt`)
|  - Pre-exec Gatekeeper    |  - Verifies Passport v2 + Ed25519 BEFORE byte 0 executes
|  - wasmi Fuel Sandbox     |  - Hard per-frame / per-request instruction fuel cap
|  - Linear Memory Cap      |  - Fixed power-of-two memory (512 KiB .. 64 MiB), no malloc
|  - CapabilityPolicy       |  - Explicit allowlists for Net domains, FS roots, Clipboard
|  - Secret<T> Vault        |  - Secrets never leak to logs, disk, or unpinned domains
+---------------------------+
```

---

## 2. Threat Matrix & Mitigations

| # | Threat | Attack Vector | Mitigation in Infimum |
| :--- | :--- | :--- | :--- |
| **T1** | **Remote Code Execution (RCE) / Sandbox Escape** | Malicious `.ifm` or `.ifw` tries to access OS syscalls, filesystem, shell, or host memory. | Capsules execute strictly inside `wasmi` Wasm 1.0 linear memory with **zero** WASI imports. Only `env.host_call` exists, gated by `CapabilityPolicy` and `infimum.passport` before instantiation. |
| **T2** | **Bytecode Tampering / Supply-Chain Swap** | Attacker modifies 1 byte of Wasm `Code`/`Data`/`Import` section in a compiled `.ifw` while keeping a benign `"infimum.passport"`. | **Passport v2** embeds `wasm_code_hash = blake3(magic + version + all non-custom sections)`. Gatekeeper recomputes the hash before instantiation and rejects any mismatch (`E084_PASSPORT_WASM_HASH_MISMATCH`). |
| **T3** | **Passport Forgery / Capability Hiding** | Attacker edits `"infimum.passport"` JSON to remove `Net(...)` or flip `is_pure: true`, or hides effects in a secondary `export fn`. | 1. Compiler merges capabilities across **all** `export fn` entrypoints (multi-export passport).<br>2. Optional **Ed25519** signature (`"infimum.signature"`) over `blake3("infimum-passport-v2\0" \|\| passport_json)` enforced via `--require-signature <pubkey>` (`E085_PASSPORT_SIGNATURE_INVALID`). |
| **T4** | **Wasm Section Smuggling** | Attacker appends a second `"infimum.passport"` or `"infimum.signature"` custom section so parser A reads section 1 and parser B reads section 2. | Both `infimum-lang` (`passport_crypto.rs`) and `infimum-web` (`passport.rs`) scan the **entire** Wasm file and reject any duplicate `"infimum.passport"` or `"infimum.signature"` section (`E086_PASSPORT_DUPLICATE_SECTION`). |
| **T5** | **SSRF / CRLF / Userinfo Injection** | Capsule with `Net("api.example.com")` tries `https://api.example.com@evil.com`, `127.0.0.1:other_port`, or header CRLF injection. | Host `capabilities.rs` enforces exact host + port allowlist matching, strips/blocks userinfo (`@`), and rejects `\r`, `\n`, `\0` in headers and URLs. |
| **T6** | **Secret Exfiltration (`Secret<T>`)** | LLM-generated code attempts to log, return, store, or branch-leak an API key / token (`Secret<T>`). | Compile-time taint checker (`typecheck.rs`) forbids `Secret<T>` in return values, comparisons, disk/log sinks, and unpinned network calls; only explicit `reveal` into an effect-pinned `Net("domain")` sink is permitted and recorded in `passport.secret_pipes`. |
| **T7** | **Infinite Loops / CPU & RAM DoS** | Adversarial capsule loops forever or allocates unbounded memory. | 1. Language requires static `max N` loop bounds verified by Z3.<br>2. `wasmi` enforces a hard instruction fuel budget (`FRAME_FUEL_BUDGET`).<br>3. Wasm memory has a fixed static ceiling (`512 KiB .. 64 MiB`) declared in `passport.max_memory_bytes`. |
| **T8** | **Z3 Solver Timeout / Unsound Proof Bypass** | Crafted formula causes Z3 to return `unknown` or time out, attempting to sneak unverified code past compilation. | **Fail-closed:** any `unknown`, timeout, or solver error aborts verification and blocks `.ifw` emission. |
| **T9** | **Vulnerable / Unlicensed Dependencies** | Supply-chain compromise in Rust crates. | CI enforces `cargo audit` (RustSec advisory DB) and `cargo deny check licenses advisories bans` via `deny.toml`. |

---

## 3. Out of Scope (Non-Goals)

- Physical side-channels (power analysis, EM, microarchitectural Spectre/Meltdown against the host CPU).
- Compromise of the host OS kernel, Rust compiler, or Z3 binary itself.
- Confidentiality of Ed25519 private signing keys if stored outside an HSM/KMS (operators should protect `infimum_signing.key` with `0600` permissions or enterprise KMS).
