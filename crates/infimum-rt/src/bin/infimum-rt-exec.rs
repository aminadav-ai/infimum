// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use infimum_rt::{CapsuleEngine, ExecutionPolicy};
use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "Usage: infimum-rt-exec <capsule.ifw> --input <json> [--secret <val>] [--fuel <n>] [--pubkey <hex>]"
        );
        return ExitCode::FAILURE;
    }
    let ifw_path = &args[0];
    let mut input_json = String::from("{}");
    let mut secret_val: Option<String> = None;
    let mut policy = ExecutionPolicy::default();

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--input" if i + 1 < args.len() => {
                input_json = args[i + 1].clone();
                i += 2;
            }
            "--secret" if i + 1 < args.len() => {
                secret_val = Some(args[i + 1].clone());
                i += 2;
            }
            "--fuel" if i + 1 < args.len() => {
                if let Ok(f) = args[i + 1].parse::<u64>() {
                    policy.fuel_limit = f;
                }
                i += 2;
            }
            "--pubkey" if i + 1 < args.len() => {
                if let Ok(k) = infimum_lang::passport_crypto::decode_key_32(&args[i + 1]) {
                    policy.trusted_pubkeys.push(k);
                    policy.require_signature = true;
                }
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    let ifw_bytes = match fs::read(ifw_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to read '{ifw_path}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let capsule = match CapsuleEngine::load_ifw(ifw_bytes, &policy) {
        Ok(c) => c,
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "error": e.to_string() })
            );
            return ExitCode::FAILURE;
        }
    };

    match CapsuleEngine::execute_json(&capsule, &input_json, secret_val.as_deref(), &policy) {
        Ok(res) => {
            println!("{}", serde_json::to_string(&res).unwrap_or_default());
            if res.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "error": e.to_string() })
            );
            ExitCode::FAILURE
        }
    }
}
