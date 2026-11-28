use crate::studio_abi::{STEP_KIND_CMD, STEP_KIND_TOOL};
use serde_json::Value;

pub(in crate::agent) fn describe_tool_ru(name: &str, args: &Value) -> String {
    let short_path = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| {
                s.rsplit('/')
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default()
    };
    match name {
        "list_project_files" => {
            let s = short_path("subdir");
            let lbl = if s.is_empty() {
                short_path("project_dir")
            } else {
                s
            };
            format!("Смотрю файлы {lbl}…")
        }
        "search_project" => format!(
            "Ищу «{}»…",
            args.get("query").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "get_file_outline" => format!("Сканирую структуру {}…", short_path("path")),
        "read_project_file" => format!("Читаю {}…", short_path("path")),
        "edit_project_file" => format!("Редактирую {}…", short_path("path")),
        "batch_edit_project_file" => format!("Пакетно редактирую {}…", short_path("path")),
        "apply_patch" => "Применяю unified-diff патч…".to_string(),
        "write_project_file" => format!("Записываю {}…", short_path("path")),
        "delete_project_file" => format!("Удаляю {}…", short_path("path")),
        "move_project_file" => format!("Перемещаю {} → {}…", short_path("from"), short_path("to")),
        "verify_infimum_file" => format!("Проверяю Z3/Strata {}…", short_path("path")),
        "compile_infimum_wasm" => format!("Компилирую .ifw {}…", short_path("path")),
        "run_infimum_capsule" => format!("Запускаю капсулу {}…", short_path("path")),
        "read_infimum_guide" => "Читаю справочник Infimum Lang…".to_string(),
        "read_infimum_std" => {
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("list");
            format!("Читаю std/{f}…")
        }
        "get_symbol" | "get_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Читаю символ {sym}…")
        }
        "replace_symbol" | "replace_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Обновляю символ {sym}…")
        }
        "find_references" | "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Ищу ссылки на {sym}…")
        }
        "cargo_check" => {
            let act = args.get("action").and_then(|v| v.as_str()).unwrap_or("check");
            let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
            if pkg.is_empty() {
                format!("cargo {act}…")
            } else {
                format!("cargo {act} -p {pkg}…")
            }
        }
        "git_tool" => {
            let sub = args.get("subcommand").and_then(|v| v.as_str()).unwrap_or("");
            format!("git {sub}…")
        }
        "gh_cli" => "Выполняю GitHub CLI (gh)…".to_string(),
        "wrangler_cli" => "Выполняю Cloudflare Wrangler…".to_string(),
        "web_fetch" => format!(
            "Загружаю URL {}…",
            args.get("url").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "web_search" => format!(
            "Ищу в сети «{}»…",
            args.get("query").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "read_infimum_template" => {
            let k = args.get("kind").and_then(|v| v.as_str()).unwrap_or("list");
            format!("Читаю шаблон {k}…")
        }
        "list_infimum_repo" => {
            let p = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("/");
            format!("Смотрю репозиторий {p}…")
        }
        "read_infimum_repo" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Читаю из репо {p}…")
        }
        "search_infimum_repo" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Ищу в репо «{q}»…")
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Ищу PBR-текстуры «{q}»…")
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = args.get("texture_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Привязываю текстуру {id} → слот {slot}…")
        }
        "run_project_command" => {
            let c: String = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            format!("Выполняю: {c}…")
        }
        _ => format!("Инструмент {name}…"),
    }
}

