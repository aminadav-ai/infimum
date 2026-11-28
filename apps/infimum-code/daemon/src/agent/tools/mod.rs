//! Modular native toolset for `infimum-agentd`.
//! Decomposed by domain: `sandbox`, `fs`, `patch`, `symbols`, `cargo`, `ifm`, `describe`.

use crate::policy::CapabilityPolicy;
use serde_json::{json, Value};
use std::path::Path;

use crate::agent::dev_tools::{
    tool_gh_cli, tool_git, tool_web_fetch, tool_web_search, tool_wrangler_cli,
};

pub(in crate::agent) mod cargo;
pub(in crate::agent) mod describe;
pub(in crate::agent) mod fs;
pub(in crate::agent) mod ifm;
pub(in crate::agent) mod patch;
pub(in crate::agent) mod sandbox;
pub(in crate::agent) mod symbols;

pub use sandbox::{
    add_allowed_workspace, delete_conversations_for_project, is_sensitive_path,
    load_allowed_workspaces, remove_allowed_workspace, resolve_whitelisted_project,
    save_allowed_workspaces,
};
pub(crate) use ifm::{resolve_infimum_bin, resolve_infimum_web_bin};

pub(in crate::agent) use cargo::has_cargo_project;
pub(in crate::agent) use describe::{describe_tool_ru, describe_tool_step};
pub(in crate::agent) use ifm::append_play_tag_if_axw_present;
pub(in crate::agent) use sandbox::resolve_sandboxed_subdir;

#[cfg(test)]
pub(in crate::agent) use cargo::parse_cargo_json_output;
#[cfg(test)]
pub(in crate::agent) use ifm::resolve_infimum_std_dir;
#[cfg(test)]
pub(in crate::agent) use sandbox::is_unsafe_fallback_command;
#[cfg(test)]
pub(in crate::agent) use symbols::{
    extract_ifm_outline, extract_js_outline, parse_js_symbol_spans, parse_rs_symbol_spans,
    OUTLINE_MAX_LINE_CHARS,
};

/// Central dispatcher for all native workspace tools.
pub(in crate::agent) fn execute_native_tool(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    name: &str,
    args: &Value,
    step_idx: Option<usize>,
) -> Value {
    match name {
        // Filesystem & search
        "list_project_files" => fs::tool_list_project_files(default_proj, args),
        "search_project" => fs::tool_search_project(default_proj, args),
        "read_project_file" => fs::tool_read_project_file(default_proj, args),
        "edit_project_file" => fs::tool_edit_project_file(default_proj, args),
        "batch_edit_project_file" => fs::tool_batch_edit_project_file(default_proj, args),
        "write_project_file" => fs::tool_write_project_file(default_proj, args),
        "delete_project_file" => fs::tool_delete_project_file(default_proj, args),
        "move_project_file" => fs::tool_move_project_file(default_proj, args),
        "run_project_command" => fs::tool_run_project_command(policy, default_proj, args, step_idx),

        // Multi-file atomic unified diff patching
        "apply_patch" => patch::tool_apply_patch(default_proj, args),

        // AST-backed outline, symbol read/replace, and reference search
        "get_file_outline" => symbols::tool_get_file_outline(default_proj, args),
        "get_symbol" | "get_ifm_symbol" => symbols::tool_get_symbol(default_proj, args),
        "replace_symbol" | "replace_ifm_symbol" => symbols::tool_replace_symbol(default_proj, args),
        "find_references" => symbols::tool_find_references(default_proj, args, false),
        "find_ifm_callers" => symbols::tool_find_references(default_proj, args, true),

        // Structured Rust cargo diagnostics
        "cargo_check" => cargo::tool_cargo_check(policy, default_proj, args),

        // Infimum Lang & PBR toolchain
        "read_infimum_guide" => json!({
            "path": "docs/INFIMUM_LANG_GUIDE.md",
            "content": ifm::read_canonical_infimum_guide(),
        }),
        "read_infimum_std" => ifm::tool_read_infimum_std(args),
        "read_infimum_template" => ifm::tool_read_infimum_template(args),
        "verify_infimum_file" => ifm::tool_verify_infimum_file(policy, default_proj, args),
        "compile_infimum_wasm" => ifm::tool_compile_infimum_wasm(policy, default_proj, args),
        "run_infimum_capsule" => ifm::tool_run_infimum_capsule(default_proj, args),
        "list_infimum_repo" => super::repo_index::tool_list_infimum_repo(args),
        "read_infimum_repo" => super::repo_index::tool_read_infimum_repo(args),
        "search_infimum_repo" => super::repo_index::tool_search_infimum_repo(args),
        "search_pbr_textures" => ifm::tool_search_pbr_textures(args),
        "set_pbr_texture" => ifm::tool_set_pbr_texture(default_proj, args),

        // Git & Developer Mode tools
        "git_tool" => tool_git(args, default_proj, policy),
        "gh_cli" => tool_gh_cli(args, default_proj, policy),
        "wrangler_cli" => tool_wrangler_cli(args, default_proj, policy),
        "web_fetch" => tool_web_fetch(args, policy.is_dev_mode()),
        "web_search" => tool_web_search(args, policy.is_dev_mode()),

        _ => json!({ "error": format!("Unknown tool '{name}'") }),
    }
}
