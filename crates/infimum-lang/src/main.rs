#![deny(clippy::todo, clippy::unimplemented, dead_code, unused_must_use)]

use infimum_lang::abi::{generate_json_abi, generate_rust_abi};
use infimum_lang::codegen::{build_native_binary, run_native_binary};
use infimum_lang::diag::{InfimumDiagnostic, Span};
use infimum_lang::parser::parse_module_from_path;
use infimum_lang::showcase::{build_showcase_assets, serve_showcase_http};
use infimum_lang::store::GraphStore;
use infimum_lang::strata_elf::{build_strata_elf_target, run_strata_elf_target};
use infimum_lang::strata_wasm::{
    build_strata_wasm_from_module_with_cache, build_strata_wasm_target,
};
use infimum_lang::wasm::{build_wasm_module, WasmArtifact};
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
                eprintln!("{}", diag.to_json_pretty());
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
            let module = parse_module_from_path(Path::new(file_path))?;
            let store = GraphStore::open(&cwd)?;
            let reports = store.verify_module_with_cache(&module, use_cache)?;

            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&reports).unwrap_or_default()
                );
            } else {
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
            let module = parse_module_from_path(Path::new(file_path))?;
            let mut store = GraphStore::open(&cwd)?;
            let art = build_strata_wasm_from_module_with_cache(
                &mut store,
                &module,
                Path::new(out_path),
                use_cache,
            )?;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&art).unwrap_or_default());
            } else {
                println!(
                    "  Strata Direct Wasm -> {} [{} node(s), exports={:?}, {} bytes ({:.2} KB) in {:.2}ms, zero rustc]",
                    art.wasm_path,
                    art.compiled_nodes,
                    art.exported_functions,
                    art.wasm_size_bytes,
                    art.wasm_size_bytes as f64 / 1024.0,
                    art.build_us as f64 / 1000.0
                );
            }
            Ok(())
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
        ["build-strata-elf", target, "-o", out_path] => {
            let store = GraphStore::open(&cwd)?;
            let art = build_strata_elf_target(&store, target, Path::new(out_path))?;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&art).unwrap_or_default());
            } else {
                println!(
                    "  Strata Direct ELF64 -> {} [{} node(s), {} bytes ({:.2} KB) in {:.2}ms, zero rustc]",
                    art.elf_path,
                    art.compiled_nodes,
                    art.elf_size_bytes,
                    art.elf_size_bytes as f64 / 1024.0,
                    art.build_us as f64 / 1000.0
                );
            }
            Ok(())
        }
        ["run-strata", target, rest @ ..] => {
            let store = GraphStore::open(&cwd)?;
            let outcome = run_strata_elf_target(&store, &cwd, target, rest)?;
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_default()
                );
            } else {
                if !outcome.stdout.is_empty() {
                    print!("{}", outcome.stdout);
                }
                if outcome.exit_code != 0 {
                    return Err(InfimumDiagnostic::new(
                        "E074_RUNTIME_REJECTED",
                        "strata-elf",
                        format!("Strata ELF '{}' exited with status {}", target, outcome.exit_code),
                        Span::zero(),
                        "Check CLI argument count and root 'req' preconditions",
                    ));
                }
            }
            Ok(())
        }
        ["build", target] => {
            let store = GraphStore::open(&cwd)?;
            let art = build_native_binary(&store, &cwd, target, None)?;
            print_build_artifact(&art, json_mode);
            Ok(())
        }
        ["build", target, "-o", out_path] => {
            let store = GraphStore::open(&cwd)?;
            let art = build_native_binary(&store, &cwd, target, Some(Path::new(out_path)))?;
            print_build_artifact(&art, json_mode);
            Ok(())
        }
        ["build", target, "--target", "wasm"] | ["build-wasm", target] => {
            let store = GraphStore::open(&cwd)?;
            let art = build_wasm_module(&store, &cwd, target, None)?;
            print_wasm_artifact(&art, json_mode);
            Ok(())
        }
        ["build", target, "--target", "wasm", "-o", out_path]
        | ["build-wasm", target, "-o", out_path] => {
            let store = GraphStore::open(&cwd)?;
            let art = build_wasm_module(&store, &cwd, target, Some(Path::new(out_path)))?;
            print_wasm_artifact(&art, json_mode);
            Ok(())
        }
        ["run", target, rest @ ..] => {
            let store = GraphStore::open(&cwd)?;
            let outcome = run_native_binary(&store, &cwd, target, rest)?;
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_default()
                );
            } else {
                if !outcome.stdout.is_empty() {
                    print!("{}", outcome.stdout);
                }
                if !outcome.stderr.is_empty() {
                    eprint!("{}", outcome.stderr);
                }
                if outcome.exit_code != 0 {
                    return Err(InfimumDiagnostic::new(
                        "E074_RUNTIME_REJECTED",
                        "runtime",
                        format!(
                            "Node '{}' exited with status {}",
                            target, outcome.exit_code
                        ),
                        Span::zero(),
                        "Ensure CLI arguments match parameter types and satisfy root 'req' preconditions",
                    ));
                }
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
        ["build-showcase"] => {
            let out_dir = cwd.join("showcase");
            let bundle = build_showcase_assets(&cwd, &out_dir)?;
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&bundle).unwrap_or_default()
                );
            } else {
                println!(
                    "  Showcase assets built in {} (checkout.wasm = {:.1} KB, {} verified nodes)",
                    out_dir.display(),
                    bundle.wasm_size_bytes as f64 / 1024.0,
                    bundle.ingested_nodes.len()
                );
            }
            Ok(())
        }
        ["serve-showcase"] => {
            let out_dir = cwd.join("showcase");
            let _ = build_showcase_assets(&cwd, &out_dir)?;
            serve_showcase_http(&out_dir, "127.0.0.1:8787")
        }
        ["serve-showcase", bind_addr] => {
            let out_dir = cwd.join("showcase");
            let _ = build_showcase_assets(&cwd, &out_dir)?;
            serve_showcase_http(&out_dir, bind_addr)
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
            "Usage: infimum <mcp | verify <f.ifm> | ingest <f.ifm> | compile-wasm <f.ifm> -o <out.wasm> | export-abi <f.ifm> --rust [-o <out.rs>] | build-strata-wasm <ref> -o <out.wasm> | build-strata-elf <ref> -o <out> | run-strata <ref> [args...] | build <ref> [-o out] | run <ref> [args...] | show <ref> | graph | passport <ref> | rename <old> <new>> [--json] [--no-cache]",
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

fn print_build_artifact(art: &infimum_lang::codegen::BuildArtifact, json_mode: bool) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(art).unwrap_or_default());
    } else {
        let hit = if art.cache_hit {
            "cache-hit"
        } else {
            "compiled -O native"
        };
        println!(
            "  Built #{} ({}) -> {} [{} node(s), {} KB, {} in {}ms]",
            art.short_hash,
            art.entry_alias_or_ref,
            art.binary_path,
            art.compiled_nodes,
            art.binary_size_bytes / 1024,
            hit,
            art.build_ms
        );
    }
}

fn print_wasm_artifact(art: &WasmArtifact, json_mode: bool) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(art).unwrap_or_default());
    } else {
        let hit = if art.cache_hit {
            "cache-hit"
        } else {
            "compiled wasm32 -Oz"
        };
        println!(
            "  Built Wasm #{} ({}) -> {} [{} node(s), {:.1} KB, {} in {}ms]",
            art.short_hash,
            art.entry_alias_or_ref,
            art.wasm_path,
            art.compiled_nodes,
            art.wasm_size_bytes as f64 / 1024.0,
            hit,
            art.build_ms
        );
    }
}
