use infimum_lang::showcase::build_showcase_assets;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn test_showcase_bundle_generation_and_dom_security_hygiene() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let out_dir = std::env::temp_dir().join(format!("infimum_s6_showcase_{nanos}"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let bundle = build_showcase_assets(&repo_root, &out_dir).expect("showcase build ok");
    assert!(out_dir.join("checkout.wasm").is_file());
    assert!(out_dir.join("diagnostics.json").is_file());
    assert!(bundle.wasm_size_bytes > 0 && bundle.wasm_size_bytes < 120 * 1024);

    // Ensure genuine Z3 & Taint diagnostics were captured (no mocked strings)
    assert_eq!(bundle.anti_stub_diagnostic.code, "E050_CONTRACT_VIOLATION");
    assert!(!bundle.anti_stub_diagnostic.counterexample.is_empty());
    assert_eq!(
        bundle.secret_leak_diagnostic.code,
        "E065_REVEAL_DOMAIN_MISMATCH"
    );

    // Verify client showcase script avoids all dangerous DOM XSS sinks
    let app_js = fs::read_to_string(repo_root.join("showcase/app.js")).unwrap();
    for forbidden in ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "eval("] {
        assert!(
            !app_js.contains(forbidden),
            "showcase/app.js must not contain dangerous DOM sink '{forbidden}'"
        );
    }

    let _ = fs::remove_dir_all(&out_dir);
}
