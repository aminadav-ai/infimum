use super::conversations::*;
use super::dev_tools::*;
use super::providers::*;
use super::timeline::*;
use super::tool_specs::*;
use super::tools::*;
use crate::policy::CapabilityPolicy;
use crate::studio_abi::*;
use serde_json::json;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn test_policy_with_root(root: PathBuf) -> CapabilityPolicy {
    CapabilityPolicy {
        allowed_fs_roots: vec![root],
        ..CapabilityPolicy::default()
    }
}

#[test]
fn get_file_outline_extracts_real_ifm_and_rs_files() {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let policy = test_policy_with_root(project_root.clone());

    // 1. Real .ifm files (modularized studio capsule: primitives.ifm + infimum_code.ifm)
    let prim_path = project_root.join("capsule/src/primitives.ifm");
    if prim_path.is_file() {
        let res = execute_native_tool(
            &policy,
            &project_root,
            "get_file_outline",
            &json!({ "path": prim_path.display().to_string() }),
            None,
        );
        assert_eq!(res["ok"], true, "primitives.ifm outline failed: {res}");
        let outline = res["outline"].as_str().unwrap();
        assert!(outline.contains("fn buf_can_insert("), "missing buf_can_insert in outline");
        assert!(outline.contains("[req]"), "missing [req] summary in outline");
        assert!(outline.contains("-[Mem]-> i64"), "missing effect signature in outline");
        assert!(res["entries_count"].as_u64().unwrap() >= 3);
        for line in outline.lines() {
            assert!(line.chars().count() <= OUTLINE_MAX_LINE_CHARS + 1);
        }
    }
    let ifm_path = project_root.join("capsule/src/infimum_code.ifm");
    if ifm_path.is_file() {
        let res = execute_native_tool(
            &policy,
            &project_root,
            "get_file_outline",
            &json!({ "path": ifm_path.display().to_string() }),
            None,
        );
        assert_eq!(res["ok"], true, "infimum_code.ifm outline failed: {res}");
        let outline = res["outline"].as_str().unwrap();
        assert!(outline.contains("export fn "), "missing export fn in outline");
        assert!(res["entries_count"].as_u64().unwrap() > 10);
        for line in outline.lines() {
            assert!(line.chars().count() <= OUTLINE_MAX_LINE_CHARS + 1);
        }
    }

    // 2. Real .rs files (modularized daemon/src/agent/*.rs)
    let conv_rs = project_root.join("daemon/src/agent/conversations.rs");
    let res_conv = execute_native_tool(
        &policy,
        &project_root,
        "get_file_outline",
        &json!({ "path": conv_rs.display().to_string() }),
        None,
    );
    assert_eq!(res_conv["ok"], true, "conversations.rs outline failed: {res_conv}");
    assert!(
        res_conv["outline"].as_str().unwrap().contains("pub struct AttachedImage"),
        "missing struct"
    );

    let tools_rs = project_root.join("daemon/src/agent/tools.rs");
    let res_tools = execute_native_tool(
        &policy,
        &project_root,
        "get_file_outline",
        &json!({ "path": tools_rs.display().to_string() }),
        None,
    );
    assert_eq!(res_tools["ok"], true, "tools.rs outline failed: {res_tools}");
    assert!(
        res_tools["outline"].as_str().unwrap().contains("fn execute_native_tool("),
        "missing execute_native_tool"
    );

    let prov_rs = project_root.join("daemon/src/agent/providers.rs");
    let res_rs = execute_native_tool(
        &policy,
        &project_root,
        "get_file_outline",
        &json!({ "path": prov_rs.display().to_string() }),
        None,
    );
    assert_eq!(res_rs["ok"], true, "providers.rs outline failed: {res_rs}");
    let rs_outline = res_rs["outline"].as_str().unwrap();
    assert!(rs_outline.contains("impl SseTurnAccumulator"), "missing impl");
    assert!(rs_outline.contains("fn ingest_line("), "missing method inside impl");

    let mod_rs = project_root.join("daemon/src/agent/mod.rs");
    let res_mod = execute_native_tool(
        &policy,
        &project_root,
        "get_file_outline",
        &json!({ "path": mod_rs.display().to_string() }),
        None,
    );
    assert_eq!(res_mod["ok"], true, "mod.rs outline failed: {res_mod}");
    assert!(
        res_mod["outline"].as_str().unwrap().contains("mod tests"),
        "missing mod tests"
    );

    // 3. Synthetic .ifm with all supported declaration kinds
    let synthetic_ifm = r#"use std.math;
const MAX_ITEMS: i64 = 128;
enum Mode { Chat, Web }
struct Point { x: i64, y: i64 }
layout Panel { w: i64, h: i64 }
forma Button { label: str }
export fn render(x: i64) -[Mem, Gpu]-> unit
  req x >= 0
  ens result == ()
{
  ()
}
"#;
    let (entries, truncated) = extract_ifm_outline(synthetic_ifm);
    assert!(!truncated);
    assert_eq!(entries.len(), 7);
    assert!(entries[0].contains("L1..L1: use std.math"));
    assert!(entries[1].contains("L2..L2: const MAX_ITEMS: i64 = 128"));
    assert!(entries[2].contains("L3..L3: enum Mode"));
    assert!(entries[3].contains("L4..L4: struct Point"));
    assert!(entries[4].contains("L5..L5: layout Panel"));
    assert!(entries[5].contains("L6..L6: forma Button"));
    assert!(entries[6].contains("L7..L12: export fn render(x: i64) -[Mem, Gpu]-> unit [req, ens]"));
}

