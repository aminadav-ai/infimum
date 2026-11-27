#![deny(clippy::todo, clippy::unimplemented, dead_code, unused_must_use)]

use infimum_lang::abi::{generate_js_abi, generate_json_abi, generate_rust_abi};
use infimum_lang::diag::{InfimumDiagnostic, Span};
use infimum_lang::parser::parse_module_from_path;
use infimum_lang::passport_crypto::{
    compute_key_id, decode_key_32, encode_hex, generate_keypair, verify_passport_signature,
};
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::{
    build_strata_wasm_from_module_signed, build_strata_wasm_target,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let json_mode = args.iter().any(|a| a == "--json");
    let no_cache = args.iter().any(|a| a == "--no-cache");
    let pos_args: Vec<&str> = args
        .iter()
        .filter(|a| a.as_str() != "--json" && a.as_str() != "--no-cache")
        .map(String::as_str)
        .collect();

    match run_cli(&pos_args, json_mode, no_cache) {
        Ok(()) => ExitCode::SUCCESS,
        Err(diag) => {
            if json_mode {
                let env = infimum_lang::diag::JsonDiagnosticEnvelope {
                    ok: false,
                    phase: diag.phase.clone(),
                    diagnostics: vec![diag],
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&env).unwrap_or_default()
                );
            } else {
                eprintln!("error {}", diag);
            }
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: &[&str], json_mode: bool, no_cache: bool) -> Result<(), InfimumDiagnostic> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let use_cache = !no_cache;

    match args {
        ["mcp"] | ["--mcp"] => {
            infimum_lang::mcp::serve_stdio();
            Ok(())
        }
        ["test", file_path] => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let results = infimum_lang::eval::run_module_tests(&module);
            let passed = results.iter().filter(|r| r.passed).count();
            let failed = results.len() - passed;
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": failed == 0,
                        "passed": passed,
                        "failed": failed,
                        "results": results,
                    }))
                    .unwrap_or_default()
                );
            } else if results.is_empty() {
                println!("No test blocks found in {file_path}");
            } else {
                for r in &results {
                    if r.passed {
                        println!("  PASS  {}", r.name);
                    } else {
                        println!(
                            "  FAIL  {} ({})",
                            r.name,
                            r.error.as_deref().unwrap_or("assertion failed")
                        );
                    }
                }
                println!("\nTest result: {passed} passed, {failed} failed.");
            }
            if failed > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        ["verify", file_path] => {
            let src_text = fs::read_to_string(file_path).ok();
            let enrich_line = |mut d: InfimumDiagnostic| -> InfimumDiagnostic {
                if d.file.is_none() {
                    d = d.with_file(*file_path);
                }
                if d.source_line.is_none() {
                    if let Some(ref txt) = src_text {
                        if let Some(line_txt) =
                            infimum_lang::diag::extract_source_line(txt, d.span.line)
                        {
                            d = d.with_source_line(line_txt);
                        }
                    }
                }
                d
            };
            let module = parse_module_from_path(Path::new(file_path)).map_err(enrich_line)?;
            let store = GraphStore::open(&cwd)?;
            if json_mode {
                match store.verify_module_collect_all(&module, use_cache, 8) {
                    Ok(reports) => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "ok": true,
                                "phase": "verify",
                                "diagnostics": [],
                                "reports": reports,
                            }))
                            .unwrap_or_default()
                        );
                    }
                    Err(errs) => {
                        let diagnostics: Vec<InfimumDiagnostic> =
                            errs.into_iter().map(enrich_line).collect();
                        let env = infimum_lang::diag::JsonDiagnosticEnvelope {
                            ok: false,
                            phase: "verify".to_string(),
                            diagnostics,
                        };
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&env).unwrap_or_default()
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                let reports = store
                    .verify_module_with_cache(&module, use_cache)
                    .map_err(enrich_line)?;
                for r in reports {
                    println!(
                        "  #{}  {:<26} [Z3: {} in {}ms]",
                        r.short_hash, r.alias, r.status, r.elapsed_ms
                    );
                }
            }
            Ok(())
        }
        ["ingest", file_path] => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let mut store = GraphStore::open(&cwd)?;
            let entries = store.ingest_module_with_cache(&module, use_cache)?;

            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entries).unwrap_or_default()
                );
            } else {
                for item in entries {
                    let storage_tag = if item.deduplicated {
                        "dedup (identical semantics)"
                    } else {
                        "stored"
                    };
                    println!(
                        "  #{}  {:<26} [Z3 proved {} oblig. ({}ms) | {}]",
                        item.short_hash,
                        item.alias,
                        item.obligations_proved,
                        item.verify_ms,
                        storage_tag
                    );
                }
            }
            Ok(())
        }
        ["compile-wasm", file_path, "-o", out_path] => {
            run_compile_wasm(&cwd, file_path, out_path, None, use_cache, json_mode)
        }
        ["compile-wasm", file_path, "-o", out_path, "--sign-key", key_path] => {
            run_compile_wasm(
                &cwd,
                file_path,
                out_path,
                Some(Path::new(key_path)),
                use_cache,
                json_mode,
            )
        }
        ["keygen", "--out-dir", out_dir] => {
            let dir = Path::new(out_dir);
            fs::create_dir_all(dir).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_KEYGEN_IO",
                    "keygen",
                    format!("Failed to create key directory '{}': {e}", dir.display()),
                    Span::zero(),
                    "Check write permissions on target directory",
                )
            })?;
            let (sk, pk) = generate_keypair();
            let key_id = compute_key_id(&pk);
            let sk_path = dir.join("infimum_signing.key");
            let pk_path = dir.join("infimum_signing.pub");
            fs::write(&sk_path, format!("{}\n", encode_hex(&sk))).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_KEYGEN_IO",
                    "keygen",
                    format!("Failed to write secret key '{}': {e}", sk_path.display()),
                    Span::zero(),
                    "Check write permissions on target directory",
                )
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&sk_path, fs::Permissions::from_mode(0o600));
            }
            fs::write(&pk_path, format!("{}\n", encode_hex(&pk))).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_KEYGEN_IO",
                    "keygen",
                    format!("Failed to write public key '{}': {e}", pk_path.display()),
                    Span::zero(),
                    "Check write permissions on target directory",
                )
            })?;
            if json_mode {
                println!(
                    "{{\"alg\":\"Ed25519\",\"key_id\":\"{}\",\"secret_key_path\":\"{}\",\"public_key_path\":\"{}\",\"public_key\":\"{}\"}}",
                    key_id,
                    sk_path.display(),
                    pk_path.display(),
                    encode_hex(&pk)
                );
            } else {
                println!("Generated Ed25519 signing keypair (key_id={key_id}):");
                println!("  Secret key (0600): {}", sk_path.display());
                println!("  Public key:        {}", pk_path.display());
            }
            Ok(())
        }
        ["verify-passport", ifw_path] => run_verify_passport(Path::new(ifw_path), None, json_mode),
        ["verify-passport", ifw_path, "--pubkey", pub_path] => {
            run_verify_passport(Path::new(ifw_path), Some(Path::new(pub_path)), json_mode)
        }
        ["export-abi", file_path, "--rust", "-o", out_path] => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let rust_src = generate_rust_abi(&module)?;
            write_abi_output(Path::new(out_path), &rust_src)?;
            if json_mode {
                println!(
                    "{{\"format\":\"rust\",\"out\":\"{}\",\"consts\":{},\"enums\":{},\"layouts\":{}}}",
                    out_path,
                    module.consts.len(),
                    module.enums.len(),
                    module.layouts.len()
                );
            } else {
                println!(
                    "  Exported Rust ABI -> {} ({} const(s), {} enum(s), {} layout(s))",
                    out_path,
                    module.consts.len(),
                    module.enums.len(),
                    module.layouts.len()
                );
            }
            Ok(())
        }
        ["export-abi", file_path, "--rust"] => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let rust_src = generate_rust_abi(&module)?;
            print!("{rust_src}");
            Ok(())
        }
        ["export-abi", file_path, "--js", "-o", out_path] => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let js_src = generate_js_abi(&module)?;
            write_abi_output(Path::new(out_path), &js_src)?;
            if json_mode {
                println!(
                    "{{\"format\":\"js\",\"out\":\"{}\",\"consts\":{},\"enums\":{},\"layouts\":{}}}",
                    out_path,
                    module.consts.len(),
                    module.enums.len(),
                    module.layouts.len()
                );
            } else {
                println!(
                    "  Exported JS ABI -> {} ({} const(s), {} enum(s), {} layout(s))",
                    out_path,
                    module.consts.len(),
                    module.enums.len(),
                    module.layouts.len()
                );
            }
            Ok(())
        }
        ["export-abi", file_path, "--js"] => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let js_src = generate_js_abi(&module)?;
            print!("{js_src}");
            Ok(())
        }
        ["export-abi", file_path, "-o", out_path] if json_mode => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let json_src = generate_json_abi(&module)?;
            write_abi_output(Path::new(out_path), &json_src)?;
            Ok(())
        }
        ["export-abi", file_path] if json_mode => {
            let module = parse_module_from_path(Path::new(file_path))?;
            let json_src = generate_json_abi(&module)?;
            println!("{json_src}");
            Ok(())
        }
        ["build-strata-wasm", target, "-o", out_path] => {
            let store = GraphStore::open(&cwd)?;
            let art = build_strata_wasm_target(&store, target, Path::new(out_path))?;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&art).unwrap_or_default());
            } else {
                println!(
                    "  Strata Direct Wasm -> {} [{} node(s), {} bytes ({:.2} KB) in {:.2}ms, zero rustc]",
                    art.wasm_path,
                    art.compiled_nodes,
                    art.wasm_size_bytes,
                    art.wasm_size_bytes as f64 / 1024.0,
                    art.build_us as f64 / 1000.0
                );
            }
            Ok(())
        }
        ["show", target] => {
            let store = GraphStore::open(&cwd)?;
            let view = store.show_node(target)?;
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).unwrap_or_default()
                );
            } else {
                println!("Node:      #{}", view.node.hash);
                println!("Aliases:   {}", view.aliases.join(", "));
                println!("Canonical: {}", view.node.canonical_source);
                if !view.neighbor_signatures.is_empty() {
                    println!("Direct Neighbors (minimal AI context):");
                    for n in view.neighbor_signatures {
                        let eff = if n.effects.is_empty() {
                            "->".to_string()
                        } else {
                            format!("-[{}]->", n.effects.join(", "))
                        };
                        println!(
                            "  #{} ({}) ({}) {} {}",
                            n.short_hash,
                            n.aliases.join("/"),
                            n.params.join(", "),
                            eff,
                            n.ret_type
                        );
                    }
                }
            }
            Ok(())
        }
        ["graph"] => {
            let store = GraphStore::open(&cwd)?;
            let nodes = store.list_graph()?;
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&nodes).unwrap_or_default()
                );
            } else if nodes.is_empty() {
                println!("Graph is empty (.infimum/nodes has 0 nodes).");
            } else {
                println!("Infimum Content-Addressed Graph ({} nodes):", nodes.len());
                for n in nodes {
                    let eff = if n.direct_effects.is_empty() {
                        "pure".to_string()
                    } else {
                        n.direct_effects.join(", ")
                    };
                    let deps: Vec<String> =
                        n.direct_deps.iter().map(|d| format!("#{}", &d[..12])).collect();
                    println!(
                        "  #{}  aliases=[{}]  effects=[{}]  calls=[{}]",
                        n.short_hash,
                        n.aliases.join(", "),
                        eff,
                        deps.join(", ")
                    );
                }
            }
            Ok(())
        }
        ["passport", target] => {
            let store = GraphStore::open(&cwd)?;
            let p = store.compute_passport(target)?;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&p).unwrap_or_default());
            } else {
                println!("=== INFIMUM SECURITY PASSPORT ===");
                println!(
                    "Node:                 #{} ({})",
                    p.short_hash,
                    p.aliases.join(", ")
                );
                println!("Reachable Subgraph:   {} node(s)", p.total_reachable_nodes);
                println!(
                    "Touches Secret<T>:    {}",
                    if p.uses_secret_types { "YES" } else { "NO" }
                );
                if p.is_pure {
                    println!(
                        "Capabilities:         PURE (zero network, filesystem, or clock access)"
                    );
                } else {
                    println!("Capabilities:");
                    for cap in p.capabilities {
                        println!(
                            "  [CAP]  {:<20} via: {}",
                            cap.effect,
                            cap.witness_chain.join(" -> ")
                        );
                    }
                }
                if p.uses_secret_types {
                    println!("Secret Data-Flow Pipes:");
                    if p.secret_pipes.is_empty() {
                        println!("  [SAFE] Secret<T> stays internal (0 external network sinks)");
                    } else {
                        for pipe in p.secret_pipes {
                            println!(
                                "  [PIPE] {} -> {} (\"{}\")  via: {}",
                                pipe.secret_type,
                                pipe.operation,
                                pipe.destination_domain,
                                pipe.witness_chain.join(" -> ")
                            );
                        }
                    }
                    println!("  [SAFE] Disk / Log / Unpinned Leaks: 0 (verified by taint checker)");
                }
            }
            Ok(())
        }
        ["rename", old_alias, new_alias] => {
            let mut store = GraphStore::open(&cwd)?;
            let hash = store.rename_alias(old_alias, new_alias)?;
            if json_mode {
                println!(
                    "{{\"old\":\"{old_alias}\",\"new\":\"{new_alias}\",\"hash\":\"{hash}\"}}"
                );
            } else {
                println!(
                    "Renamed alias '{old_alias}' -> '{new_alias}' (node #{} unchanged)",
                    &hash[..12]
                );
            }
            Ok(())
        }
        _ => Err(InfimumDiagnostic::new(
            "E041_CLI_USAGE",
            "cli",
            "Invalid CLI arguments",
            Span::zero(),
            "Usage: infimum <mcp | verify <f.ifm> | test <f.ifm> | ingest <f.ifm> | compile-wasm <f.ifm> -o <out.ifw> [--sign-key <key>] | export-abi <f.ifm> --rust|--js [-o <out>] | build-strata-wasm <ref> -o <out.ifw> | passport <ref> | verify-passport <f.ifw> [--pubkey <pub>] | keygen --out-dir <dir> | show <ref> | graph | rename <old> <new>> [--json] [--no-cache]",
        )),
    }
}

