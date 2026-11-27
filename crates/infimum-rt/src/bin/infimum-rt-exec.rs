// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use infimum_rt::{CapsuleEngine, ExecutionPolicy};
use std::env;
use std::fs;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "Usage: infimum-rt-exec <capsule.ifw> --input <json> [--secret <val>] [--fuel <n>] [--pubkey <hex|path>] [--allow-net <domain>] [--mock-net]"
        );
        return ExitCode::FAILURE;
    }
    let ifw_path = &args[0];
    let mut input_json = String::from("{}");
    let mut secret_val: Option<String> = None;
    let mut mock_net = false;
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
                let raw = fs::read_to_string(&args[i + 1]).unwrap_or_else(|_| args[i + 1].clone());
                if let Ok(k) = infimum_lang::passport_crypto::decode_key_32(raw.trim()) {
                    policy.trusted_pubkeys.push(k);
                    policy.require_signature = true;
                }
                i += 2;
            }
            "--allow-net" if i + 1 < args.len() => {
                policy
                    .allowed_net_domains
                    .insert(args[i + 1].to_ascii_lowercase());
                i += 2;
            }
            "--mock-net" => {
                mock_net = true;
                i += 1;
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

    let observed_calls: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let exec_res = if mock_net {
        let obs = Arc::clone(&observed_calls);
        let expected_auth = secret_val
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("Bearer {s}"));
        CapsuleEngine::execute_json_with_http(
            &capsule,
            &input_json,
            secret_val.as_deref(),
            &policy,
            Some(&move |method, url, auth, _body| {
                let auth_matches_secret = match (&expected_auth, auth) {
                    (Some(exp), Some(got)) => exp == got,
                    _ => false,
                };
                if let Ok(mut g) = obs.lock() {
                    g.push(serde_json::json!({
                        "method": method,
                        "url": url,
                        "has_auth": auth.is_some(),
                        "auth_matches_secret": auth_matches_secret,
                    }));
                }
                Ok(b"{\"mock\":true}".to_vec())
            }),
        )
    } else {
        CapsuleEngine::execute_json(&capsule, &input_json, secret_val.as_deref(), &policy)
    };

    match exec_res {
        Ok(res) => {
            let mut v = serde_json::to_value(&res).unwrap_or_default();
            if mock_net {
                if let Some(obj) = v.as_object_mut() {
                    let calls = observed_calls
                        .lock()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    obj.insert("mock_net_calls".to_string(), serde_json::Value::Array(calls));
                }
            }
            println!("{}", serde_json::to_string(&v).unwrap_or_default());
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