#[test]
fn batch_edit_project_file_applies_atomically_and_rolls_back_on_error() {
    let tmp_dir = env::temp_dir().join(format!("infimum_web_batch_test_{}", now_millis()));
    fs::create_dir_all(&tmp_dir).unwrap();
    let file_path = tmp_dir.join("sample.rs");
    let original = "const A: i64 = 1;\nconst B: i64 = 2;\nconst C: i64 = 3;\n";
    fs::write(&file_path, original).unwrap();

    let policy = test_policy_with_root(tmp_dir.clone());

    // 1. Failing batch (edit #1 is valid, edit #2 is missing) -> must roll back atomically!
    let fail_res = execute_native_tool(
        &policy,
        &tmp_dir,
        "batch_edit_project_file",
        &json!({
            "path": file_path.display().to_string(),
            "edits": [
                { "target": "const A: i64 = 1;", "replacement": "const A: i64 = 10;" },
                { "target": "const NON_EXISTENT: i64 = 99;", "replacement": "const X: i64 = 0;" }
            ]
        }),
        None,
    );
    assert_eq!(fail_res["ok"], false);
    assert_eq!(fail_res["failed_index"], 1);
    assert_eq!(fs::read_to_string(&file_path).unwrap(), original);

    // 2. Overlapping edits -> must roll back atomically!
    let overlap_res = execute_native_tool(
        &policy,
        &tmp_dir,
        "batch_edit_project_file",
        &json!({
            "path": file_path.display().to_string(),
            "edits": [
                { "target": "const A: i64 = 1;\nconst B: i64 = 2;", "replacement": "X" },
                { "target": "const B: i64 = 2;\nconst C: i64 = 3;", "replacement": "Y" }
            ]
        }),
        None,
    );
    assert_eq!(overlap_res["ok"], false);
    assert_eq!(fs::read_to_string(&file_path).unwrap(), original);

    // 3. Valid multi-edit -> applies all 3 edits in a single write!
    let ok_res = execute_native_tool(
        &policy,
        &tmp_dir,
        "batch_edit_project_file",
        &json!({
            "path": file_path.display().to_string(),
            "edits": [
                { "target": "const C: i64 = 3;", "replacement": "const C: i64 = 30;" },
                { "target": "const A: i64 = 1;", "replacement": "const A: i64 = 10;" },
                { "target": "const B: i64 = 2;", "replacement": "const B: i64 = 20;" }
            ]
        }),
        None,
    );
    assert_eq!(ok_res["ok"], true, "batch edit failed: {ok_res}");
    assert_eq!(ok_res["applied_count"], 3);
    assert_eq!(ok_res["total_lines"], 3);
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "const A: i64 = 10;\nconst B: i64 = 20;\nconst C: i64 = 30;\n"
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn compact_old_tool_responses_preserves_thought_signature_recent_4_and_errors() {
    let big = "x".repeat(1500);
    let big_files: Vec<String> = (0..80).map(|i| format!("src/module_long_file_name_{i}.rs")).collect();

    // Build 6 tool steps (each is a model functionCall + user functionResponse)
    let mut contents = vec![
        json!({ "role": "user", "parts": [{ "text": "Analyze project" }] }),
        // Step 1 (older, should compact read_project_file + list_project_files + get_file_outline)
        json!({
            "role": "model",
            "parts": [{
                "functionCall": { "name": "read_project_file", "args": { "path": "a.rs" } },
                "thoughtSignature": "sig-step-1"
            }]
        }),
        json!({
            "role": "user",
            "parts": [
                {
                    "functionResponse": {
                        "name": "read_project_file",
                        "response": { "path": "a.rs", "start_line": 10, "end_line": 90, "content": big }
                    }
                },
                {
                    "functionResponse": {
                        "name": "list_project_files",
                        "response": { "project_dir": "/proj", "files": big_files }
                    }
                },
                {
                    "functionResponse": {
                        "name": "get_file_outline",
                        "response": { "ok": true, "path": "a.rs", "outline": big }
                    }
                }
            ]
        }),
        // Step 2 (older, verify_infimum_file error MUST NOT be compacted; search_project SHOULD compact)
        json!({
            "role": "model",
            "parts": [{
                "functionCall": { "name": "verify_infimum_file", "args": { "path": "a.ifm" } },
                "thoughtSignature": "sig-step-2"
            }]
        }),
        json!({
            "role": "user",
            "parts": [
                {
                    "functionResponse": {
                        "name": "verify_infimum_file",
                        "response": { "ok": false, "output": big }
                    }
                },
                {
                    "functionResponse": {
                        "name": "search_project",
                        "response": { "ok": true, "matches": big }
                    }
                }
            ]
        }),
        // Steps 3..6 (latest 4 tool steps -> MUST stay 100% full!)
        json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-3" }] }),
        json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s3.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
        json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-4" }] }),
        json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s4.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
        json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-5" }] }),
        json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s5.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
        json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-6" }] }),
        json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s6.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
    ];

    compact_old_tool_responses(&mut contents, 4);

    // 1. Model turns and thoughtSignatures are untouched
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "sig-step-1");
    assert_eq!(contents[3]["parts"][0]["thoughtSignature"], "sig-step-2");
    assert_eq!(contents[11]["parts"][0]["thoughtSignature"], "sig-step-6");

    // 2. Older read-only tools (>900 chars) are compacted with metadata
    let s1_read = contents[2]["parts"][0]["functionResponse"]["response"]["content"]
        .as_str()
        .unwrap();
    assert!(s1_read.contains("compacted older tool output: read_project_file a.rs lines 10..90"));
    assert!(s1_read.contains("1500 chars"));

    let s1_files = contents[2]["parts"][1]["functionResponse"]["response"]["files"]
        .as_str()
        .unwrap();
    assert!(s1_files.contains("compacted older tool output: list_project_files /proj (80 files)"));

    let s1_outline = contents[2]["parts"][2]["functionResponse"]["response"]["outline"]
        .as_str()
        .unwrap();
    assert!(s1_outline.contains("compacted older tool output: get_file_outline a.rs"));

    let s2_search = contents[4]["parts"][1]["functionResponse"]["response"]["matches"]
        .as_str()
        .unwrap();
    assert!(s2_search.contains("compacted older tool output: search_project"));

    // 3. Older verify_infimum_file error is preserved in full
    let s2_verify = contents[4]["parts"][0]["functionResponse"]["response"]["output"]
        .as_str()
        .unwrap();
    assert_eq!(s2_verify.len(), 1500);

    // 4. Latest 4 tool steps (indices 6, 8, 10, 12) remain completely full
    for idx in [6usize, 8, 10, 12] {
        let c = contents[idx]["parts"][0]["functionResponse"]["response"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(c.len(), 1500, "recent tool step at idx {idx} must not be compacted");
    }
}

#[test]
fn sse_accumulator_merges_text_and_preserves_thought_signature_on_tool_call() {
    let mut acc = SseTurnAccumulator::default();
    acc.ingest_line(
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"step 1 "}]}}]}"#,
    );
    acc.ingest_line(
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"step 2"}]}}]}"#,
    );
    acc.ingest_line(
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Привет, "}]}}]}"#,
    );
    acc.ingest_line(
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"мир!"}]}}]}"#,
    );
    acc.ingest_line(
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"search_project","args":{"query":"foo"}},"thoughtSignature":"sig-abc-123"}]}}],"usageMetadata":{"promptTokenCount":1200,"candidatesTokenCount":85,"thoughtsTokenCount":315}}"#,
    );

    assert_eq!(acc.chunks_parsed, 5);
    assert_eq!(acc.parts.len(), 3);
    assert_eq!(acc.parts[0]["thought"], true);
    assert_eq!(acc.parts[0]["text"], "step 1 step 2");
    assert_eq!(acc.parts[1]["text"], "Привет, мир!");
    assert_eq!(acc.parts[2]["functionCall"]["name"], "search_project");
    assert_eq!(acc.parts[2]["thoughtSignature"], "sig-abc-123");
    assert_eq!(acc.in_tokens, 1200);
    assert_eq!(acc.out_tokens, 400);
}

