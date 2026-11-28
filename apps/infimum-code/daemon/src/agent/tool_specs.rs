//! Single source of truth for agent tool declarations (Gemini & OpenAI-compat).
//!
//! Compact descriptions minimize per-turn prompt token overhead while keeping
//! parameter contracts crisp. Developer-only tools (`gh_cli`, `wrangler_cli`,
//! `web_fetch`, `web_search`) are only advertised when `dev_mode` is enabled.

use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ToolSpecOptions {
    pub dev_mode: bool,
    pub include_repo_snapshot: bool,
    pub include_pbr_tools: bool,
    pub include_cargo: bool,
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    /// OpenAI-style JSON schema (`type: "object"`, lowercase types).
    /// Converted automatically to Gemini uppercase types (`OBJECT`, `STRING`, ...).
    parameters: Value,
}

fn to_gemini_schema(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if k == "type" {
                    if let Some(s) = val.as_str() {
                        out.insert(k.clone(), Value::String(s.to_ascii_uppercase()));
                        continue;
                    }
                }
                out.insert(k.clone(), to_gemini_schema(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(to_gemini_schema).collect()),
        other => other.clone(),
    }
}

fn build_tool_specs(opts: ToolSpecOptions) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec {
            name: "list_project_files",
            description: "List files in the active project. Supports subdir, glob pattern (e.g. '*.rs', '**/*.ifm'), max_depth, and limit (default 200, max 500). Ignores target/, node_modules/, .git/.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "subdir": { "type": "string", "description": "Optional relative subdirectory" },
                    "pattern": { "type": "string", "description": "Optional glob filter (e.g. '*.rs', 'src/**/*.ifm')" },
                    "max_depth": { "type": "integer", "description": "Optional max directory depth" },
                    "limit": { "type": "integer", "description": "Max files to return (default 200, max 500)" },
                    "project_dir": { "type": "string", "description": "Alias for subdir" }
                }
            }),
        },
        ToolSpec {
            name: "search_project",
            description: "Fast code search (ripgrep/grep) across the active project. Supports path, glob filter, regex, case sensitivity, context lines, and modes ('matches', 'files', 'count').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search text or regex" },
                    "path": { "type": "string", "description": "Optional relative file or subdirectory" },
                    "glob": { "type": "string", "description": "Optional file glob filter (e.g. '*.rs', '*.ifm')" },
                    "is_regex": { "type": "boolean", "description": "Treat query as regex (default false = literal)" },
                    "case_sensitive": { "type": "boolean", "description": "Case-sensitive search (default false)" },
                    "context_lines": { "type": "integer", "description": "Context lines before/after match (0..5)" },
                    "mode": { "type": "string", "description": "'matches' (default), 'files' (paths only), or 'count'" },
                    "max_matches": { "type": "integer", "description": "Max matches (default 50, max 200)" },
                    "project_dir": { "type": "string", "description": "Alias for path" }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "get_file_outline",
            description: "Return compact declaration outline with line ranges for .ifm, .rs, .js, .mjs, .ts, .tsx files without reading full bodies.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "read_project_file",
            description: "Read a line range from a file (with line numbers by default). Prefer targeted ranges or get_symbol for large files.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" },
                    "with_line_numbers": { "type": "boolean", "description": "Prefix lines with 'N| ' (default true)" }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "get_symbol",
            description: "Fetch exact AST source and line range of a declaration (fn, struct, enum, forma, layout, const, impl, class, function) or method ('Type::method', 'Class::method') in .rs, .js, .mjs, .ts, .tsx, or .ifm.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "name": { "type": "string", "description": "Symbol name or 'Type::method' / 'Class::method'" }
                },
                "required": ["path", "name"]
            }),
        },
        ToolSpec {
            name: "replace_symbol",
            description: "Atomically replace a declaration or method ('Type::method', 'Class::method') by name using AST spans in .rs, .js, .mjs, .ts, .tsx, or .ifm, avoiding brittle whitespace matching.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "name": { "type": "string", "description": "Symbol name or 'Type::method' / 'Class::method'" },
                    "new_source": { "type": "string" }
                },
                "required": ["path", "name", "new_source"]
            }),
        },
        ToolSpec {
            name: "find_references",
            description: "Find references/callers of an identifier across .rs, .ifm, .js, .mjs, .ts, .tsx files with AST-resolved enclosing symbol names and line numbers.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string" },
                    "path": { "type": "string", "description": "Optional relative file or subdirectory" },
                    "glob": { "type": "string", "description": "Optional glob filter (e.g. '*.rs', '*.ifm')" },
                    "max_matches": { "type": "integer", "description": "Max matches (default 60, max 200)" }
                },
                "required": ["symbol"]
            }),
        },
        ToolSpec {
            name: "apply_patch",
            description: "Atomically apply a multi-file unified diff patch (create via '--- /dev/null', delete via '+++ /dev/null', or modify multiple hunks/files in one call). Validates all hunks before writing anything.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Standard unified diff ('--- a/path' / '+++ b/path' + '@@ ... @@' hunks)" },
                    "dry_run": { "type": "boolean", "description": "Validate patch without writing to disk (default false)" }
                },
                "required": ["patch"]
            }),
        },
        ToolSpec {
            name: "edit_project_file",
            description: "Surgically replace an exact target snippet in a file. Set replace_all=true to replace all occurrences. On mismatch, returns nearest similar lines.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "target": { "type": "string" },
                    "replacement": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "target", "replacement"]
            }),
        },
        ToolSpec {
            name: "batch_edit_project_file",
            description: "Atomically apply multiple independent exact target->replacement edits to one file in a single turn. Rolls back completely on any error.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": { "type": "string" },
                                "replacement": { "type": "string" }
                            },
                            "required": ["target", "replacement"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        },
        ToolSpec {
            name: "write_project_file",
            description: "Create or overwrite a file inside the active project (creates parent directories automatically).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "delete_project_file",
            description: "Safely delete a regular file inside the active project (blocks directories and sensitive secret files).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "move_project_file",
            description: "Rename or move a file inside the active project (creates destination parent directories).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string" },
                    "to": { "type": "string" }
                },
                "required": ["from", "to"]
            }),
        },
        ToolSpec {
            name: "read_infimum_guide",
            description: "Read the canonical Infimum Lang & infimum-web reference guide. Call before writing or compiling .ifm code.",
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolSpec {
            name: "read_infimum_std",
            description: "Read built-in Infimum std module (e.g. 'gfx.ifm', 'input.ifm', 'mesh3d.ifm', 'math.ifm') or 'list'.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string" }
                }
            }),
        },
        ToolSpec {
            name: "read_infimum_template",
            description: "Read a Z3-verified starter capsule template ('ui', 'game2d', 'game3d', or 'list').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string" }
                }
            }),
        },
        ToolSpec {
            name: "verify_infimum_file",
            description: "Verify an Infimum (.ifm) file with Z3 Telos/Flux/Forma contracts (`infimum verify`).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "compile_infimum_wasm",
            description: "Compile an Infimum (.ifm) file into a WebAssembly capsule (.ifw).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "output": { "type": "string", "description": "Optional output .ifw path (defaults to <path>.ifw)" }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "run_infimum_capsule",
            description: "Run a compiled .ifw capsule in native infimum-web. headless=true (default) runs smoke-check; headless=false opens GUI window.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "headless": { "type": "boolean" }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "git_tool",
            description: if opts.dev_mode {
                "Run safe structured git subcommands (status, diff, log, branch, show, add, commit, stash, checkout, switch, restore, tag, rev-parse, ls-files, blame, fetch, pull, push, remote) inside the workspace."
            } else {
                "Run safe local git subcommands (status, diff, log, branch, show, add, commit, stash, checkout, switch, restore, tag, rev-parse, ls-files, blame) inside the workspace."
            },
            parameters: json!({
                "type": "object",
                "properties": {
                    "subcommand": { "type": "string", "description": "Git subcommand (e.g. 'status', 'diff', 'log', 'commit')" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional arguments passed after the subcommand"
                    },
                    "cwd": { "type": "string", "description": "Optional relative subdirectory (e.g. 'infimum' or 'infimum-site')" }
                },
                "required": ["subcommand"]
            }),
        },
        ToolSpec {
            name: "run_project_command",
            description: "Run a local shell command inside the workspace with timeout and head+tail output truncation.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 60, max 300)" }
                },
                "required": ["command"]
            }),
        },
    ];

    if opts.include_pbr_tools {
        specs.push(ToolSpec {
            name: "search_pbr_textures",
            description: "Search 800+ free CC0 1K PBR textures for 3D capsules by keywords (e.g. 'brick', 'metal', 'wood').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            }),
        });
        specs.push(ToolSpec {
            name: "set_pbr_texture",
            description: "Bind a CC0 1K PBR texture ID to a 3D texture layer slot (0..11).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "slot": { "type": "integer" },
                    "texture_id": { "type": "string" }
                },
                "required": ["slot", "texture_id"]
            }),
        });
    }

    if opts.include_repo_snapshot {
        specs.push(ToolSpec {
            name: "list_infimum_repo",
            description: "List files in the bundled Infimum monorepo + website snapshot.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string" }
                }
            }),
        });
        specs.push(ToolSpec {
            name: "read_infimum_repo",
            description: "Read a file (1-based line range, up to 400 lines) from the bundled Infimum monorepo snapshot.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["path"]
            }),
        });
        specs.push(ToolSpec {
            name: "search_infimum_repo",
            description: "Search text across the bundled Infimum monorepo snapshot.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "prefix": { "type": "string" },
                    "case_sensitive": { "type": "boolean" },
                    "max_results": { "type": "integer" }
                },
                "required": ["query"]
            }),
        });
    }

    if opts.include_cargo {
        specs.push(ToolSpec {
            name: "cargo_check",
            description: "Run Rust `cargo check|test|clippy|build --message-format=json` and return compact grouped errors/warnings (`file:line:col [code] message + help`) and test summaries without noisy logs.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "'check' (default), 'test', 'clippy', or 'build'" },
                    "package": { "type": "string", "description": "Optional crate/package name (-p <pkg>)" },
                    "cwd": { "type": "string", "description": "Optional relative directory containing Cargo.toml (auto-detected if omitted)" },
                    "filter": { "type": "string", "description": "Optional test name filter (for action='test')" },
                    "release": { "type": "boolean", "description": "Build/check with --release (default false)" },
                    "max_diagnostics": { "type": "integer", "description": "Max errors/warnings per category (default 30, max 100)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 180, max 300)" }
                }
            }),
        });
    }

    if opts.dev_mode {
        specs.push(ToolSpec {
            name: "gh_cli",
            description: "Developer Mode: run GitHub CLI (`gh`) with args (e.g. ['pr', 'list'], ['run', 'view', '--log-failed'], ['issue', 'list'], ['release', 'list']).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments passed to `gh`"
                    },
                    "cwd": { "type": "string", "description": "Optional relative subdirectory" }
                },
                "required": ["args"]
            }),
        });
        specs.push(ToolSpec {
            name: "wrangler_cli",
            description: "Developer Mode: run Cloudflare `wrangler` CLI with args (e.g. ['whoami'], ['deployments', 'list'], ['d1', 'list'], ['deploy']).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments passed to `wrangler`"
                    },
                    "cwd": { "type": "string", "description": "Optional relative subdirectory (e.g. 'infimum-site')" }
                },
                "required": ["args"]
            }),
        });
        specs.push(ToolSpec {
            name: "web_fetch",
            description: "Developer Mode: fetch an HTTP(S) URL (with SSRF protection). Converts HTML to clean compact text by default (mode='text') or returns raw text/JSON (mode='raw').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_chars": { "type": "integer", "description": "Max output chars (default 6000, max 20000)" },
                    "mode": { "type": "string", "description": "'text' (default, strips HTML) or 'raw'" }
                },
                "required": ["url"]
            }),
        });
        specs.push(ToolSpec {
            name: "web_search",
            description: "Developer Mode: search the web via Gemini Search grounding and return a concise answer with verified source URLs.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "description": "Max source links (default 5, max 10)" }
                },
                "required": ["query"]
            }),
        });
    }

    specs
}

