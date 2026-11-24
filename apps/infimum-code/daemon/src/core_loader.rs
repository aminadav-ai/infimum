//! Hot-loader for Z3-verified `.ifw` agent cores (`agent_core`, `editor_core`, `outline_core`).
//!
//! Checks `cores/<file_name>` on disk first (so recompiling a `.ifm` core takes effect
//! immediately without recompiling `infimum-agentd`), falling back to embedded bytes.

use std::path::PathBuf;
use wasmi::{Config, Engine, Module};

pub fn load_core_axw(file_name: &str, embedded: &[u8]) -> (Engine, Module) {
    let mut cfg = Config::default();
    cfg.consume_fuel(true);
    let engine = Engine::new(&cfg);

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        // Monorepo workspace target/{release,debug}/infimum-agentd -> apps/infimum-code/cores/<file>
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            candidates.push(root.join("apps/infimum-code/cores").join(file_name));
        }
        // Installed side-by-side layout: <prefix>/bin/infimum-agentd -> <prefix>/cores/<file>
        if let Some(root) = exe.parent().and_then(|p| p.parent()) {
            candidates.push(root.join("cores").join(file_name));
        }
    }
    if let Ok(dir) = std::env::var("INFIMUM_CODE_HOME") {
        candidates.push(PathBuf::from(dir).join("cores").join(file_name));
    }
    candidates.push(PathBuf::from("cores").join(file_name));

    for p in candidates {
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(module) = Module::new(&engine, &bytes) {
                return (engine, module);
            }
        }
    }
    let module = Module::new(&engine, embedded)
        .unwrap_or_else(|e| panic!("Embedded core module {file_name} must be valid Wasm: {e}"));
    (engine, module)
}