fn write_abi_output(out_path: &Path, content: &str) -> Result<(), InfimumDiagnostic> {
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    fs::write(out_path, content).map_err(|e| {
        InfimumDiagnostic::new(
            "E066_ABI_IO",
            "export-abi",
            format!("Failed to write ABI file '{}': {e}", out_path.display()),
            Span::zero(),
            "Check write permissions on target path",
        )
    })
}

fn run_compile_wasm(
    cwd: &Path,
    file_path: &str,
    out_path: &str,
    sign_key_path: Option<&Path>,
    use_cache: bool,
    json_mode: bool,
) -> Result<(), InfimumDiagnostic> {
    let sign_key = match sign_key_path {
        Some(p) => {
            let hex_str = fs::read_to_string(p).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "compile-wasm",
                    format!("Failed to read signing key '{}': {e}", p.display()),
                    Span::zero(),
                    "Ensure the --sign-key file exists and is readable",
                )
            })?;
            Some(decode_key_32(&hex_str)?)
        }
        None => None,
    };
    let module = parse_module_from_path(Path::new(file_path))?;
    let mut store = GraphStore::open(cwd)?;
    let art = build_strata_wasm_from_module_signed(
        &mut store,
        &module,
        Path::new(out_path),
        use_cache,
        sign_key.as_ref(),
    )?;
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "phase": "compile-wasm",
                "diagnostics": [],
                "artifact": art,
            }))
            .unwrap_or_default()
        );
    } else {
        let sig_note = if sign_key.is_some() {
            ", Ed25519 signed"
        } else {
            ""
        };
        println!(
            "  Strata Direct Wasm -> {} [{} node(s), exports={:?}, {} bytes ({:.2} KB) in {:.2}ms, zero rustc{}]",
            art.wasm_path,
            art.compiled_nodes,
            art.exported_functions,
            art.wasm_size_bytes,
            art.wasm_size_bytes as f64 / 1024.0,
            art.build_us as f64 / 1000.0,
            sig_note
        );
    }
    Ok(())
}