pub(in crate::agent) fn short_path_label(raw: &str) -> String {
    let p = raw.trim();
    if p.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        p.to_string()
    } else {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

pub(in crate::agent) fn describe_tool_step(name: &str, args: &Value) -> (i64, String, String) {
    match name {
        "list_project_files" => {
            let d = args
                .get("subdir")
                .or_else(|| args.get("project_dir"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            (STEP_KIND_TOOL, "Список файлов проекта".to_string(), short_path_label(d))
        }
        "search_project" => {
            let q = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Поиск по коду".to_string(), q.chars().take(80).collect())
        }
        "get_file_outline" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Структура файла".to_string(), short_path_label(p))
        }
        "read_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let s = args.get("start_line").and_then(|v| v.as_i64()).unwrap_or(1);
            let e = args.get("end_line").and_then(|v| v.as_i64()).unwrap_or(s + 100);
            (
                STEP_KIND_TOOL,
                "Чтение файла".to_string(),
                format!("{} (L{}..{})", short_path_label(p), s, e),
            )
        }
        "edit_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Редактирование файла".to_string(), short_path_label(p))
        }
        "batch_edit_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let n = args.get("edits").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            (STEP_KIND_TOOL, format!("Пакетная правка ({n})"), short_path_label(p))
        }
        "apply_patch" => {
            let patch = args.get("patch").and_then(|s| s.as_str()).unwrap_or("");
            let files: Vec<String> = patch
                .lines()
                .filter_map(|l| l.strip_prefix("+++ "))
                .map(|s| s.split('\t').next().unwrap_or(s).trim())
                .filter(|s| *s != "/dev/null")
                .map(|s| short_path_label(s.strip_prefix("b/").unwrap_or(s)))
                .take(3)
                .collect();
            let detail = if files.is_empty() {
                "unified diff".to_string()
            } else {
                files.join(", ")
            };
            (STEP_KIND_TOOL, "Патч (apply_patch)".to_string(), detail)
        }
        "write_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Запись файла".to_string(), short_path_label(p))
        }
        "verify_infimum_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Верификация Z3".to_string(), short_path_label(p))
        }
        "compile_infimum_wasm" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Компиляция .ifm → .ifw".to_string(), short_path_label(p))
        }
        "run_infimum_capsule" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let headless = args.get("headless").and_then(|v| v.as_bool()).unwrap_or(true);
            let title = if headless {
                "Проверка капсулы (headless)"
            } else {
                "Запуск окна infimum-web"
            };
            (STEP_KIND_TOOL, title.to_string(), short_path_label(p))
        }
        "read_infimum_guide" => (
            STEP_KIND_TOOL,
            "Справочник Infimum Lang".to_string(),
            "docs/INFIMUM_LANG_GUIDE.md".to_string(),
        ),
        "read_infimum_std" => {
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("list");
            (
                STEP_KIND_TOOL,
                "Стандартная библиотека".to_string(),
                format!("std/{}", f.trim_start_matches("std/")),
            )
        }
        "get_symbol" | "get_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, format!("Символ {sym}"), short_path_label(p))
        }
        "replace_symbol" | "replace_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, format!("Замена {sym}"), short_path_label(p))
        }
        "find_references" | "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Поиск ссылок".to_string(), sym.to_string())
        }
        "cargo_check" => {
            let act = args.get("action").and_then(|v| v.as_str()).unwrap_or("check");
            let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_CMD,
                format!("cargo {act}"),
                if pkg.is_empty() {
                    short_path_label(args.get("cwd").and_then(|v| v.as_str()).unwrap_or(""))
                } else {
                    format!("-p {pkg}")
                },
            )
        }
        "read_infimum_template" => {
            let k = args.get("kind").and_then(|v| v.as_str()).unwrap_or("list");
            (STEP_KIND_TOOL, "Шаблон Infimum".to_string(), k.to_string())
        }
        "list_infimum_repo" => {
            let p = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("/");
            (STEP_KIND_TOOL, "Репозиторий Infimum".to_string(), p.to_string())
        }
        "read_infimum_repo" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            (STEP_KIND_TOOL, "Файл из репо".to_string(), p.to_string())
        }
        "search_infimum_repo" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Поиск в репо".to_string(), q.to_string())
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Поиск PBR-текстур".to_string(), q.to_string())
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = args.get("texture_id").and_then(|v| v.as_str()).unwrap_or("?");
            (STEP_KIND_TOOL, "PBR-текстура".to_string(), format!("slot {slot} -> {id}"))
        }
        "delete_project_file" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Удалить файл".to_string(), short_path_label(p))
        }
        "move_project_file" => {
            let from = args.get("from").and_then(|v| v.as_str()).unwrap_or("");
            let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                "Переместить файл".to_string(),
                format!("{} -> {}", short_path_label(from), short_path_label(to)),
            )
        }
        "git_tool" => {
            let sub = args.get("subcommand").and_then(|v| v.as_str()).unwrap_or("");
            let cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_CMD, format!("git {sub}"), short_path_label(cwd))
        }
        "gh_cli" => {
            let cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_CMD, "GitHub CLI".to_string(), short_path_label(cwd))
        }
        "wrangler_cli" => {
            let cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            (STEP_KIND_CMD, "Wrangler CLI".to_string(), short_path_label(cwd))
        }
        "web_fetch" => (
            STEP_KIND_TOOL,
            "Web Fetch".to_string(),
            args.get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect(),
        ),
        "web_search" => (
            STEP_KIND_TOOL,
            "Web Search".to_string(),
            args.get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect(),
        ),
        "run_project_command" => {
            let cmd = args.get("command").and_then(|s| s.as_str()).unwrap_or("");
            (
                STEP_KIND_CMD,
                "Команда терминала".to_string(),
                format!("$ {}", cmd.chars().take(120).collect::<String>()),
            )
        }
        _ => (STEP_KIND_TOOL, format!("Инструмент {name}"), String::new()),
    }
}
