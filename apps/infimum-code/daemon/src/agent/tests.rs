use super::conversations::*;
use super::providers::*;
use super::timeline::*;
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
    assert_eq!(ok_res.get("content").and_then(|v| v.as_str()), Some("hello_safe"));

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

#[test]
fn bwrap_sandbox_blocks_sibling_and_clears_env() {
    let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
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
