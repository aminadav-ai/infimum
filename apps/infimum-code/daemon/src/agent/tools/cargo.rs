//! Structured Rust `cargo_check` tool (`check`, `test`, `clippy`, `build`) using
//! `--message-format=json` to produce ultra-compact, actionable diagnostics without noisy build logs.

use crate::policy::CapabilityPolicy;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent::dev_tools::{build_clean_path, run_subprocess_managed, truncate_head_tail};
use crate::agent::tools::sandbox::resolve_sandboxed_subdir;

#[derive(Debug, Default)]
pub(in crate::agent) struct ParsedCargoOutput {
    pub(in crate::agent) errors: Vec<String>,
    pub(in crate::agent) warnings: Vec<String>,
    pub(in crate::agent) test_summary: Vec<String>,
    pub(in crate::agent) failed_tests: Vec<String>,
    pub(in crate::agent) failure_details: Vec<String>,
    pub(in crate::agent) non_json_tail: Vec<String>,
}

/// Parse combined stdout/stderr from `cargo <action> --message-format=json` into compact
/// deduplicated error/warning lines and test failure summaries.
pub(in crate::agent) fn parse_cargo_json_output(raw: &str, max_diagnostics: usize) -> ParsedCargoOutput {
    let mut out = ParsedCargoOutput::default();
    let mut seen_errors: HashSet<String> = HashSet::new();
    let mut seen_warnings: HashSet<String> = HashSet::new();
    let mut in_failures_block = false;
    let mut in_failure_names = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') && trimmed.ends_with('}') {
            if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("");
                if reason == "compiler-message" {
                    if let Some(msg) = v.get("message") {
                        let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("");
                        if level == "error" || level == "warning" {
                            let text = msg
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("")
                                .trim();
                            // Skip summary lines like "aborting due to 2 previous errors" / "warning: ... generated 3 warnings"
                            if text.starts_with("aborting due to ")
                                || text.contains(" emitted")
                                || text.starts_with("`") && text.ends_with("warnings")
                            {
                                continue;
                            }
                            let code = msg
                                .get("code")
                                .and_then(|c| c.get("code"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("");
                            let primary_span = msg
                                .get("spans")
                                .and_then(|s| s.as_array())
                                .and_then(|arr| {
                                    arr.iter()
                                        .find(|sp| {
                                            sp.get("is_primary")
                                                .and_then(|b| b.as_bool())
                                                .unwrap_or(false)
                                        })
                                        .or_else(|| arr.first())
                                });
                            let loc = if let Some(sp) = primary_span {
                                let f = sp.get("file_name").and_then(|s| s.as_str()).unwrap_or("?");
                                let ln = sp.get("line_start").and_then(|n| n.as_u64()).unwrap_or(0);
                                let col = sp.get("column_start").and_then(|n| n.as_u64()).unwrap_or(0);
                                format!("{f}:{ln}:{col}")
                            } else {
                                "<unknown>".to_string()
                            };
                            let label = primary_span
                                .and_then(|sp| sp.get("label"))
                                .and_then(|s| s.as_str())
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty() && *s != text);
                            // Extract first helpful child suggestion if present
                            let help = msg
                                .get("children")
                                .and_then(|c| c.as_array())
                                .and_then(|arr| {
                                    arr.iter().find_map(|ch| {
                                        let cl = ch.get("level").and_then(|s| s.as_str()).unwrap_or("");
                                        if cl == "help" || cl == "note" {
                                            let cm = ch.get("message").and_then(|s| s.as_str()).unwrap_or("").trim();
                                            let repl = ch
                                                .get("spans")
                                                .and_then(|s| s.as_array())
                                                .and_then(|sa| sa.first())
                                                .and_then(|sp| sp.get("suggested_replacement"))
                                                .and_then(|s| s.as_str())
                                                .map(|s| s.trim())
                                                .filter(|s| !s.is_empty() && s.len() <= 80);
                                            if let Some(r) = repl {
                                                Some(format!("{cm}: `{r}`"))
                                            } else if !cm.is_empty() && cm.len() <= 140 {
                                                Some(cm.to_string())
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    })
                                });
                            let mut entry = if code.is_empty() {
                                format!("{loc}: {text}")
                            } else {
                                format!("{loc} [{code}]: {text}")
                            };
                            if let Some(lbl) = label {
                                entry.push_str(&format!(" ({lbl})"));
                            }
                            if let Some(h) = help {
                                entry.push_str(&format!(" -> help: {h}"));
                            }
                            if level == "error" {
                                if seen_errors.insert(entry.clone())
                                    && out.errors.len() < max_diagnostics
                                {
                                    out.errors.push(entry);
                                }
                            } else if seen_warnings.insert(entry.clone())
                                && out.warnings.len() < max_diagnostics
                            {
                                out.warnings.push(entry);
                            }
                        }
                    }
                }
                continue;
            }
        }

        // Parse cargo test runner output (non-JSON lines)
        if trimmed.starts_with("test result:") {
            in_failures_block = false;
            in_failure_names = false;
            out.test_summary.push(trimmed.to_string());
            continue;
        }
        if trimmed == "failures:" {
            if !in_failures_block {
                in_failures_block = true;
                in_failure_names = false;
            } else {
                in_failure_names = true;
            }
            continue;
        }
        if in_failure_names {
            if trimmed.starts_with("test result:") {
                in_failure_names = false;
            } else if out.failed_tests.len() < 40 {
                let s = trimmed.to_string();
                if !out.failed_tests.contains(&s) {
                    out.failed_tests.push(s);
                }
            }
            continue;
        }
        if in_failures_block {
            if out.failure_details.len() < 120 {
                out.failure_details.push(line.to_string());
            }
            continue;
        }
        if trimmed.starts_with("test ") && trimmed.ends_with("... FAILED") {
            let name = trimmed
                .trim_start_matches("test ")
                .trim_end_matches("... FAILED")
                .trim()
                .to_string();
            if !out.failed_tests.contains(&name) {
                out.failed_tests.push(name);
            }
            continue;
        }
        // Keep genuine cargo errors (e.g. Cargo.toml parse error / missing package) while skipping noise
        if (trimmed.starts_with("error:")
            || trimmed.starts_with("error[")
            || trimmed.starts_with("Caused by:")
            || trimmed.starts_with("panicked at"))
            && out.non_json_tail.len() < 30
        {
            out.non_json_tail.push(trimmed.to_string());
        }
    }

    out
}