#[test]
fn extract_gemini_error_handles_object_and_array_payloads() {
    let obj = r#"{"error":{"code":503,"message":"The model is overloaded."}}"#;
    let arr = r#"[{"error":{"code":429,"message":"Resource has been exhausted."}}]"#;
    assert_eq!(
        extract_gemini_error_from_raw(obj).as_deref(),
        Some("The model is overloaded.")
    );
    assert_eq!(
        extract_gemini_error_from_raw(arr).as_deref(),
        Some("Resource has been exhausted.")
    );
}

#[test]
fn normalize_contents_roles_drops_leading_model_and_merges_adjacent() {
    let mut contents = vec![
        json!({"role": "model", "parts": [{"text": "stale"}]}),
        json!({"role": "user", "parts": [{"text": "hi"}]}),
        json!({"role": "user", "parts": [{"text": "again"}]}),
        json!({"role": "model", "parts": [{"text": "ok"}]}),
    ];
    normalize_contents_roles(&mut contents);
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(contents[1]["role"], "model");
}

#[test]
fn timeline_encode_roundtrip_and_bounds() {
    let snap = TimelineSnapshot {
        round: 3,
        max_rounds: 30,
        total_elapsed_tenths: 47,
        has_running_cmd: true,
        thought_snippet: "Проверяю раскладку ленты".to_string(),
        steps: vec![
            AgentStep {
                status: STEP_STATUS_DONE,
                kind: STEP_KIND_THINK,
                dur_tenths: 12,
                title: "Анализ задачи".to_string(),
                detail: "gemini-4-pro".to_string(),
                output_tail: String::new(),
                started_at: None,
            },
            AgentStep {
                status: STEP_STATUS_RUNNING,
                kind: STEP_KIND_CMD,
                dur_tenths: 35,
                title: "Команда терминала".to_string(),
                detail: "$ cargo test".to_string(),
                output_tail: "running 19 tests\ntest ok".to_string(),
                started_at: None,
            },
        ],
    };
    let words = encode_timeline_words(&snap, LIVE_TIMELINE_MAX_WORDS as usize);
    assert_eq!(words[0] as usize, words.len());
    assert_eq!(words[1], 3);
    assert_eq!(words[2], 30);
    assert_eq!(words[3], 47);
    assert_eq!(words[4], 1);
    assert_eq!(words[5], 2);
    let th_len = words[6] as usize;
    assert_eq!(th_len, "Проверяю раскладку ленты".chars().count());
    let p0 = 7 + th_len;
    assert_eq!(words[p0], STEP_STATUS_DONE);
    assert_eq!(words[p0 + 1], STEP_KIND_THINK);
    assert_eq!(words[p0 + 2], 12);
    let t0_len = words[p0 + 3] as usize;
    let q0 = p0 + 4 + t0_len;
    let d0_len = words[q0] as usize;
    let r0 = q0 + 1 + d0_len;
    let tail0_len = words[r0] as usize;
    assert_eq!(tail0_len, 0);
    let p1 = r0 + 1 + tail0_len;
    assert_eq!(words[p1], STEP_STATUS_RUNNING);
    assert_eq!(words[p1 + 1], STEP_KIND_CMD);
    assert_eq!(words[p1 + 2], 35);
}

#[test]
fn kill_active_shell_command_interrupts_sleeping_bash() {
    let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut policy = test_policy_with_root(project_root.clone());
    policy.allow_proc = true;
    ASYNC_CANCELLED.store(false, Ordering::SeqCst);
    ASYNC_CMD_KILL_REQUESTED.store(false, Ordering::SeqCst);

    let killer = std::thread::spawn(|| {
        let wait_t0 = Instant::now();
        while ASYNC_CMD_PID.load(Ordering::SeqCst) == 0 && wait_t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(25));
        }
        std::thread::sleep(Duration::from_millis(120));
        kill_active_shell_command()
    });

    let t0 = Instant::now();
    let res = execute_native_tool(
        &policy,
        &project_root,
        "run_project_command",
        &json!({ "command": "echo started; sleep 15; echo finished" }),
        None,
    );
    let elapsed = t0.elapsed();
    let did_kill = killer.join().unwrap();

    assert!(did_kill, "expected kill_active_shell_command to find running pid");
    assert_eq!(res["killed_by_user"], true, "expected killed_by_user=true, got: {res}");
    assert!(elapsed < Duration::from_secs(4), "command took too long to die: {elapsed:?}");
    assert!(res["output"].as_str().unwrap_or("").contains("started"));
}

#[test]
fn timeline_json_save_and_load_roundtrip() {
    let tmp_path = std::env::temp_dir().join(format!(
        "infimum_web_tl_roundtrip_{}.json",
        std::process::id()
    ));
    let payload = json!({
        "project": "infimum-web",
        "title": "Тест таймлайна",
        "messages": [
            { "role": "user", "content": "Проверь сборку" },
            {
                "role": "assistant",
                "content": "Сборка прошла успешно.",
                "timeline": {
                    "round": 2,
                    "max_rounds": 30,
                    "total_elapsed_tenths": 18,
                    "has_running_cmd": false,
                    "thought_snippet": "Запускаю cargo check",
                    "steps": [
                        {
                            "status": STEP_STATUS_DONE,
                            "kind": STEP_KIND_CMD,
                            "dur_tenths": 14,
                            "title": "Команда терминала",
                            "detail": "$ cargo check",
                            "output_tail": "Finished dev profile"
                        }
                    ]
                }
            }
        ]
    });
    fs::write(&tmp_path, payload.to_string()).expect("write temp conv json");
    let loaded = load_conversation_from_path(&tmp_path).expect("load conv json");
    let _ = fs::remove_file(&tmp_path);
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].0, 0);
    assert!(loaded[0].2.is_none());
    assert_eq!(loaded[1].0, 1);
    let tl = loaded[1].2.as_ref().expect("expected assistant timeline");
    assert_eq!(tl.round, 2);
    assert_eq!(tl.total_elapsed_tenths, 18);
    assert_eq!(tl.thought_snippet, "Запускаю cargo check");
    assert_eq!(tl.steps.len(), 1);
    assert_eq!(tl.steps[0].title, "Команда терминала");
    assert_eq!(tl.steps[0].output_tail, "Finished dev profile");
}