fn run_verify_passport(
    ifw_path: &Path,
    pubkey_path: Option<&Path>,
    json_mode: bool,
) -> Result<(), InfimumDiagnostic> {
    let ifw_bytes = fs::read(ifw_path).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "verify-passport",
            format!("Cannot read '{}': {e}", ifw_path.display()),
            Span::zero(),
            "Verify the .ifw file path exists",
        )
    })?;
    let mut trusted = Vec::new();
    if let Some(p) = pubkey_path {
        let hex_str = fs::read_to_string(p).map_err(|e| {
            InfimumDiagnostic::new(
                "E040_FILE_READ",
                "verify-passport",
                format!("Cannot read public key '{}': {e}", p.display()),
                Span::zero(),
                "Verify the --pubkey file path exists",
            )
        })?;
        trusted.push(decode_key_32(&hex_str)?);
    }
    let verified = verify_passport_signature(&ifw_bytes, &trusted)?;
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&verified).unwrap_or_default()
        );
    } else {
        println!("=== INFIMUM PASSPORT v{} VERIFIED ===", verified.passport.schema_version);
        println!("  File:           {}", ifw_path.display());
        println!("  Root Hash:      #{}", verified.passport.short_hash);
        println!("  Wasm Code Hash: {} [OK]", verified.computed_wasm_code_hash);
        println!("  Max Memory:     {} bytes", verified.passport.max_memory_bytes);
        println!("  Z3 Obligations: {}", verified.passport.z3_obligations_proved);
        println!(
            "  Exports:        {}",
            verified.passport.exported_functions.join(", ")
        );
        println!(
            "  Pure:           {}",
            if verified.passport.is_pure { "YES" } else { "NO" }
        );
        if let Some(sig) = &verified.signature {
            println!(
                "  Signature:      {} key_id={} [VERIFIED]",
                sig.alg, sig.key_id
            );
        } else {
            println!("  Signature:      none (unsigned)");
        }
    }
    Ok(())
}