pub(super) fn detect_tool_options(proj_dir: &std::path::Path, dev_mode: bool) -> ToolSpecOptions {
    // If the active project directory already contains the live Infimum monorepo on disk,
    // omit the redundant bundled repo-snapshot tools to save per-turn prompt tokens.
    let is_local_monorepo = proj_dir.join("crates/infimum-lang/Cargo.toml").is_file()
        || proj_dir.join("infimum/crates/infimum-lang/Cargo.toml").is_file();
    let include_cargo = super::tools::has_cargo_project(proj_dir);
    ToolSpecOptions {
        dev_mode,
        include_repo_snapshot: !is_local_monorepo,
        include_pbr_tools: !is_local_monorepo || !dev_mode,
        include_cargo,
    }
}

pub(super) fn gemini_tool_declarations(opts: ToolSpecOptions) -> Value {
    let decls: Vec<Value> = build_tool_specs(opts)
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "parameters": to_gemini_schema(&s.parameters),
            })
        })
        .collect();
    json!({ "functionDeclarations": decls })
}

pub(super) fn openai_tool_declarations(opts: ToolSpecOptions) -> Value {
    let tools: Vec<Value> = build_tool_specs(opts)
        .into_iter()
        .map(|s| {
            json!({
                "type": "function",
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.parameters,
                }
            })
        })
        .collect();
    Value::Array(tools)
}