#[test]
fn single_folder_jail_blocks_sibling_project_and_sensitive_files() {
    let base = std::env::temp_dir().join(format!("infimum_jail_test_{}", std::process::id()));
    let proj_a = base.join("proj_a");
    let proj_b = base.join("proj_b");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&proj_a).unwrap();
    fs::create_dir_all(&proj_b).unwrap();
    fs::write(proj_a.join("ok.txt"), "hello_safe").unwrap();
    fs::write(proj_a.join(".env"), "SECRET=123").unwrap();
    fs::write(proj_b.join("sibling.txt"), "TOP_SECRET").unwrap();

    let mut pol = CapabilityPolicy::default();
    pol.allowed_fs_roots.push(base.clone());
    pol.allow_proc = true;

    // Reading inside active project succeeds
    let ok_res = execute_native_tool(
        &pol,
        &proj_a,
        "read_project_file",
        &json!({ "path": "ok.txt" }),
        None,
    );
    assert_eq!(ok_res.get("content").and_then(|v| v.as_str()), Some("1| hello_safe"));
    let ok_raw = execute_native_tool(
        &pol,
        &proj_a,
        "read_project_file",
        &json!({ "path": "ok.txt", "with_line_numbers": false }),
        None,
    );
    assert_eq!(ok_raw.get("content").and_then(|v| v.as_str()), Some("hello_safe"));

    // Reading sibling project via absolute path is DENIED
    let sib_res = execute_native_tool(
        &pol,
        &proj_a,
        "read_project_file",
        &json!({ "path": proj_b.join("sibling.txt").display().to_string() }),
        None,
    );
    assert!(sib_res.get("error").is_some(), "sibling read must be denied: {sib_res}");

    // Reading .env even inside active project is DENIED
    let env_res = execute_native_tool(
        &pol,
        &proj_a,
        "read_project_file",
        &json!({ "path": ".env" }),
        None,
    );
    assert!(env_res.get("error").is_some(), ".env read must be denied: {env_res}");

    // list_project_files hides .env
    let list_res = execute_native_tool(&pol, &proj_a, "list_project_files", &json!({}), None);
    let files = list_res.get("files").and_then(|v| v.as_array()).unwrap();
    assert!(files.iter().any(|f| f.as_str() == Some("ok.txt")));
    assert!(!files.iter().any(|f| f.as_str() == Some(".env")));

    // Writing inside a brand-new nested subdirectory succeeds
    let nested_ok = execute_native_tool(
        &pol,
        &proj_a,
        "write_project_file",
        &json!({ "path": "src/sub/main.ifm", "content": "fn main() -> i64 { 0 }" }),
        None,
    );
    assert_eq!(nested_ok.get("ok").and_then(|v| v.as_bool()), Some(true), "nested write must succeed: {nested_ok}");
    assert!(proj_a.join("src/sub/main.ifm").is_file());

    // Traversal via non-existent parent + `..` is DENIED
    let trav_bad = execute_native_tool(
        &pol,
        &proj_a,
        "write_project_file",
        &json!({ "path": "src/../../proj_b/escape.ifm", "content": "bad" }),
        None,
    );
    assert!(trav_bad.get("error").is_some(), "traversal write must be denied: {trav_bad}");
    assert!(!proj_b.join("escape.ifm").exists());

    let _ = fs::remove_dir_all(&base);
}

fn bwrap_userns_available() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn bwrap_sandbox_blocks_sibling_and_clears_env() {
    let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
    if !bwrap_userns_available() {
        eprintln!("skipping bwrap_sandbox_blocks_sibling_and_clears_env: userns restricted by host AppArmor");
        return;
    }
    let base = std::env::temp_dir().join(format!("infimum_bwrap_test_{}", std::process::id()));
    let proj_a = base.join("proj_a");
    let proj_b = base.join("proj_b");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&proj_a).unwrap();
    fs::create_dir_all(&proj_b).unwrap();
    fs::write(proj_a.join("inside.txt"), "INSIDE_OK").unwrap();
    fs::write(proj_b.join("outside.txt"), "OUTSIDE_LEAK").unwrap();

    let mut pol = CapabilityPolicy::default();
    pol.allowed_fs_roots.push(base.clone());
    pol.allow_proc = true;

    let cmd = format!(
        "cat inside.txt; cat {} 2>&1 || true; env",
        proj_b.join("outside.txt").display()
    );
    let res = execute_native_tool(
        &pol,
        &proj_a,
        "run_project_command",
        &json!({ "command": cmd }),
        None,
    );
    let out = res.get("output").and_then(|v| v.as_str()).unwrap_or("");
    assert!(out.contains("INSIDE_OK"), "sandbox must allow active folder: {out}");
    assert!(!out.contains("OUTSIDE_LEAK"), "sandbox leaked sibling file! {out}");
    assert!(!out.contains("GEMINI_API_KEY="), "sandbox leaked GEMINI_API_KEY! {out}");
    assert!(!out.contains("OPENROUTER_API_KEY="), "sandbox leaked OPENROUTER_API_KEY! {out}");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn bwrap_sandbox_exposes_infimum_cli_without_source_leak() {
    let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
    if !bwrap_userns_available() {
        eprintln!("skipping bwrap_sandbox_exposes_infimum_cli_without_source_leak: userns restricted by host AppArmor");
        return;
    }
    let base = std::env::temp_dir().join(format!("infimum_cli_test_{}", std::process::id()));
    let proj = base.join("proj");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&proj).unwrap();

    let mut pol = CapabilityPolicy::default();
    pol.allowed_fs_roots.push(base.clone());
    pol.allow_proc = true;

    // 1. Inside bwrap, `which infimum` must resolve to /tmp/infimum-bin/infimum,
    // while the host infimum-lang source tree remains unreachable.
    let host_lang_dir = resolve_infimum_std_dir()
        .and_then(|d| d.parent().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "/nonexistent/infimum-lang".to_string());
    let res = execute_native_tool(
        &pol,
        &proj,
        "run_project_command",
        &json!({ "command": format!("which infimum; ls '{host_lang_dir}' 2>&1 || true") }),
        None,
    );
    let out = res.get("output").and_then(|v| v.as_str()).unwrap_or("");
    if resolve_infimum_bin(&pol).is_file() {
        assert!(
            out.contains("/tmp/infimum-bin/infimum"),
            "Expected read-only infimum binary in sandbox PATH, got: {out}"
        );
    }
    assert!(
        !out.contains("Cargo.toml"),
        "Sandbox must not expose infimum-lang source tree: {out}"
    );

    // 2. compile_infimum_wasm and run_infimum_capsule must block paths outside active project
    let bad_compile = execute_native_tool(
        &pol,
        &proj,
        "compile_infimum_wasm",
        &json!({ "path": "../outside.ifm" }),
        None,
    );
    assert!(bad_compile.get("error").is_some(), "compile_infimum_wasm must enforce jail");

    let bad_run = execute_native_tool(
        &pol,
        &proj,
        "run_infimum_capsule",
        &json!({ "path": "../outside.ifw", "headless": true }),
        None,
    );
    assert!(bad_run.get("error").is_some(), "run_infimum_capsule must enforce jail");

    // 3. read_infimum_guide returns canonical docs with compiler and launcher workflow
    let guide_res = execute_native_tool(&pol, &proj, "read_infimum_guide", &json!({}), None);
    let guide_txt = guide_res.get("content").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        guide_txt.contains("infimum compile-wasm") && guide_txt.contains("run_infimum_capsule"),
        "Expected canonical Infimum guide with toolchain docs, got len={}",
        guide_txt.len()
    );

    // 4. read_infimum_std reads std/*.ifm safely and blocks path traversal
    if resolve_infimum_std_dir().is_some() {
        let std_res = execute_native_tool(&pol, &proj, "read_infimum_std", &json!({ "file": "input.ifm" }), None);
        let std_txt = std_res.get("content").and_then(|v| v.as_str()).unwrap_or("");
        assert!(std_txt.contains("STD_KEYS_HELD_WORD"), "Expected std/input.ifm content: {std_res}");
    }
    let std_bad = execute_native_tool(&pol, &proj, "read_infimum_std", &json!({ "file": "../Cargo.toml" }), None);
    assert!(std_bad.get("error").is_some(), "Path traversal in read_infimum_std must fail: {std_bad}");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn semantic_ifm_tools_and_templates_work_and_verify() {
    let base = std::env::temp_dir().join(format!("ifm_sem_tools_{}", now_millis()));
    let proj = base.join("proj");
    fs::create_dir_all(&proj).unwrap();
    let pol = CapabilityPolicy {
        allowed_net_domains: std::collections::BTreeSet::new(),
        allowed_fs_roots: vec![proj.clone()],
        allow_proc: false,
        allow_user_workspaces: false,
        dev_mode: false,
        confirm_external_mutations_override: None,
    };

    let sample = r#"struct Vec2 {
  x: word,
  y: word,
}