/// Detect a Cargo project directory inside `default_proj` (either root or a unique depth-1 child).
pub(in crate::agent) fn detect_cargo_manifest_dir(default_proj: &Path) -> Option<PathBuf> {
    if default_proj.join("Cargo.toml").is_file() {
        return Some(default_proj.to_path_buf());
    }
    let mut found = Vec::new();
    if let Ok(rd) = fs::read_dir(default_proj) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() && p.join("Cargo.toml").is_file() {
                found.push(p);
            }
        }
    }
    if found.len() == 1 {
        return found.into_iter().next();
    }
    None
}

pub(in crate::agent) fn has_cargo_project(default_proj: &Path) -> bool {
    if default_proj.join("Cargo.toml").is_file() {
        return true;
    }
    if let Ok(rd) = fs::read_dir(default_proj) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() && p.join("Cargo.toml").is_file() {
                return true;
            }
        }
    }
    false
}

pub(in crate::agent) fn tool_cargo_check(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    args: &Value,
) -> Value {
    if !policy.allow_proc {
        return json!({ "error": "Proc capability denied (launch with --allow-proc)" });
    }
    let action = args
        .get("action")
        .and_then(|s| s.as_str())
        .unwrap_or("check")
        .trim()
        .to_ascii_lowercase();
    if !matches!(action.as_str(), "check" | "test" | "clippy" | "build") {
        return json!({
            "error": format!("Unsupported cargo action '{action}'. Allowed: check, test, clippy, build")
        });
    }

    let cwd = match args
        .get("cwd")
        .and_then(|s| s.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(sub) => match resolve_sandboxed_subdir(default_proj, Some(sub)) {
            Ok(d) => d,
            Err(e) => return json!({ "error": e }),
        },
        None => match detect_cargo_manifest_dir(default_proj) {
            Some(d) => d,
            None => {
                return json!({
                    "error": "No Cargo.toml found in project root (or multiple sub-crates exist). Pass 'cwd' explicitly."
                });
            }
        },
    };
    if !cwd.join("Cargo.toml").is_file() {
        return json!({
            "error": format!("No Cargo.toml found in '{}'", cwd.display())
        });
    }

    let package = args
        .get("package")
        .and_then(|s| s.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let filter = args
        .get("filter")
        .and_then(|s| s.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let release = args
        .get("release")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_diagnostics = args
        .get("max_diagnostics")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .clamp(1, 100) as usize;
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(180)
        .clamp(10, 300);

    let home = env::var("HOME").unwrap_or_default();
    let cargo_dir = PathBuf::from(&home).join(".cargo");
    let rustup_dir = PathBuf::from(&home).join(".rustup");
    let cargo_bin = {
        let c = cargo_dir.join("bin/cargo");
        if c.is_file() {
            c
        } else {
            PathBuf::from("cargo")
        }
    };

    let mut cmd = Command::new(&cargo_bin);
    cmd.current_dir(&cwd)
        .arg(&action)
        .arg("--message-format=json")
        .arg("--color=never");
    if let Some(pkg) = package {
        cmd.arg("-p").arg(pkg);
    }
    if release {
        cmd.arg("--release");
    }
    if action == "test" {
        if let Some(flt) = filter {
            cmd.arg(flt);
        }
    }
    cmd.env("PATH", build_clean_path());
    if cargo_dir.is_dir() {
        cmd.env("CARGO_HOME", &cargo_dir);
    }
    if rustup_dir.is_dir() {
        cmd.env("RUSTUP_HOME", &rustup_dir);
    }

    let label = match package {
        Some(p) => format!("cargo {action} -p {p}"),
        None => format!("cargo {action}"),
    };
    match run_subprocess_managed(cmd, timeout_secs, &label) {
        Ok((code, raw_output, timed_out)) => {
            let parsed = parse_cargo_json_output(&raw_output, max_diagnostics);
            let mut diag_lines: Vec<String> = Vec::new();
            if !parsed.errors.is_empty() {
                diag_lines.push(format!("ERRORS ({}):", parsed.errors.len()));
                for e in &parsed.errors {
                    diag_lines.push(format!("  E: {e}"));
                }
            }
            if !parsed.warnings.is_empty() {
                diag_lines.push(format!("WARNINGS ({}):", parsed.warnings.len()));
                for w in &parsed.warnings {
                    diag_lines.push(format!("  W: {w}"));
                }
            }
            if !parsed.failed_tests.is_empty() {
                diag_lines.push(format!("FAILED TESTS ({}): {}", parsed.failed_tests.len(), parsed.failed_tests.join(", ")));
            }
            if !parsed.failure_details.is_empty() {
                diag_lines.push("FAILURE DETAILS:".to_string());
                for d in &parsed.failure_details {
                    diag_lines.push(d.clone());
                }
            }
            if !parsed.test_summary.is_empty() {
                for s in &parsed.test_summary {
                    diag_lines.push(s.clone());
                }
            }
            if !parsed.non_json_tail.is_empty() && parsed.errors.is_empty() {
                for l in &parsed.non_json_tail {
                    diag_lines.push(l.clone());
                }
            }
            let (diagnostics_text, truncated) = truncate_head_tail(&diag_lines.join("\n"), 5500);
            json!({
                "ok": code == 0 && !timed_out,
                "action": action,
                "cwd": cwd.display().to_string(),
                "exit_code": code,
                "timed_out": timed_out,
                "error_count": parsed.errors.len(),
                "warning_count": parsed.warnings.len(),
                "failed_tests": parsed.failed_tests,
                "test_summary": parsed.test_summary,
                "truncated": truncated,
                "diagnostics_text": diagnostics_text,
            })
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}