fn helper(a: i64) -> i64 {
  a + 1
}

fn main_fn(v: i64) -> i64 {
  helper(v) + helper(2)
}
"#;
    fs::write(proj.join("demo.ifm"), sample).unwrap();

    let got = execute_native_tool(
        &pol,
        &proj,
        "get_ifm_symbol",
        &json!({ "path": "demo.ifm", "name": "helper" }),
        None,
    );
    assert_eq!(got.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(got.get("source").and_then(|v| v.as_str()).unwrap().contains("a + 1"));

    let rep = execute_native_tool(
        &pol,
        &proj,
        "replace_ifm_symbol",
        &json!({ "path": "demo.ifm", "name": "helper", "new_source": "fn helper(a: i64) -> i64 {\n  a + 42\n}" }),
        None,
    );
    assert_eq!(rep.get("ok").and_then(|v| v.as_bool()), Some(true));
    let updated = fs::read_to_string(proj.join("demo.ifm")).unwrap();
    assert!(updated.contains("a + 42") && updated.contains("main_fn"));

    let callers = execute_native_tool(
        &pol,
        &proj,
        "find_ifm_callers",
        &json!({ "symbol": "helper" }),
        None,
    );
    assert_eq!(callers.get("count").and_then(|v| v.as_u64()), Some(1));

    // Verify all 3 starter templates with Z3 when local infimum CLI is present
    let bin = resolve_infimum_bin(&pol);
    for kind in ["ui", "game2d", "game3d"] {
        let tpl = execute_native_tool(
            &pol,
            &proj,
            "read_infimum_template",
            &json!({ "kind": kind }),
            None,
        );
        let src = tpl.get("source").and_then(|v| v.as_str()).unwrap_or("");
        assert!(src.contains("step_ui"), "template {kind} missing step_ui");
        if bin.is_file() {
            let p = proj.join(format!("tpl_{kind}.ifm"));
            fs::write(&p, src).unwrap();
            let out = Command::new(&bin)
                .args(["verify", &p.display().to_string()])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "Template '{kind}' failed Z3 verify:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    // Verify list/read/search_infimum_repo list, read live sources, search, and block path traversal
    let repo_list = execute_native_tool(
        &pol,
        &proj,
        "list_infimum_repo",
        &json!({ "prefix": "crates/infimum-lang/examples/showcase/slipgate" }),
        None,
    );
    assert!(
        repo_list.get("output").and_then(|v| v.as_str()).is_some_and(|s| s.contains("abi.ifm")),
        "repo_list={repo_list}"
    );
    let repo_read = execute_native_tool(
        &pol,
        &proj,
        "read_infimum_repo",
        &json!({ "path": "crates/infimum-lang/examples/showcase/slipgate/src/abi.ifm", "start_line": 1, "end_line": 80 }),
        None,
    );
    let content = repo_read.get("content").and_then(|v| v.as_str()).unwrap_or("");
    assert!(content.contains("struct Enemy"), "slipgate abi.ifm should use typed structs: {repo_read}");
    let repo_search = execute_native_tool(
        &pol,
        &proj,
        "search_infimum_repo",
        &json!({ "query": "struct Enemy", "prefix": "crates/infimum-lang/examples/showcase/slipgate" }),
        None,
    );
    assert!(
        repo_search.get("total_hits").and_then(|v| v.as_u64()).unwrap_or(0) >= 1,
        "repo_search={repo_search}"
    );
    let repo_bad = execute_native_tool(
        &pol,
        &proj,
        "read_infimum_repo",
        &json!({ "path": "../Cargo.toml" }),
        None,
    );
    assert!(repo_bad.get("error").is_some(), "Path traversal in read_infimum_repo must fail: {repo_bad}");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn enhanced_list_search_edit_delete_and_move_tools_work() {
    let base = std::env::temp_dir().join(format!("ifm_enhanced_fs_{}", now_millis()));
    let proj = base.join("proj");
    fs::create_dir_all(proj.join("src/nested")).unwrap();
    fs::create_dir_all(proj.join("target/debug")).unwrap();
    fs::create_dir_all(proj.join("node_modules/pkg")).unwrap();

    fs::write(proj.join("src/lib.rs"), "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n").unwrap();
    fs::write(proj.join("src/nested/util.rs"), "pub fn helper() -> i32 { alpha() + alpha() }\n").unwrap();
    fs::write(proj.join("src/app.js"), "export function greet(name) {\n  return `hi ${name}`;\n}\n").unwrap();
    fs::write(proj.join("Cargo.lock"), "alpha beta noise\n").unwrap();
    fs::write(proj.join("target/debug/ignored.rs"), "alpha\n").unwrap();
    fs::write(proj.join("node_modules/pkg/ignored.js"), "alpha\n").unwrap();

    let pol = test_policy_with_root(proj.clone());

    // 1. list_project_files with pattern, subdir, limit, and ignored dirs
    let list_rs = execute_native_tool(
        &pol,
        &proj,
        "list_project_files",
        &json!({ "subdir": "src", "pattern": "*.rs" }),
        None,
    );
    let files: Vec<String> = list_rs["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    assert_eq!(files.len(), 2, "expected 2 rs files, got {list_rs}");
    assert_eq!(list_rs["truncated"].as_bool(), Some(false));
    assert!(!files.iter().any(|f| f.contains("target") || f.contains("node_modules")));

    // 2. list_project_files truncation reporting
    let list_lim = execute_native_tool(
        &pol,
        &proj,
        "list_project_files",
        &json!({ "subdir": "src", "limit": 1 }),
        None,
    );
    assert_eq!(list_lim["truncated"].as_bool(), Some(true));
    assert!(list_lim["total_found"].as_u64().unwrap_or(0) >= 3);

    // 3. search_project with glob, files mode, count mode, and Cargo.lock exclusion
    let srch_files = execute_native_tool(
        &pol,
        &proj,
        "search_project",
        &json!({ "query": "alpha", "mode": "files" }),
        None,
    );
    let out_files = srch_files["matches"].as_str().unwrap_or("");
    assert!(out_files.contains("lib.rs"), "srch_files={srch_files}");
    assert!(!out_files.contains("Cargo.lock"), "Cargo.lock must be ignored: {srch_files}");
    assert!(!out_files.contains("target/"), "target/ must be ignored: {srch_files}");

    let srch_glob = execute_native_tool(
        &pol,
        &proj,
        "search_project",
        &json!({ "query": "greet", "glob": "*.js", "mode": "count" }),
        None,
    );
    assert_eq!(srch_glob["ok"].as_bool(), Some(true), "srch_glob={srch_glob}");
    assert!(srch_glob["matches"].as_str().unwrap_or("").contains("app.js"));

    // 4. edit_project_file replace_all + nearest_lines on miss
    let ed_all = execute_native_tool(
        &pol,
        &proj,
        "edit_project_file",
        &json!({
            "path": "src/nested/util.rs",
            "target": "alpha()",
            "replacement": "beta()",
            "replace_all": true
        }),
        None,
    );
    assert_eq!(ed_all["ok"].as_bool(), Some(true), "ed_all={ed_all}");
    assert_eq!(ed_all["replacements"].as_u64(), Some(2));

    let ed_miss = execute_native_tool(
        &pol,
        &proj,
        "edit_project_file",
        &json!({
            "path": "src/lib.rs",
            "target": "pub fn alpha() -> i64 { 999 }",
            "replacement": "x"
        }),
        None,
    );
    assert!(ed_miss.get("error").is_some());
    assert!(
        ed_miss.get("nearest_lines").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty()),
        "expected nearest_lines suggestion on miss: {ed_miss}"
    );

    // 5. move_project_file & delete_project_file
    let mv = execute_native_tool(
        &pol,
        &proj,
        "move_project_file",
        &json!({ "from": "src/app.js", "to": "src/web/app_moved.js" }),
        None,
    );
    assert_eq!(mv["ok"].as_bool(), Some(true), "mv={mv}");
    assert!(proj.join("src/web/app_moved.js").is_file());
    assert!(!proj.join("src/app.js").exists());

    let del = execute_native_tool(
        &pol,
        &proj,
        "delete_project_file",
        &json!({ "path": "src/web/app_moved.js" }),
        None,
    );
    assert_eq!(del["ok"].as_bool(), Some(true), "del={del}");
    assert!(!proj.join("src/web/app_moved.js").exists());

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn multi_language_get_and_replace_symbol_work_for_rs_and_js() {
    let base = std::env::temp_dir().join(format!("ifm_sym_multilang_{}", now_millis()));
    let proj = base.join("proj");
    fs::create_dir_all(&proj).unwrap();
    let pol = test_policy_with_root(proj.clone());

    fs::write(
        proj.join("demo.rs"),
        "pub struct Counter {\n    pub v: i32,\n}\n\npub fn compute(x: i32) -> i32 {\n    x * 2\n}\n",
    )
    .unwrap();
    fs::write(
        proj.join("worker.js"),
        "export async function handleRequest(req) {\n  return new Response('ok');\n}\n\nconst VERSION = 1;\n",
    )
    .unwrap();

    // Rust get_symbol + replace_symbol
    let rs_sym = execute_native_tool(
        &pol,
        &proj,
        "get_symbol",
        &json!({ "path": "demo.rs", "name": "compute" }),
        None,
    );
    assert_eq!(rs_sym["ok"].as_bool(), Some(true), "rs_sym={rs_sym}");
    assert!(rs_sym["source"].as_str().unwrap_or("").contains("x * 2"));

    let rs_rep = execute_native_tool(
        &pol,
        &proj,
        "replace_symbol",
        &json!({
            "path": "demo.rs",
            "name": "compute",
            "new_source": "pub fn compute(x: i32) -> i32 {\n    x * 3\n}"
        }),
        None,
    );
    assert_eq!(rs_rep["ok"].as_bool(), Some(true), "rs_rep={rs_rep}");
    assert!(fs::read_to_string(proj.join("demo.rs")).unwrap().contains("x * 3"));

    // JS get_symbol + replace_symbol
    let js_sym = execute_native_tool(
        &pol,
        &proj,
        "get_symbol",
        &json!({ "path": "worker.js", "name": "handleRequest" }),
        None,
    );
    assert_eq!(js_sym["ok"].as_bool(), Some(true), "js_sym={js_sym}");
    assert!(js_sym["source"].as_str().unwrap_or("").contains("Response('ok')"));

    let js_rep = execute_native_tool(
        &pol,
        &proj,
        "replace_symbol",
        &json!({
            "path": "worker.js",
            "name": "handleRequest",
            "new_source": "export async function handleRequest(req) {\n  return new Response('updated');\n}"
        }),
        None,
    );
    assert_eq!(js_rep["ok"].as_bool(), Some(true), "js_rep={js_rep}");
    assert!(fs::read_to_string(proj.join("worker.js")).unwrap().contains("updated"));

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn fallback_command_filter_and_dev_mode_guards_work() {
    // Token-based safety filter: `which ... curl` is safe, `curl ...` is blocked.
    assert!(!is_unsafe_fallback_command("which rg git z3 curl"));
    assert!(!is_unsafe_fallback_command("echo curl_version"));
    assert!(is_unsafe_fallback_command("curl https://example.com"));
    assert!(is_unsafe_fallback_command("echo ok && wget https://example.com/a.sh"));
    assert!(is_unsafe_fallback_command("rm -rf /"));
    assert!(is_unsafe_fallback_command("sudo ls"));

    let base = std::env::temp_dir().join(format!("ifm_dev_guards_{}", now_millis()));
    let proj = base.join("proj");
    fs::create_dir_all(&proj).unwrap();

    // Safe mode (dev_mode = false): dev-only tools are blocked
    let safe_pol = test_policy_with_root(proj.clone());
    for tool in ["gh_cli", "wrangler_cli", "web_fetch", "web_search"] {
        let res = execute_native_tool(&safe_pol, &proj, tool, &json!({}), None);
        let err = res["error"].as_str().unwrap_or("");
        assert!(
            err.contains("Developer Mode"),
            "tool {tool} must require Developer Mode when disabled: {res}"
        );
    }

    // git_tool: push requires Developer Mode, force push is always blocked
    let push_safe = execute_native_tool(
        &safe_pol,
        &proj,
        "git_tool",
        &json!({ "subcommand": "push" }),
        None,
    );
    assert!(
        push_safe["error"].as_str().unwrap_or("").contains("Developer Mode"),
        "git push must require Developer Mode: {push_safe}"
    );

    let mut dev_pol = test_policy_with_root(proj.clone());
    dev_pol.dev_mode = true;
    let push_force = execute_native_tool(
        &dev_pol,
        &proj,
        "git_tool",
        &json!({ "subcommand": "push", "args": ["--force"] }),
        None,
    );
    assert!(
        push_force["error"].as_str().unwrap_or("").to_lowercase().contains("force"),
        "git push --force must be blocked even in dev mode: {push_force}"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn web_fetch_ssrf_html_cleaner_and_gemini_grounding_parser_work() {
    // SSRF URL validation
    assert!(validate_web_url("http://127.0.0.1:8080/secret").is_err());
    assert!(validate_web_url("http://localhost/admin").is_err());
    assert!(validate_web_url("http://169.254.169.254/latest/meta-data").is_err());
    assert!(validate_web_url("http://10.0.0.1/internal").is_err());
    assert!(validate_web_url("file:///etc/passwd").is_err());
    assert!(validate_web_url("ftp://example.com/a").is_err());

    // HTML to compact text strips script/style/nav and keeps readable content
    let html = r#"
    <html><head><title>Doc</title><style>body{color:red}</style></head>
    <body>
      <nav>Home | Pricing | Login</nav>
      <script>console.log("ignore me")</script>
      <h1>Infimum Guide</h1>
      <p>Hello &amp; welcome to <b>Infimum</b>.</p>
    </body></html>
    "#;
    let txt = html_to_compact_text(html);
    assert!(txt.contains("Infimum Guide"), "txt={txt}");
    assert!(txt.contains("Hello & welcome to Infimum"), "txt={txt}");
    assert!(!txt.contains("ignore me"), "script must be stripped: {txt}");
    assert!(!txt.contains("color:red"), "style must be stripped: {txt}");
    assert!(!txt.contains("Pricing"), "nav must be stripped: {txt}");

    // Gemini Search grounding parser extracts summary, deduplicated sources, and queries
    let sample_resp = r#"{
      "candidates": [{
        "content": { "parts": [{ "text": "Cloudflare Workers support D1 and KV." }] },
        "groundingMetadata": {
          "webSearchQueries": ["cloudflare workers d1 kv"],
          "groundingChunks": [
            { "web": { "uri": "https://developers.cloudflare.com/d1/", "title": "Cloudflare D1" } },
            { "web": { "uri": "https://developers.cloudflare.com/d1/", "title": "Duplicate D1" } },
            { "web": { "uri": "https://developers.cloudflare.com/kv/", "title": "Cloudflare KV" } }
          ]
        }
      }]
    }"#;
    let parsed = parse_gemini_grounding_response(sample_resp, 5).unwrap();
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert!(parsed["summary"].as_str().unwrap_or("").contains("D1 and KV"));
    let srcs = parsed["sources"].as_array().unwrap();
    assert_eq!(srcs.len(), 2, "duplicate URLs must be deduplicated: {parsed}");
    assert_eq!(srcs[0]["title"].as_str(), Some("Cloudflare D1"));
    assert_eq!(parsed["queries_used"][0].as_str(), Some("cloudflare workers d1 kv"));
}

#[test]
fn tool_specs_filter_by_dev_mode_and_monorepo_context() {
    let safe_opts = ToolSpecOptions {
        dev_mode: false,
        include_repo_snapshot: true,
        include_pbr_tools: true,
    };
    let dev_opts = ToolSpecOptions {
        dev_mode: true,
        include_repo_snapshot: false,
        include_pbr_tools: false,
    };

    let safe_names: Vec<String> = openai_tool_declarations(safe_opts)
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(safe_names.contains(&"git_tool".to_string()));
    assert!(safe_names.contains(&"get_symbol".to_string()));
    assert!(safe_names.contains(&"delete_project_file".to_string()));
    assert!(safe_names.contains(&"list_infimum_repo".to_string()));
    assert!(!safe_names.contains(&"gh_cli".to_string()));
    assert!(!safe_names.contains(&"wrangler_cli".to_string()));
    assert!(!safe_names.contains(&"web_fetch".to_string()));
    assert!(!safe_names.contains(&"web_search".to_string()));

    let dev_names: Vec<String> = gemini_tool_declarations(dev_opts)["functionDeclarations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(dev_names.contains(&"gh_cli".to_string()));
    assert!(dev_names.contains(&"wrangler_cli".to_string()));
    assert!(dev_names.contains(&"web_fetch".to_string()));
    assert!(dev_names.contains(&"web_search".to_string()));
    assert!(!dev_names.contains(&"list_infimum_repo".to_string()));
    assert!(!dev_names.contains(&"search_pbr_textures".to_string()));
}

#[test]
fn bridge_settings_wire_and_json_roundtrip() {
    use crate::bridge_gen::NativeSettings;

    let mut s = NativeSettings::default();
    assert!(s.confirm_external_mutations);
    assert!(!s.dev_mode);

    s.theme = 1;
    s.font_scale_pct = 125;
    s.currency_id = 2;
    s.model_idx = 5;
    s.dev_mode = true;
    s.confirm_external_mutations = false;

    let wire_get = s.to_ifm_wire(true, s.dev_mode);
    assert_eq!(wire_get, "1 1 125 2 5 1 0\n");
    assert_eq!(s.to_ifm_wire(false, false), "0\n");

    let wire_put = wire_get.strip_prefix("1 ").unwrap();
    let decoded = NativeSettings::from_ifm_wire(wire_put, NativeSettings::default());
    assert_eq!(decoded.theme, 1);
    assert_eq!(decoded.font_scale_pct, 125);
    assert_eq!(decoded.currency_id, 2);
    assert_eq!(decoded.model_idx, 5);
    assert!(decoded.dev_mode);
    assert!(!decoded.confirm_external_mutations);

    let j = decoded.to_json();
    let merged = NativeSettings::from_json_merge(&j, NativeSettings::default());
    assert_eq!(merged.theme, 1);
    assert_eq!(merged.font_scale_pct, 125);
    assert!(merged.dev_mode);
    assert!(!merged.confirm_external_mutations);
}

#[test]
fn approval_classifier_and_gate_allow_deny_cancel() {
    use crate::agent::approval::{
        cancel_pending_approval, classify_external_mutation, pending_approval_id,
        pending_approval_snapshot, request_external_approval_with_timeout,
        resolve_pending_approval,
    };
    use std::time::Duration;

    // Read-only commands must not require confirmation
    assert!(classify_external_mutation("git_tool", "status", &[], ".").is_none());
    assert!(classify_external_mutation("git_tool", "diff", &[], ".").is_none());
    assert!(classify_external_mutation("gh_cli", "", &["pr".into(), "list".into()], ".").is_none());
    assert!(classify_external_mutation("wrangler_cli", "", &["whoami".into()], ".").is_none());

    // External mutating commands must be classified
    let push_info = classify_external_mutation(
        "git_tool",
        "push",
        &["origin".into(), "main".into()],
        "infimum",
    )
    .expect("git push must require approval");
    assert!(push_info.command.contains("git push"));

    let deploy_info = classify_external_mutation(
        "wrangler_cli",
        "",
        &["deploy".into()],
        "infimum-site",
    )
    .expect("wrangler deploy must require approval");
    assert!(deploy_info.command.contains("wrangler deploy"));

    let pr_merge = classify_external_mutation(
        "gh_cli",
        "",
        &["pr".into(), "merge".into(), "42".into()],
        "infimum",
    )
    .expect("gh pr merge must require approval");
    assert!(pr_merge.command.contains("gh pr merge"));

    // 1) Allow flow
    let h_allow = std::thread::spawn(move || {
        request_external_approval_with_timeout(push_info, Duration::from_secs(5))
    });
    let mut seen_id = 0u64;
    for _ in 0..80 {
        let id = pending_approval_id();
        if id > 0 {
            seen_id = id;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(seen_id > 0, "pending approval id should become active");
    let snap = pending_approval_snapshot().expect("snapshot present");
    assert_eq!(snap.id, seen_id);
    assert!(resolve_pending_approval(seen_id, true));
    assert!(h_allow.join().unwrap().is_ok());

    // 2) Deny flow
    let h_deny = std::thread::spawn(move || {
        request_external_approval_with_timeout(deploy_info, Duration::from_secs(5))
    });
    let mut deny_id = 0u64;
    for _ in 0..80 {
        let id = pending_approval_id();
        if id > 0 {
            deny_id = id;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(deny_id > 0);
    assert!(resolve_pending_approval(deny_id, false));
    let deny_res = h_deny.join().unwrap();
    assert!(deny_res.is_err());

    // 3) Cancel flow
    let h_cancel = std::thread::spawn(move || {
        request_external_approval_with_timeout(pr_merge, Duration::from_secs(5))
    });
    for _ in 0..80 {
        if pending_approval_id() > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cancel_pending_approval();
    assert!(h_cancel.join().unwrap().is_err());
    assert_eq!(pending_approval_id(), 0);
}

#[test]
fn diff_view_collects_status_and_paginates_unified_diff() {
    use crate::agent::diff_view::{collect_git_diff_page, collect_git_status_entries};
    use std::process::Command;

    let base = std::env::temp_dir().join(format!("ifm_diff_view_{}", now_millis()));
    let repo = base.join("sample_repo");
    fs::create_dir_all(&repo).unwrap();

    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git must run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run_git(&["init", "-q"]);
    fs::write(repo.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
    run_git(&["add", "hello.txt"]);
    run_git(&["commit", "-q", "-m", "init"]);

    // Modify tracked file and add untracked file
    fs::write(repo.join("hello.txt"), "line1\nline2_changed\nline3\nline4\n").unwrap();
    fs::write(repo.join("new_file.txt"), "alpha\nbeta\n").unwrap();

    // Test both direct repo root and parent meta-workspace discovery
    let entries = collect_git_status_entries(&base);
    assert_eq!(entries.len(), 2, "Expected 2 changed entries: {entries:?}");
    let hello = entries.iter().find(|e| e.path == "sample_repo/hello.txt").unwrap();
    assert_eq!(hello.status, 'M');
    assert_eq!(hello.added, 2);
    assert_eq!(hello.deleted, 1);

    let untracked = entries.iter().find(|e| e.path == "sample_repo/new_file.txt").unwrap();
    assert_eq!(untracked.status, '?');
    assert_eq!(untracked.added, 2);

    // Unified diff for modified file
    let page = collect_git_diff_page(&base, Some("sample_repo/hello.txt"), 0, 50);
    let page_txt = page.lines.join("\n");
    assert!(page.total_lines > 0);
    assert!(page_txt.contains("-line2"), "Missing deleted line: {page_txt}");
    assert!(page_txt.contains("+line2_changed"), "Missing added line: {page_txt}");
    assert!(page_txt.contains("@@"), "Missing hunk header: {page_txt}");

    // Synthetic diff for untracked file
    let new_page = collect_git_diff_page(&base, Some("sample_repo/new_file.txt"), 0, 50);
    let new_txt = new_page.lines.join("\n");
    assert!(new_txt.contains("+alpha"));
    assert!(new_txt.contains("+beta"));

    // Pagination check
    let p1 = collect_git_diff_page(&base, None, 0, 3);
    assert_eq!(p1.lines.len(), 3);
    assert!(p1.total_lines > 3);

    let _ = fs::remove_dir_all(&base);
}
