use crate::ast::{
    BinOp, ConstDef, ConstValue, EffectCap, EnumDef, EnumVariant, Expr, FnDef, LayoutDef,
    LayoutField, Module, Param, Stmt, TargetRef, Type, UnaryOp,
};
use crate::diag::{InfimumDiagnostic, Span};
use crate::lexer::{tokenize, Token, TokenKind};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Internal Raw AST (pre-resolution / pre-desugaring)
// ---------------------------------------------------------------------------

struct RawUse {
    path: String,
    span: Span,
}

struct RawModFile {
    name: String,
    path: String,
    span: Span,
}

struct RawMod {
    name: String,
    items: Vec<RawItem>,
    formas: Vec<crate::forma::FormaDef<RawExpr>>,
    functions: Vec<RawFnDef>,
}

struct RawConst {
    name: String,
    is_pub: bool,
    ty: Type,
    expr: RawExpr,
    span: Span,
}

struct RawEnumVariant {
    name: String,
    value: Option<RawExpr>,
    span: Span,
}

struct RawEnum {
    name: String,
    variants: Vec<RawEnumVariant>,
    span: Span,
}

struct RawInv {
    lo: RawExpr,
    hi: RawExpr,
    inclusive: bool,
    span: Span,
}

struct RawStruct {
    name: String,
    fields: Vec<(String, bool, Option<RawInv>, Span)>,
    span: Span,
}

#[derive(Clone)]
struct StructDef {
    fields: BTreeMap<String, i64>,
    field_invs: BTreeMap<String, (i64, i64)>,
    field_fixed: BTreeSet<String>,
    field_names: Vec<String>,
    size: i64,
}

#[derive(Clone)]
struct LayoutFieldMeta {
    offset: i64,
    is_array: bool,
    is_fixed: bool,
    struct_type: Option<String>,
    stride: i64,
    count: i64,
    inv: Option<(i64, i64)>,
}

struct RawLayoutField {
    name: String,
    struct_type: Option<String>,
    len_expr: Option<RawExpr>,
    is_array: bool,
    is_fixed: bool,
    at_expr: Option<RawExpr>,
    inv: Option<RawInv>,
    span: Span,
}

struct RawLayout {
    name: String,
    base_expr: Option<RawExpr>,
    fields: Vec<RawLayoutField>,
    span: Span,
}

enum RawItem {
    Const(RawConst),
    Enum(RawEnum),
    Struct(RawStruct),
    Layout(RawLayout),
    Mod(RawMod),
}

#[derive(Clone)]
pub(crate) enum RawMatchPattern {
    Wildcard(Span),
    Int(i64),
    Bool(bool),
    ConstRef(String, Span),
    Path2(String, String, Span),
    Path3(String, String, String, Span),
}

#[derive(Clone)]
pub(crate) struct RawMatchArm {
    patterns: Vec<RawMatchPattern>,
    body: RawExpr,
}

#[derive(Clone)]
pub(crate) enum RawType {
    Builtin(Type),
    Named(String, Span),
    Secret(Box<RawType>),
    Tuple(Vec<RawType>),
}

#[derive(Clone)]
pub(crate) enum RawStmt {
    Let {
        name: String,
        mutable: bool,
        ty: Option<RawType>,
        value: RawExpr,
    },
    LetTuple {
        names: Vec<(String, bool)>,
        value: RawExpr,
    },
    Assign {
        name: String,
        op: Option<BinOp>,
        value: RawExpr,
    },
    LayoutStore {
        layout: String,
        field: String,
        index: Option<RawExpr>,
        subfield: Option<String>,
        op: Option<BinOp>,
        value: RawExpr,
        span: Span,
    },
    While {
        cond: RawExpr,
        max_iters: RawExpr,
        body: Vec<RawStmt>,
        span: Span,
    },
    For {
        var: String,
        start: RawExpr,
        end: RawExpr,
        inclusive: bool,
        max_iters: Option<RawExpr>,
        body: Vec<RawStmt>,
        span: Span,
    },
    Break(Span),
    Continue(Span),
    Expr(RawExpr),
}

#[derive(Clone)]
pub(crate) enum RawExpr {
    IntLit(i64),
    FixedLit(i64),
    BoolLit(bool),
    StrLit(String),
    UnitLit,
    Var(String),
    Path2 {
        head: String,
        member: String,
        span: Span,
    },
    Path3 {
        head: String,
        member: String,
        prop: String,
        span: Span,
    },
    LayoutLoad {
        layout: String,
        field: String,
        index: Option<Box<RawExpr>>,
        subfield: Option<String>,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        expr: Box<RawExpr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<RawExpr>,
        rhs: Box<RawExpr>,
    },
    If {
        cond: Box<RawExpr>,
        then_branch: Box<RawExpr>,
        else_branch: Box<RawExpr>,
    },
    Match {
        scrutinee: Box<RawExpr>,
        arms: Vec<RawMatchArm>,
        span: Span,
    },
    Call {
        target: TargetRef,
        args: Vec<RawExpr>,
    },
    ModCall {
        module: String,
        func: String,
        args: Vec<RawExpr>,
        span: Span,
    },
    Perform {
        effect: String,
        op: String,
        args: Vec<RawExpr>,
    },
    MemLoad {
        index: Box<RawExpr>,
    },
    MemStore {
        index: Box<RawExpr>,
        value: Box<RawExpr>,
    },
    SecretWrap(Box<RawExpr>),
    Reveal {
        domain: String,
        expr: Box<RawExpr>,
    },
    Block {
        stmts: Vec<RawStmt>,
        tail: Box<RawExpr>,
    },
    TupleLit(Vec<RawExpr>),
    TupleField {
        expr: Box<RawExpr>,
        index: usize,
    },
    StructLit {
        name: String,
        fields: Vec<(String, RawExpr, Span)>,
        span: Span,
    },
    FieldAccess {
        expr: Box<RawExpr>,
        field: String,
        span: Span,
    },
}

struct RawFnDef {
    name: String,
    exported: bool,
    is_pub: bool,
    module: Option<String>,
    test_label: Option<String>,
    params: Vec<(String, RawType)>,
    effects: Vec<EffectCap>,
    ret_type: RawType,
    requires: Vec<RawExpr>,
    ensures: Vec<RawExpr>,
    body: RawExpr,
    source_file: Option<String>,
    span: Span,
}

struct RawFile {
    uses: Vec<RawUse>,
    mod_files: Vec<RawModFile>,
    items: Vec<RawItem>,
    formas: Vec<crate::forma::FormaDef<RawExpr>>,
    functions: Vec<RawFnDef>,
}

// ---------------------------------------------------------------------------
// Public Entry Points
// ---------------------------------------------------------------------------

fn resolve_embedded_std(spec: &str) -> Option<(&'static str, &'static str)> {
    match spec {
        "std/math" | "std/math.ifm" => Some(("__std__/math.ifm", include_str!("../std/math.ifm"))),
        "std/color" | "std/color.ifm" => Some(("__std__/color.ifm", include_str!("../std/color.ifm"))),
        "std/sys_mem" | "std/sys_mem.ifm" => {
            Some(("__std__/sys_mem.ifm", include_str!("../std/sys_mem.ifm")))
        }
        "std/mem" | "std/mem.ifm" => Some(("__std__/mem.ifm", include_str!("../std/mem.ifm"))),
        "std/gfx" | "std/gfx.ifm" => Some(("__std__/gfx.ifm", include_str!("../std/gfx.ifm"))),
        "std/mesh3d" | "std/mesh3d.ifm" => Some(("__std__/mesh3d.ifm", include_str!("../std/mesh3d.ifm"))),
        "std/text" | "std/text.ifm" => Some(("__std__/text.ifm", include_str!("../std/text.ifm"))),
        "std/audio" | "std/audio.ifm" => Some(("__std__/audio.ifm", include_str!("../std/audio.ifm"))),
        "std/input" | "std/input.ifm" => Some(("__std__/input.ifm", include_str!("../std/input.ifm"))),
        "std/fmt" | "std/fmt.ifm" => Some(("__std__/fmt.ifm", include_str!("../std/fmt.ifm"))),
        "std/theme" | "std/theme.ifm" => Some(("__std__/theme.ifm", include_str!("../std/theme.ifm"))),
        "std/icons" | "std/icons.ifm" => Some(("__std__/icons.ifm", include_str!("../std/icons.ifm"))),
        "std/ui" | "std/ui.ifm" => Some(("__std__/ui.ifm", include_str!("../std/ui.ifm"))),
        "std/text_edit" | "std/text_edit.ifm" => {
            Some(("__std__/text_edit.ifm", include_str!("../std/text_edit.ifm")))
        }
        "std/markdown" | "std/markdown.ifm" => {
            Some(("__std__/markdown.ifm", include_str!("../std/markdown.ifm")))
        }
        "std/buf" | "std/buf.ifm" => Some(("__std__/buf.ifm", include_str!("../std/buf.ifm"))),
        "std/json" | "std/json.ifm" => Some(("__std__/json.ifm", include_str!("../std/json.ifm"))),
        "std/prelude" | "std/prelude.ifm" => {
            Some(("__std__/prelude.ifm", include_str!("../std/prelude.ifm")))
        }
        _ => None,
    }
}

fn tag_raw_items_source(items: &mut [RawItem], label: &str) {
    for item in items {
        if let RawItem::Mod(rm) = item {
            for f in &mut rm.functions {
                if f.source_file.is_none() {
                    f.source_file = Some(label.to_string());
                }
            }
            tag_raw_items_source(&mut rm.items, label);
        }
    }
}

fn tag_raw_file_source(raw: &mut RawFile, label: &str) {
    for f in &mut raw.functions {
        if f.source_file.is_none() {
            f.source_file = Some(label.to_string());
        }
    }
    tag_raw_items_source(&mut raw.items, label);
}

fn diag_with_file(diag: InfimumDiagnostic, file_label: &str) -> InfimumDiagnostic {
    if diag.message.starts_with(file_label) {
        diag
    } else {
        let mut d = InfimumDiagnostic::new(
            diag.code.clone(),
            diag.phase.clone(),
            format!("{file_label}: {}", diag.message),
            diag.span,
            diag.repair_hint.clone(),
        );
        if !diag.counterexample.is_empty() {
            d = d.with_counterexample(diag.counterexample.clone());
        }
        d
    }
}

fn load_std_recursive(
    spec: &str,
    use_span: Span,
    visiting: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    all_items: &mut Vec<RawItem>,
    all_formas: &mut Vec<crate::forma::FormaDef<RawExpr>>,
    all_fns: &mut Vec<RawFnDef>,
) -> Result<(), InfimumDiagnostic> {
    let Some((virtual_key, src)) = resolve_embedded_std(spec) else {
        return Err(InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Unknown built-in Infimum stdlib module '{spec}'"),
            use_span,
            "Available stdlib modules: std/prelude.ifm, std/sys_mem.ifm, std/mem.ifm, std/math.ifm, std/color.ifm, std/gfx.ifm, std/input.ifm, std/text.ifm, std/audio.ifm, std/fmt.ifm, std/theme.ifm, std/icons.ifm, std/ui.ifm, std/text_edit.ifm, std/markdown.ifm, std/buf.ifm, std/json.ifm",
        ));
    };
    let canon = PathBuf::from(virtual_key);
    if visiting.iter().any(|p| p == &canon) {
        return Err(InfimumDiagnostic::new(
            "E019_IMPORT_CYCLE",
            "parse",
            format!("Circular stdlib import detected: {virtual_key}"),
            use_span,
            "Break the import cycle",
        ));
    }
    if visited.contains(&canon) {
        return Ok(());
    }
    let display_label = format!("std/{}", virtual_key.trim_start_matches("__std__/"));
    let mut raw = parse_raw_file(src).map_err(|d| diag_with_file(d, &display_label))?;
    tag_raw_file_source(&mut raw, &display_label);
    visiting.push(canon.clone());
    for u in &raw.uses {
        load_std_recursive(
            &u.path,
            u.span,
            visiting,
            visited,
            all_items,
            all_formas,
            all_fns,
        )?;
    }
    visiting.pop();
    visited.insert(canon);

    all_items.extend(raw.items);
    all_formas.extend(raw.formas);
    all_fns.extend(raw.functions);
    Ok(())
}

pub fn parse_module(src: &str) -> Result<Module, InfimumDiagnostic> {
    let raw = parse_raw_file(src)?;
    if raw.uses.is_empty() {
        return resolve_and_lower(raw.items, raw.formas, raw.functions);
    }
    let mut visiting: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut all_items: Vec<RawItem> = Vec::new();
    let mut all_formas: Vec<crate::forma::FormaDef<RawExpr>> = Vec::new();
    let mut all_fns: Vec<RawFnDef> = Vec::new();
    for u in &raw.uses {
        if u.path.starts_with("std/") {
            load_std_recursive(
                &u.path,
                u.span,
                &mut visiting,
                &mut visited,
                &mut all_items,
                &mut all_formas,
                &mut all_fns,
            )?;
        } else {
            return Err(InfimumDiagnostic::new(
                "E018_USE_WITHOUT_PATH",
                "parse",
                format!(
                    "Cannot resolve 'use \"{}\"' without a source file path",
                    u.path
                ),
                u.span,
                "Load multi-file modules with parse_module_from_path(entry_path) or CLI file commands",
            ));
        }
    }
    all_items.extend(raw.items);
    all_formas.extend(raw.formas);
    all_fns.extend(raw.functions);
    resolve_and_lower(all_items, all_formas, all_fns)
}

/// Load and parse a multi-file Infimum module starting at `entry_path`.
///
/// Resolves `use "relative/path.ifm";` directives recursively relative to each file's directory,
/// deduplicates diamond imports by canonical path, detects import cycles, and collects all
/// declarations in deterministic topological order (dependencies before dependents).
pub fn parse_module_from_path(entry_path: &Path) -> Result<Module, InfimumDiagnostic> {
    let mut visiting: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut all_items: Vec<RawItem> = Vec::new();
    let mut all_formas: Vec<crate::forma::FormaDef<RawExpr>> = Vec::new();
    let mut all_fns: Vec<RawFnDef> = Vec::new();
    let entry_dir = fs::canonicalize(entry_path)
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    load_file_recursive(
        entry_path,
        entry_dir.as_deref(),
        Span::zero(),
        &mut visiting,
        &mut visited,
        &mut all_items,
        &mut all_formas,
        &mut all_fns,
    )?;

    resolve_and_lower(all_items, all_formas, all_fns)
}

fn normalize_virtual_rel(base_dir: &str, rel: &str) -> String {
    let raw = if base_dir.is_empty() || rel.starts_with("std/") {
        rel.to_string()
    } else {
        format!("{base_dir}/{rel}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn virtual_parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Load and parse a multi-file Infimum module from an in-memory virtual filesystem
/// (used by Online Infimum Code / `wasm32`) while preserving per-file names and spans.
pub fn parse_module_from_virtual_fs(
    entry_path: &str,
    files: &BTreeMap<String, String>,
) -> Result<Module, InfimumDiagnostic> {
    let mut visiting: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut all_items: Vec<RawItem> = Vec::new();
    let mut all_formas: Vec<crate::forma::FormaDef<RawExpr>> = Vec::new();
    let mut all_fns: Vec<RawFnDef> = Vec::new();

    load_virtual_recursive(
        entry_path,
        Span::zero(),
        files,
        &mut visiting,
        &mut visited,
        &mut all_items,
        &mut all_formas,
        &mut all_fns,
    )?;

    resolve_and_lower(all_items, all_formas, all_fns)
}

#[allow(clippy::too_many_arguments)]
fn load_virtual_recursive(
    cur_path: &str,
    use_span: Span,
    files: &BTreeMap<String, String>,
    visiting: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    all_items: &mut Vec<RawItem>,
    all_formas: &mut Vec<crate::forma::FormaDef<RawExpr>>,
    all_fns: &mut Vec<RawFnDef>,
) -> Result<(), InfimumDiagnostic> {
    if cur_path.starts_with("std/") {
        return load_std_recursive(
            cur_path, use_span, visiting, visited, all_items, all_formas, all_fns,
        );
    }
    let key = normalize_virtual_rel("", cur_path);
    if !files.contains_key(&key) {
        if let Some(std_tail) = key.split("std/").nth(1) {
            let spec = format!("std/{std_tail}");
            if resolve_embedded_std(&spec).is_some() {
                return load_std_recursive(
                    &spec, use_span, visiting, visited, all_items, all_formas, all_fns,
                );
            }
        }
    }
    let Some(src) = files.get(&key) else {
        return Err(InfimumDiagnostic::new(
            "E010_IMPORT_NOT_FOUND",
            "parse",
            format!("Cannot resolve `use \"{cur_path}\"` in virtual project"),
            use_span,
            "Check that the imported .ifm file exists in the project or std/",
        ));
    };

    let canon = PathBuf::from(&key);
    if let Some(cycle_start) = visiting.iter().position(|p| p == &canon) {
        let mut chain: Vec<String> = visiting[cycle_start..]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        chain.push(key.clone());
        return Err(InfimumDiagnostic::new(
            "E019_IMPORT_CYCLE",
            "parse",
            format!("Circular 'use' import detected: {}", chain.join(" -> ")),
            use_span,
            "Break the import cycle by moving shared declarations into a common leaf .ifm file",
        ));
    }
    if visited.contains(&canon) {
        return Ok(());
    }

    let mut raw = parse_raw_file(src).map_err(|d| diag_with_file(d, &key))?;
    tag_raw_file_source(&mut raw, &key);
    let base_dir = virtual_parent_dir(&key);

    visiting.push(canon.clone());
    for u in &raw.uses {
        if u.path.starts_with("std/") {
            load_std_recursive(
                &u.path, u.span, visiting, visited, all_items, all_formas, all_fns,
            )?;
        } else if let Some(std_tail) = u.path.split("/std/").nth(1) {
            let spec = format!("std/{std_tail}");
            load_std_recursive(
                &spec, u.span, visiting, visited, all_items, all_formas, all_fns,
            )?;
        } else {
            let dep_key = normalize_virtual_rel(base_dir, &u.path);
            load_virtual_recursive(
                &dep_key, u.span, files, visiting, visited, all_items, all_formas, all_fns,
            )?;
        }
    }
    for mf in &raw.mod_files {
        let dep_key = normalize_virtual_rel(base_dir, &mf.path);
        let mut mod_items = Vec::new();
        let mut mod_formas = Vec::new();
        let mut mod_fns = Vec::new();
        load_virtual_recursive(
            &dep_key,
            mf.span,
            files,
            visiting,
            visited,
            &mut mod_items,
            &mut mod_formas,
            &mut mod_fns,
        )?;
        all_items.push(RawItem::Mod(RawMod {
            name: mf.name.clone(),
            items: mod_items,
            formas: mod_formas,
            functions: mod_fns,
        }));
    }
    visiting.pop();
    visited.insert(canon);

    all_items.extend(raw.items);
    all_formas.extend(raw.formas);
    all_fns.extend(raw.functions);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn load_file_recursive(
    path: &Path,
    entry_dir: Option<&Path>,
    use_span: Span,
    visiting: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    all_items: &mut Vec<RawItem>,
    all_formas: &mut Vec<crate::forma::FormaDef<RawExpr>>,
    all_fns: &mut Vec<RawFnDef>,
) -> Result<(), InfimumDiagnostic> {
    let canon = fs::canonicalize(path).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Cannot resolve Infimum source file '{}': {e}", path.display()),
            use_span,
            "Provide a valid path to an .ifm source file",
        )
    })?;

    if let Some(cycle_start) = visiting.iter().position(|p| p == &canon) {
        let mut chain: Vec<String> = visiting[cycle_start..]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        chain.push(canon.display().to_string());
        return Err(InfimumDiagnostic::new(
            "E019_IMPORT_CYCLE",
            "parse",
            format!("Circular 'use' import detected: {}", chain.join(" -> ")),
            use_span,
            "Break the import cycle by moving shared declarations into a common leaf .ifm file",
        ));
    }

    if visited.contains(&canon) {
        return Ok(());
    }

    let src = fs::read_to_string(&canon).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Cannot read '{}': {e}", canon.display()),
            use_span,
            "Check file permissions and ensure the .ifm file is valid UTF-8",
        )
    })?;

    let display_label = entry_dir
        .and_then(|d| canon.strip_prefix(d).ok())
        .map(|p| p.display().to_string())
        .or_else(|| path.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_else(|| canon.display().to_string());

    let mut raw = parse_raw_file(&src).map_err(|d| diag_with_file(d, &display_label))?;
    tag_raw_file_source(&mut raw, &display_label);
    let base_dir = canon.parent().unwrap_or_else(|| Path::new("."));

    visiting.push(canon.clone());
    for u in &raw.uses {
        if u.path.starts_with("std/") {
            load_std_recursive(
                &u.path,
                u.span,
                visiting,
                visited,
                all_items,
                all_formas,
                all_fns,
            )?;
        } else {
            let dep_path = base_dir.join(&u.path);
            load_file_recursive(
                &dep_path,
                entry_dir,
                u.span,
                visiting,
                visited,
                all_items,
                all_formas,
                all_fns,
            )?;
        }
    }
    for mf in &raw.mod_files {
        let dep_path = base_dir.join(&mf.path);
        let mut mod_items = Vec::new();
        let mut mod_formas = Vec::new();
        let mut mod_fns = Vec::new();
        load_file_recursive(
            &dep_path,
            entry_dir,
            mf.span,
            visiting,
            visited,
            &mut mod_items,
            &mut mod_formas,
            &mut mod_fns,
        )?;
        all_items.push(RawItem::Mod(RawMod {
            name: mf.name.clone(),
            items: mod_items,
            formas: mod_formas,
            functions: mod_fns,
        }));
    }
    visiting.pop();
    visited.insert(canon);

    all_items.extend(raw.items);
    all_formas.extend(raw.formas);
    all_fns.extend(raw.functions);
    Ok(())
}

fn parse_raw_file(src: &str) -> Result<RawFile, InfimumDiagnostic> {
    let tokens = tokenize(src)?;
    let mut parser = Parser { tokens, idx: 0 };
    let mut uses = Vec::new();
    let mut mod_files = Vec::new();
    let mut items = Vec::new();
    let mut formas = Vec::new();
    let mut functions = Vec::new();

    while !parser.at_eof() {
        let is_pub = if parser.peek_is_contextual_kw("pub") {
            parser.advance();
            true
        } else {
            false
        };
        match parser.peek().kind {
            TokenKind::Use => uses.push(parser.parse_use()?),
            TokenKind::Const => {
                let mut c = parser.parse_const()?;
                c.is_pub = is_pub;
                items.push(RawItem::Const(c));
            }
            TokenKind::Enum => items.push(RawItem::Enum(parser.parse_enum()?)),
            TokenKind::Layout => items.push(RawItem::Layout(parser.parse_layout()?)),
            _ if parser.peek_is_contextual_kw("mod") => {
                match parser.parse_mod()? {
                    ParsedMod::Inline(rm) => items.push(RawItem::Mod(rm)),
                    ParsedMod::File(mf) => mod_files.push(mf),
                }
            }
            _ if parser.peek_is_contextual_kw("struct") => {
                items.push(RawItem::Struct(parser.parse_struct()?))
            }
            _ if parser.peek_is_contextual_kw("test") => {
                functions.push(parser.parse_test_fn()?)
            }
            TokenKind::Forma => {
                formas.push(crate::forma::parser::parse_forma_def(&mut parser, false)?)
            }
            TokenKind::Export if matches!(parser.peek_next(), TokenKind::Forma) => {
                parser.advance();
                formas.push(crate::forma::parser::parse_forma_def(&mut parser, true)?)
            }
            _ => {
                let mut f = parser.parse_fn()?;
                f.is_pub = is_pub;
                functions.push(f);
            }
        }
    }

    Ok(RawFile {
        uses,
        mod_files,
        items,
        formas,
        functions,
    })
}

enum ParsedMod {
    Inline(RawMod),
    File(RawModFile),
}

// ---------------------------------------------------------------------------
// Recursive-Descent Parser
// ---------------------------------------------------------------------------

pub struct Parser {
    tokens: Vec<Token>,
    idx: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.idx]
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        let pos = self.idx + offset;
        if pos < self.tokens.len() {
            &self.tokens[pos].kind
        } else {
            &TokenKind::Eof
        }
    }

    fn peek_next(&self) -> &TokenKind {
        self.peek_at(1)
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.idx].clone();
        if self.idx + 1 < self.tokens.len() {
            self.idx += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &TokenKind, hint: &str) -> Result<Token, InfimumDiagnostic> {
        if &self.peek().kind == expected {
            Ok(self.advance())
        } else {
            Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Expected {:?}, found {:?}", expected, self.peek().kind),
                self.peek().span,
                hint,
            ))
        }
    }

    fn expect_ident(&mut self, hint: &str) -> Result<(String, Span), InfimumDiagnostic> {
        let tok = self.peek().clone();
        if let TokenKind::Ident(name) = tok.kind {
            self.advance();
            Ok((name, tok.span))
        } else {
            Err(InfimumDiagnostic::new(
                "E011_EXPECTED_IDENT",
                "parse",
                format!("Expected identifier, found {:?}", tok.kind),
                tok.span,
                hint,
            ))
        }
    }

    fn peek_is_contextual_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Ident(s) if s == kw)
    }

    fn parse_use(&mut self) -> Result<RawUse, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Use, "Expected 'use'")?;
        let path = match self.peek().kind.clone() {
            TokenKind::Str(s) => {
                self.advance();
                s
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E010_UNEXPECTED_TOKEN",
                    "parse",
                    format!("Expected relative file path string after 'use', found {other:?}"),
                    self.peek().span,
                    "Write import directives as: use \"relative/path.ifm\";",
                ));
            }
        };
        self.expect(&TokenKind::Semi, "Terminate 'use' directive with ';'")?;
        Ok(RawUse {
            path,
            span: tok.span,
        })
    }

    fn parse_const(&mut self) -> Result<RawConst, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Const, "Expected 'const'")?;
        let (name, _) = self.expect_ident("Expected constant name after 'const'")?;
        self.expect(&TokenKind::Colon, "Expected ':' and type after constant name")?;
        let ty = self.parse_type()?;
        if !matches!(ty, Type::I64 | Type::Fixed | Type::Bool) {
            return Err(InfimumDiagnostic::new(
                "E064_CONST_EVAL",
                "parse",
                format!("Constant '{name}' has unsupported type '{ty}'; only 'i64', 'fixed', and 'bool' are allowed"),
                tok.span,
                "Declare constants as 'const NAME: i64 = ...;', 'const G: fixed = 9.81;', or 'const FLAG: bool = ...;'",
            ));
        }
        self.expect(&TokenKind::Assign, "Expected '=' in const declaration")?;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::Semi, "Terminate 'const' declaration with ';'")?;
        Ok(RawConst {
            name,
            is_pub: false,
            ty,
            expr,
            span: tok.span,
        })
    }

    fn parse_mod(&mut self) -> Result<ParsedMod, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'mod'")?;
        let (name, _) = self.expect_ident("Expected module name after 'mod'")?;
        if self.peek_is_contextual_kw("from") {
            self.advance();
            let tok = self.peek().clone();
            let path = match tok.kind {
                TokenKind::Str(s) => {
                    self.advance();
                    s
                }
                other => {
                    return Err(InfimumDiagnostic::new(
                        "E010_UNEXPECTED_TOKEN",
                        "parse",
                        format!("Expected module file path string after 'from', found {other:?}"),
                        tok.span,
                        "Write: mod ai from \"ai.ifm\";",
                    ));
                }
            };
            self.expect(&TokenKind::Semi, "Terminate 'mod ... from \"...\";' with ';'")?;
            return Ok(ParsedMod::File(RawModFile { name, path, span }));
        }
        self.expect(&TokenKind::LBrace, "Expected '{' or 'from \"file.ifm\";' after module name")?;
        let mut items = Vec::new();
        let mut formas = Vec::new();
        let mut functions = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let is_pub = if self.peek_is_contextual_kw("pub") {
                self.advance();
                true
            } else {
                false
            };
            match self.peek().kind {
                TokenKind::Const => {
                    let mut c = self.parse_const()?;
                    c.is_pub = is_pub;
                    items.push(RawItem::Const(c));
                }
                TokenKind::Enum => items.push(RawItem::Enum(self.parse_enum()?)),
                TokenKind::Layout => items.push(RawItem::Layout(self.parse_layout()?)),
                _ if self.peek_is_contextual_kw("struct") => {
                    items.push(RawItem::Struct(self.parse_struct()?))
                }
                TokenKind::Forma => {
                    formas.push(crate::forma::parser::parse_forma_def(self, false)?)
                }
                _ => {
                    let mut f = self.parse_fn()?;
                    f.is_pub = is_pub;
                    functions.push(f);
                }
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close module body")?;
        Ok(ParsedMod::Inline(RawMod {
            name,
            items,
            formas,
            functions,
        }))
    }

    fn parse_enum(&mut self) -> Result<RawEnum, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Enum, "Expected 'enum'")?;
        let (name, _) = self.expect_ident("Expected enum name after 'enum'")?;
        self.expect(&TokenKind::LBrace, "Expected '{' to open enum body")?;

        let mut variants = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (vname, vspan) = self.expect_ident("Expected enum variant name")?;
            let value = if matches!(self.peek().kind, TokenKind::Assign) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            variants.push(RawEnumVariant {
                name: vname,
                value,
                span: vspan,
            });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close enum body")?;
        Ok(RawEnum {
            name,
            variants,
            span: tok.span,
        })
    }

    fn parse_opt_inv(&mut self) -> Result<Option<RawInv>, InfimumDiagnostic> {
        if !self.peek_is_contextual_kw("inv") {
            return Ok(None);
        }
        let tok = self.advance();
        let lo = self.parse_expr()?;
        let inclusive = if matches!(self.peek().kind, TokenKind::DotDotEq) {
            self.advance();
            true
        } else if matches!(self.peek().kind, TokenKind::DotDot) {
            self.advance();
            false
        } else {
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!(
                    "Expected '..=' or '..' in field invariant, found {:?}",
                    self.peek().kind
                ),
                self.peek().span,
                "Write 'inv 0..=7' or 'inv 0..8' after a struct or layout field",
            ));
        };
        let hi = self.parse_expr()?;
        Ok(Some(RawInv {
            lo,
            hi,
            inclusive,
            span: tok.span,
        }))
    }

    fn parse_struct(&mut self) -> Result<RawStruct, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'struct'")?;
        let (name, _) = self.expect_ident("Expected struct name after 'struct'")?;
        self.expect(&TokenKind::LBrace, "Expected '{' to open struct body")?;
        let mut fields = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (fname, fspan) = self.expect_ident("Expected struct field name")?;
            let mut is_fixed = false;
            if matches!(self.peek().kind, TokenKind::Colon) {
                self.advance();
                let (fty, fty_span) =
                    self.expect_ident("Expected 'word', 'i64', or 'fixed' for struct field type")?;
                if fty == "fixed" {
                    is_fixed = true;
                } else if fty != "word" && fty != "i64" {
                    return Err(InfimumDiagnostic::new(
                        "E010_UNEXPECTED_TOKEN",
                        "parse",
                        format!("Unsupported struct field type '{fty}'; expected 'word', 'i64', or 'fixed'"),
                        fty_span,
                        "Every struct field occupies 1 linear-memory word; declare it as 'field: word' or 'field: fixed'",
                    ));
                }
            }
            let inv = self.parse_opt_inv()?;
            fields.push((fname, is_fixed, inv, fspan));
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close struct body")?;
        Ok(RawStruct { name, fields, span })
    }

    fn parse_layout(&mut self) -> Result<RawLayout, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Layout, "Expected 'layout'")?;
        let (name, _) = self.expect_ident("Expected layout name after 'layout'")?;
        let base_expr = if self.peek_is_contextual_kw("at") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&TokenKind::LBrace, "Expected '{' to open layout body")?;

        let mut fields = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (fname, fspan) = self.expect_ident("Expected layout field name")?;
            self.expect(&TokenKind::Colon, "Expected ':' after layout field name")?;
            let (kind_kw, _) =
                self.expect_ident("Expected 'word', 'words[N]', 'fixed', 'fixed[N]', 'Struct', or 'Struct[N]'")?;
            let (struct_type, is_array, is_fixed, len_expr) = match kind_kw.as_str() {
                "word" => (None, false, false, None),
                "words" => {
                    self.expect(
                        &TokenKind::LBracket,
                        "Expected '[' after 'words' (e.g. words[8200])",
                    )?;
                    let len = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket, "Expected ']' after words length")?;
                    (None, true, false, Some(len))
                }
                "fixed" => {
                    if matches!(self.peek().kind, TokenKind::LBracket) {
                        self.advance();
                        let len = self.parse_expr()?;
                        self.expect(&TokenKind::RBracket, "Expected ']' after fixed array length")?;
                        (None, true, true, Some(len))
                    } else {
                        (None, false, true, None)
                    }
                }
                other => {
                    if matches!(self.peek().kind, TokenKind::LBracket) {
                        self.advance();
                        let len = self.parse_expr()?;
                        self.expect(&TokenKind::RBracket, "Expected ']' after struct array length")?;
                        (Some(other.to_string()), true, false, Some(len))
                    } else {
                        (Some(other.to_string()), false, false, None)
                    }
                }
            };
            let mut at_expr = None;
            let mut inv = None;
            for _ in 0..2 {
                if at_expr.is_none() && self.peek_is_contextual_kw("at") {
                    self.advance();
                    at_expr = Some(self.parse_expr()?);
                } else if inv.is_none() && self.peek_is_contextual_kw("inv") {
                    inv = self.parse_opt_inv()?;
                } else {
                    break;
                }
            }
            fields.push(RawLayoutField {
                name: fname,
                struct_type,
                len_expr,
                is_array,
                is_fixed,
                at_expr,
                inv,
                span: fspan,
            });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close layout body")?;
        Ok(RawLayout {
            name,
            base_expr,
            fields,
            span: tok.span,
        })
    }

    fn parse_fn(&mut self) -> Result<RawFnDef, InfimumDiagnostic> {
        let exported = if matches!(self.peek().kind, TokenKind::Export) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(
            &TokenKind::Fn,
            "Start each Infimum node definition with 'fn <name>(...) -> <type> { ... }' or 'export fn ...'",
        )?;
        let (name, name_span) = self.expect_ident("Provide a function alias name after 'fn'")?;
        self.expect(&TokenKind::LParen, "Expected '(' to start parameter list")?;

        let mut params = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                let (pname, _) = self.expect_ident("Expected parameter name")?;
                self.expect(&TokenKind::Colon, "Expected ':' after parameter name")?;
                let pty = self.parse_raw_type()?;
                params.push((pname, pty));
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "Expected ')' after parameter list")?;

        let mut effects = Vec::new();
        match &self.peek().kind {
            TokenKind::Arrow => {
                self.advance();
            }
            TokenKind::EffectOpen => {
                self.advance();
                if !matches!(self.peek().kind, TokenKind::EffectClose) {
                    loop {
                        let (ename, _) =
                            self.expect_ident("Expected effect capability name inside '-[...]->'")?;
                        let scope = if matches!(self.peek().kind, TokenKind::LParen) {
                            self.advance();
                            let s = match self.peek().kind.clone() {
                                TokenKind::Str(val) => {
                                    self.advance();
                                    val
                                }
                                other => {
                                    return Err(InfimumDiagnostic::new(
                                        "E012_EXPECTED_SCOPE_STR",
                                        "parse",
                                        format!("Expected string literal scope, found {other:?}"),
                                        self.peek().span,
                                        "Write capability scope as a string literal, e.g. Fs(\"/tmp\")",
                                    ));
                                }
                            };
                            self.expect(
                                &TokenKind::RParen,
                                "Expected ')' after capability scope literal",
                            )?;
                            Some(s)
                        } else {
                            None
                        };
                        effects.push(EffectCap { name: ename, scope });
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(
                    &TokenKind::EffectClose,
                    "Close capability list with ']->' before return type",
                )?;
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E013_EXPECTED_RETURN_ARROW",
                    "parse",
                    format!("Expected '->' or '-[...]->', found {other:?}"),
                    self.peek().span,
                    "Specify return type with '-> <Type>' or effects + return type with '-[Mem]-> <Type>'",
                ));
            }
        }

        let ret_type = self.parse_raw_type()?;

        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::Req => {
                    self.advance();
                    requires.push(self.parse_expr()?);
                }
                TokenKind::Ens => {
                    self.advance();
                    ensures.push(self.parse_expr()?);
                }
                _ => break,
            }
        }

        let body = self.parse_block_expr()?;

        Ok(RawFnDef {
            name,
            exported,
            is_pub: false,
            module: None,
            test_label: None,
            params,
            effects,
            ret_type,
            requires,
            ensures,
            body,
            source_file: None,
            span: name_span,
        })
    }

    fn parse_test_fn(&mut self) -> Result<RawFnDef, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'test'")?;
        let tok = self.peek().clone();
        let label = match tok.kind {
            TokenKind::Str(s) => {
                self.advance();
                s
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E010_UNEXPECTED_TOKEN",
                    "parse",
                    format!("Expected test name string literal after 'test', found {other:?}"),
                    tok.span,
                    "Write: test \"my test name\" { assert(1 + 1 == 2); }",
                ));
            }
        };
        let raw_stmts = self.parse_stmt_list_block()?;
        let mut stmts = Vec::with_capacity(raw_stmts.len() + 1);
        stmts.push(RawStmt::Let {
            name: "__ifm_test_fail".to_string(),
            mutable: true,
            ty: Some(RawType::Builtin(Type::I64)),
            value: RawExpr::IntLit(0),
        });
        for st in raw_stmts {
            stmts.push(rewrite_assert_stmt(st, span.line as i64));
        }
        let body = RawExpr::Block {
            stmts,
            tail: Box::new(RawExpr::Var("__ifm_test_fail".to_string())),
        };
        Ok(RawFnDef {
            name: format!("__ifm_test_L{}_C{}", span.line, span.col),
            exported: false,
            is_pub: false,
            module: None,
            test_label: Some(label),
            params: vec![],
            effects: vec![EffectCap {
                name: "Mem".to_string(),
                scope: None,
            }],
            ret_type: RawType::Builtin(Type::I64),
            requires: vec![],
            ensures: vec![],
            body,
            source_file: None,
            span,
        })
    }

    fn parse_raw_type(&mut self) -> Result<RawType, InfimumDiagnostic> {
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            if matches!(self.peek().kind, TokenKind::RParen) {
                self.advance();
                return Ok(RawType::Builtin(Type::Unit));
            }
            let first = self.parse_raw_type()?;
            if matches!(self.peek().kind, TokenKind::Comma) {
                let mut elems = vec![first];
                while matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                    elems.push(self.parse_raw_type()?);
                }
                self.expect(&TokenKind::RParen, "Expected ')' to close tuple type")?;
                return Ok(RawType::Tuple(elems));
            }
            self.expect(&TokenKind::RParen, "Expected ')' after parenthesized type")?;
            return Ok(first);
        }
        let (tname, span) = self.expect_ident(
            "Expected type name (i64, fixed, bool, str, unit, StructName, Secret<T>, (T1, T2))",
        )?;
        match tname.as_str() {
            "i64" => Ok(RawType::Builtin(Type::I64)),
            "fixed" => Ok(RawType::Builtin(Type::Fixed)),
            "bool" => Ok(RawType::Builtin(Type::Bool)),
            "str" | "String" => Ok(RawType::Builtin(Type::Str)),
            "unit" | "Unit" => Ok(RawType::Builtin(Type::Unit)),
            "Secret" => {
                self.expect(&TokenKind::Lt, "Expected '<' after Secret")?;
                let inner = self.parse_raw_type()?;
                self.expect(&TokenKind::Gt, "Expected '>' to close Secret<T>")?;
                Ok(RawType::Secret(Box::new(inner)))
            }
            _ => Ok(RawType::Named(tname, span)),
        }
    }

    fn parse_type(&mut self) -> Result<Type, InfimumDiagnostic> {
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            if matches!(self.peek().kind, TokenKind::RParen) {
                self.advance();
                return Ok(Type::Unit);
            }
            let first = self.parse_type()?;
            if matches!(self.peek().kind, TokenKind::Comma) {
                let mut elems = vec![first];
                while matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                    elems.push(self.parse_type()?);
                }
                self.expect(&TokenKind::RParen, "Expected ')' to close tuple type")?;
                return Ok(Type::Tuple(elems));
            }
            self.expect(&TokenKind::RParen, "Expected ')' after parenthesized type")?;
            return Ok(first);
        }
        let (tname, span) =
            self.expect_ident("Expected type name (i64, bool, str, unit, Secret<T>, (T1, T2))")?;
        match tname.as_str() {
            "i64" => Ok(Type::I64),
            "fixed" => Ok(Type::Fixed),
            "bool" => Ok(Type::Bool),
            "str" | "String" => Ok(Type::Str),
            "unit" | "Unit" => Ok(Type::Unit),
            "Secret" => {
                self.expect(&TokenKind::Lt, "Expected '<' after Secret")?;
                let inner = self.parse_type()?;
                self.expect(&TokenKind::Gt, "Expected '>' to close Secret<T>")?;
                Ok(Type::Secret(Box::new(inner)))
            }
            other => Err(InfimumDiagnostic::new(
                "E014_UNKNOWN_TYPE",
                "parse",
                format!("Unknown type '{other}'"),
                span,
                "Use one of the built-in Infimum types: i64, bool, str, unit, or Secret<T>",
            )),
        }
    }

    fn assign_op_for(kind: &TokenKind) -> Option<Option<BinOp>> {
        match kind {
            TokenKind::Assign => Some(None),
            TokenKind::PlusEq => Some(Some(BinOp::Add)),
            TokenKind::MinusEq => Some(Some(BinOp::Sub)),
            TokenKind::StarEq => Some(Some(BinOp::Mul)),
            TokenKind::PipeEq => Some(Some(BinOp::BitOr)),
            TokenKind::AmpEq => Some(Some(BinOp::BitAnd)),
            _ => None,
        }
    }

    fn try_parse_assign_stmt(&mut self) -> Result<Option<RawStmt>, InfimumDiagnostic> {
        // Variable assignment: `x = e;` or `x += e;` / `-=` / `*=` / `|=` / `&=`
        if matches!(&self.peek().kind, TokenKind::Ident(_)) {
            if let Some(op) = Self::assign_op_for(self.peek_next()) {
                let (vname, _) = self.expect_ident("Expected variable name")?;
                self.advance(); // consume assignment operator
                let val = self.parse_expr()?;
                self.expect(&TokenKind::Semi, "Terminate assignment statement with ';'")?;
                return Ok(Some(RawStmt::Assign {
                    name: vname,
                    op,
                    value: val,
                }));
            }
            // Layout assignment: `Layout.field`, `Layout.field[i]`, `Layout.field.sub`, `Layout.field[i].sub`
            if matches!(self.peek_at(1), TokenKind::Dot)
                && matches!(self.peek_at(2), TokenKind::Ident(_))
            {
                let saved = self.idx;
                let (layout, span) = self.expect_ident("Expected layout name")?;
                self.advance(); // '.'
                let (field, _) = self.expect_ident("Expected layout field name")?;
                let index = if matches!(self.peek().kind, TokenKind::LBracket) {
                    self.advance();
                    let idx_expr = self.parse_expr()?;
                    if !matches!(self.peek().kind, TokenKind::RBracket) {
                        self.idx = saved;
                        return Ok(None);
                    }
                    self.advance();
                    Some(idx_expr)
                } else {
                    None
                };
                let subfield = if matches!(self.peek().kind, TokenKind::Dot)
                    && matches!(self.peek_at(1), TokenKind::Ident(_))
                {
                    self.advance();
                    let (sf, _) = self.expect_ident("Expected struct field name")?;
                    Some(sf)
                } else {
                    None
                };
                if let Some(op) = Self::assign_op_for(&self.peek().kind) {
                    self.advance(); // assignment op
                    let val = self.parse_expr()?;
                    self.expect(
                        &TokenKind::Semi,
                        "Terminate layout field assignment with ';'",
                    )?;
                    return Ok(Some(RawStmt::LayoutStore {
                        layout,
                        field,
                        index,
                        subfield,
                        op,
                        value: val,
                        span,
                    }));
                }
                self.idx = saved;
            }
        }
        Ok(None)
    }

    fn parse_stmt_list_block(&mut self) -> Result<Vec<RawStmt>, InfimumDiagnostic> {
        self.expect(&TokenKind::LBrace, "Expected '{' to start loop body")?;
        let mut stmts = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            stmts.push(self.parse_single_stmt_in_loop()?);
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close loop body")?;
        Ok(stmts)
    }

    fn parse_single_stmt_in_loop(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        if matches!(self.peek().kind, TokenKind::Let) {
            return self.parse_let_stmt();
        }
        if matches!(self.peek().kind, TokenKind::While) {
            return self.parse_while_stmt();
        }
        if matches!(self.peek().kind, TokenKind::For) {
            return self.parse_for_stmt();
        }
        if matches!(self.peek().kind, TokenKind::Break) {
            let tok = self.advance();
            if matches!(self.peek().kind, TokenKind::Semi) {
                self.advance();
            }
            return Ok(RawStmt::Break(tok.span));
        }
        if matches!(self.peek().kind, TokenKind::Continue) {
            let tok = self.advance();
            if matches!(self.peek().kind, TokenKind::Semi) {
                self.advance();
            }
            return Ok(RawStmt::Continue(tok.span));
        }
        if let Some(assign) = self.try_parse_assign_stmt()? {
            return Ok(assign);
        }
        let expr = self.parse_expr()?;
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.advance();
        } else if !matches!(
            expr,
            RawExpr::If { .. } | RawExpr::Match { .. } | RawExpr::Block { .. }
        ) {
            self.expect(&TokenKind::Semi, "Terminate statement inside loop with ';'")?;
        }
        Ok(RawStmt::Expr(expr))
    }

    fn parse_let_stmt(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        self.expect(&TokenKind::Let, "Expected 'let'")?;
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            let mut names = Vec::new();
            while !matches!(self.peek().kind, TokenKind::RParen | TokenKind::Eof) {
                let is_mut = if matches!(self.peek().kind, TokenKind::Mut) {
                    self.advance();
                    true
                } else {
                    false
                };
                let (vname, _) = self.expect_ident("Expected variable name in tuple pattern")?;
                names.push((vname, is_mut));
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "Expected ')' to close tuple pattern")?;
            self.expect(&TokenKind::Assign, "Expected '=' in tuple let binding")?;
            let val = self.parse_expr()?;
            self.expect(&TokenKind::Semi, "Terminate tuple 'let' binding with ';'")?;
            return Ok(RawStmt::LetTuple { names, value: val });
        }
        let mutable = if matches!(self.peek().kind, TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };
        let (vname, _) = self.expect_ident("Expected variable name after 'let'")?;
        let ty = if matches!(self.peek().kind, TokenKind::Colon) {
            self.advance();
            Some(self.parse_raw_type()?)
        } else {
            None
        };
        self.expect(&TokenKind::Assign, "Expected '=' in let binding")?;
        let val = self.parse_expr()?;
        self.expect(&TokenKind::Semi, "Terminate 'let' binding with ';'")?;
        Ok(RawStmt::Let {
            name: vname,
            mutable,
            ty,
            value: val,
        })
    }

    fn parse_const_atom(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Ok(RawExpr::IntLit(v))
            }
            TokenKind::Fixed(v) => {
                self.advance();
                Ok(RawExpr::FixedLit(v))
            }
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after constant expression")?;
                Ok(inner)
            }
            TokenKind::Ident(name) => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (member, _) =
                        self.expect_ident("Expected member name after '::'")?;
                    if matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (prop, _) = self.expect_ident(
                            "Expected layout field property ('len' or 'end') after '::'",
                        )?;
                        Ok(RawExpr::Path3 {
                            head: name,
                            member,
                            prop,
                            span: tok.span,
                        })
                    } else if matches!(self.peek().kind, TokenKind::LParen) {
                        let args = self.parse_arg_list()?;
                        let mut expr = RawExpr::ModCall {
                            module: name,
                            func: member,
                            args,
                            span: tok.span,
                        };
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                        {
                            let dot_span = self.advance().span;
                            match self.advance().kind {
                                TokenKind::Int(idx) => {
                                    expr = RawExpr::TupleField {
                                        expr: Box::new(expr),
                                        index: idx.max(0) as usize,
                                    };
                                }
                                TokenKind::Ident(field) => {
                                    expr = RawExpr::FieldAccess {
                                        expr: Box::new(expr),
                                        field,
                                        span: dot_span,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Ok(expr)
                    } else {
                        Ok(RawExpr::Path2 {
                            head: name,
                            member,
                            span: tok.span,
                        })
                    }
                } else {
                    Ok(RawExpr::Var(name))
                }
            }
            other => Err(InfimumDiagnostic::new(
                "E017_INVALID_LOOP_BOUND",
                "parse",
                format!("Expected positive integer constant after 'max', found {other:?}"),
                tok.span,
                "Provide a positive compile-time iteration bound (e.g. 'while i < n max 4096 { ... }' or 'max MAX_ITERS')",
            )),
        }
    }

    fn parse_while_stmt(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        let w_tok = self.expect(&TokenKind::While, "Expected 'while'")?;
        let cond = self.parse_expr()?;
        self.expect(
            &TokenKind::Max,
            "Every 'while' loop in Infimum.Strata must declare an explicit upper bound: 'while <cond> max <N> { ... }'",
        )?;
        let max_iters = self.parse_const_atom()?;
        let body = self.parse_stmt_list_block()?;
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.advance();
        }
        Ok(RawStmt::While {
            cond,
            max_iters,
            body,
            span: w_tok.span,
        })
    }

    fn parse_for_stmt(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        let f_tok = self.expect(&TokenKind::For, "Expected 'for'")?;
        let (var, _) = self.expect_ident("Expected loop variable name after 'for'")?;
        if !self.peek_is_contextual_kw("in") {
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Expected 'in' after 'for {var}', found {:?}", self.peek().kind),
                self.peek().span,
                "Write 'for i in 0..N { ... }' or 'for i in 0..=N { ... }'",
            ));
        }
        self.advance();
        let start = self.parse_expr()?;
        let inclusive = if matches!(self.peek().kind, TokenKind::DotDotEq) {
            self.advance();
            true
        } else if matches!(self.peek().kind, TokenKind::DotDot) {
            self.advance();
            false
        } else {
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Expected '..' or '..=' in 'for' range, found {:?}", self.peek().kind),
                self.peek().span,
                "Write 'for i in start..end { ... }' or 'for i in start..=end { ... }'",
            ));
        };
        let end = self.parse_expr()?;
        let max_iters = if matches!(self.peek().kind, TokenKind::Max) {
            self.advance();
            Some(self.parse_const_atom()?)
        } else {
            None
        };
        let body = self.parse_stmt_list_block()?;
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.advance();
        }
        Ok(RawStmt::For {
            var,
            start,
            end,
            inclusive,
            max_iters,
            body,
            span: f_tok.span,
        })
    }

    fn parse_block_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        self.expect(&TokenKind::LBrace, "Expected '{' to open block")?;
        let mut stmts = Vec::new();

        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            if matches!(self.peek().kind, TokenKind::Let) {
                stmts.push(self.parse_let_stmt()?);
                continue;
            }
            if matches!(self.peek().kind, TokenKind::While) {
                stmts.push(self.parse_while_stmt()?);
                continue;
            }
            if matches!(self.peek().kind, TokenKind::For) {
                stmts.push(self.parse_for_stmt()?);
                continue;
            }
            if matches!(self.peek().kind, TokenKind::Break) {
                let tok = self.advance();
                if matches!(self.peek().kind, TokenKind::Semi) {
                    self.advance();
                }
                stmts.push(RawStmt::Break(tok.span));
                continue;
            }
            if matches!(self.peek().kind, TokenKind::Continue) {
                let tok = self.advance();
                if matches!(self.peek().kind, TokenKind::Semi) {
                    self.advance();
                }
                stmts.push(RawStmt::Continue(tok.span));
                continue;
            }
            if let Some(assign) = self.try_parse_assign_stmt()? {
                stmts.push(assign);
                continue;
            }

            let expr = self.parse_expr()?;
            if matches!(self.peek().kind, TokenKind::Semi) {
                self.advance();
                if matches!(self.peek().kind, TokenKind::RBrace) {
                    self.advance();
                    stmts.push(RawStmt::Expr(expr));
                    return Ok(RawExpr::Block {
                        stmts,
                        tail: Box::new(RawExpr::UnitLit),
                    });
                }
                stmts.push(RawStmt::Expr(expr));
            } else if matches!(
                expr,
                RawExpr::If { .. } | RawExpr::Match { .. } | RawExpr::Block { .. }
            ) && !matches!(self.peek().kind, TokenKind::RBrace)
            {
                // Allow `if ... { ... }`, `match ... { ... }`, and `{ ... }` statements without ';' before next statement
                stmts.push(RawStmt::Expr(expr));
            } else {
                self.expect(
                    &TokenKind::RBrace,
                    "Expected '}' after block tail expression (or ';' before next statement)",
                )?;
                if stmts.is_empty() {
                    return Ok(expr);
                }
                return Ok(RawExpr::Block {
                    stmts,
                    tail: Box::new(expr),
                });
            }
        }

        self.expect(&TokenKind::RBrace, "Expected '}' to close block")?;
        Ok(RawExpr::Block {
            stmts,
            tail: Box::new(RawExpr::UnitLit),
        })
    }

    fn parse_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek().kind, TokenKind::OrOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = RawExpr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_bitor()?;
        while matches!(self.peek().kind, TokenKind::AndAnd) {
            self.advance();
            let rhs = self.parse_bitor()?;
            lhs = RawExpr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_bitor(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_bitxor()?;
        while matches!(self.peek().kind, TokenKind::Pipe) {
            self.advance();
            let rhs = self.parse_bitxor()?;
            lhs = RawExpr::Binary {
                op: BinOp::BitOr,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_bitxor(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_bitand()?;
        while matches!(self.peek().kind, TokenKind::Caret) {
            self.advance();
            let rhs = self.parse_bitand()?;
            lhs = RawExpr::Binary {
                op: BinOp::BitXor,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_bitand(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_eq()?;
        while matches!(self.peek().kind, TokenKind::Amp) {
            self.advance();
            let rhs = self.parse_eq()?;
            lhs = RawExpr::Binary {
                op: BinOp::BitAnd,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_rel()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::NotEq => BinOp::Ne,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_rel()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_shift()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Le => BinOp::Le,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::Ge => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_shift()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Shl => BinOp::Shl,
                TokenKind::Shr => BinOp::Shr,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_add()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        match self.peek().kind {
            TokenKind::Minus => {
                self.advance();
                let inner = self.parse_unary()?;
                Ok(RawExpr::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(inner),
                })
            }
            TokenKind::Bang => {
                self.advance();
                let inner = self.parse_unary()?;
                Ok(RawExpr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(inner),
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_if_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        self.expect(&TokenKind::If, "Expected 'if'")?;
        let cond = self.parse_expr()?;
        let then_branch = self.parse_block_expr()?;
        let else_branch = if matches!(self.peek().kind, TokenKind::Else) {
            self.advance();
            if matches!(self.peek().kind, TokenKind::If) {
                self.parse_if_expr()?
            } else {
                self.parse_block_expr()?
            }
        } else {
            RawExpr::UnitLit
        };
        Ok(RawExpr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        })
    }

    fn parse_match_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mtok = self.expect(&TokenKind::Match, "Expected 'match'")?;
        let scrutinee = self.parse_expr()?;
        self.expect(&TokenKind::LBrace, "Expected '{' to open match arms")?;

        let mut arms = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let mut patterns = vec![self.parse_match_pattern()?];
            while matches!(self.peek().kind, TokenKind::Pipe) {
                self.advance();
                patterns.push(self.parse_match_pattern()?);
            }
            self.expect(&TokenKind::FatArrow, "Expected '=>' after match pattern")?;
            let body = self.parse_expr()?;
            let is_block_like = matches!(
                body,
                RawExpr::Block { .. } | RawExpr::If { .. } | RawExpr::Match { .. }
            );
            arms.push(RawMatchArm { patterns, body });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else if !is_block_like && !matches!(self.peek().kind, TokenKind::RBrace) {
                self.expect(&TokenKind::Comma, "Expected ',' after match arm")?;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close match expression")?;
        Ok(RawExpr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: mtok.span,
        })
    }

    fn parse_match_pattern(&mut self) -> Result<RawMatchPattern, InfimumDiagnostic> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Ok(RawMatchPattern::Int(v))
            }
            TokenKind::Minus => {
                self.advance();
                let next = self.peek().clone();
                if let TokenKind::Int(v) = next.kind {
                    self.advance();
                    Ok(RawMatchPattern::Int(v.wrapping_neg()))
                } else {
                    Err(InfimumDiagnostic::new(
                        "E010_UNEXPECTED_TOKEN",
                        "parse",
                        format!("Expected integer literal after '-' in match pattern, found {:?}", next.kind),
                        next.span,
                        "Use an integer literal (e.g. -1) in match patterns",
                    ))
                }
            }
            TokenKind::True => {
                self.advance();
                Ok(RawMatchPattern::Bool(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(RawMatchPattern::Bool(false))
            }
            TokenKind::Ident(name) => {
                self.advance();
                if name == "_" {
                    return Ok(RawMatchPattern::Wildcard(tok.span));
                }
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (member, _) = self.expect_ident("Expected variant or field name after '::'")?;
                    if matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (prop, _) = self.expect_ident(
                            "Expected layout field property ('len' or 'end') after '::'",
                        )?;
                        Ok(RawMatchPattern::Path3(name, member, prop, tok.span))
                    } else {
                        Ok(RawMatchPattern::Path2(name, member, tok.span))
                    }
                } else {
                    Ok(RawMatchPattern::ConstRef(name, tok.span))
                }
            }
            other => Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Invalid match pattern starting with {other:?}"),
                tok.span,
                "Use an integer/char/bool literal, const name, Enum::Variant, or '_' wildcard",
            )),
        }
    }

    fn parse_primary(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Ok(RawExpr::IntLit(v))
            }
            TokenKind::Fixed(v) => {
                self.advance();
                Ok(RawExpr::FixedLit(v))
            }
            TokenKind::True => {
                self.advance();
                Ok(RawExpr::BoolLit(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(RawExpr::BoolLit(false))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(RawExpr::StrLit(s))
            }
            TokenKind::LBrace => self.parse_block_expr(),
            TokenKind::LParen => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::RParen) {
                    self.advance();
                    return Ok(RawExpr::UnitLit);
                }
                let first = self.parse_expr()?;
                let mut expr = if matches!(self.peek().kind, TokenKind::Comma) {
                    let mut elems = vec![first];
                    while matches!(self.peek().kind, TokenKind::Comma) {
                        self.advance();
                        if matches!(self.peek().kind, TokenKind::RParen) {
                            break;
                        }
                        elems.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RParen, "Expected ')' to close tuple literal")?;
                    RawExpr::TupleLit(elems)
                } else {
                    self.expect(&TokenKind::RParen, "Expected ')' after parenthesized expression")?;
                    first
                };
                while matches!(self.peek().kind, TokenKind::Dot)
                    && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                {
                    let dot_span = self.advance().span;
                    match self.advance().kind {
                        TokenKind::Int(idx) => {
                            expr = RawExpr::TupleField {
                                expr: Box::new(expr),
                                index: idx.max(0) as usize,
                            };
                        }
                        TokenKind::Ident(field) => {
                            expr = RawExpr::FieldAccess {
                                expr: Box::new(expr),
                                field,
                                span: dot_span,
                            };
                        }
                        _ => {}
                    }
                }
                Ok(expr)
            }
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Match => self.parse_match_expr(),
            TokenKind::Do => {
                self.advance();
                let (effect, _) = self.expect_ident(
                    "Expected capability name after 'do' (e.g. do Net.post(...))",
                )?;
                self.expect(&TokenKind::Dot, "Expected '.' between capability and operation")?;
                let (op, _) = self.expect_ident("Expected effect operation name after '.'")?;
                let args = self.parse_arg_list()?;
                Ok(RawExpr::Perform { effect, op, args })
            }
            TokenKind::MemLoadKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'mem_load'")?;
                let index = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after mem_load(index)")?;
                Ok(RawExpr::MemLoad {
                    index: Box::new(index),
                })
            }
            TokenKind::MemStoreKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'mem_store'")?;
                let index = self.parse_expr()?;
                self.expect(&TokenKind::Comma, "Expected ',' between mem_store index and value")?;
                let value = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after mem_store(index, value)")?;
                Ok(RawExpr::MemStore {
                    index: Box::new(index),
                    value: Box::new(value),
                })
            }
            TokenKind::SecretKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'secret'")?;
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after secret(expr)")?;
                Ok(RawExpr::SecretWrap(Box::new(inner)))
            }
            TokenKind::RevealKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'reveal'")?;
                let domain = match self.peek().kind.clone() {
                    TokenKind::Str(s) => {
                        self.advance();
                        s
                    }
                    other => {
                        return Err(InfimumDiagnostic::new(
                            "E015_EXPECTED_DOMAIN_STR",
                            "parse",
                            format!("Expected destination domain string literal, found {other:?}"),
                            self.peek().span,
                            "Pass explicit destination domain literal: reveal(\"mybank.com\", secret_val)",
                        ));
                    }
                };
                self.expect(&TokenKind::Comma, "Expected ',' after reveal domain")?;
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after reveal arguments")?;
                Ok(RawExpr::Reveal {
                    domain,
                    expr: Box::new(inner),
                })
            }
            TokenKind::HashRef(hash) => {
                self.advance();
                let args = self.parse_arg_list()?;
                Ok(RawExpr::Call {
                    target: TargetRef::Hash(hash),
                    args,
                })
            }
            TokenKind::Ident(name) => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (member, _) = self.expect_ident("Expected member name after '::'")?;
                    if matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (prop, _) = self.expect_ident(
                            "Expected layout field property ('len' or 'end') after '::'",
                        )?;
                        Ok(RawExpr::Path3 {
                            head: name,
                            member,
                            prop,
                            span: tok.span,
                        })
                    } else if matches!(self.peek().kind, TokenKind::LParen) {
                        let args = self.parse_arg_list()?;
                        let mut expr = RawExpr::ModCall {
                            module: name,
                            func: member,
                            args,
                            span: tok.span,
                        };
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                        {
                            let dot_span = self.advance().span;
                            match self.advance().kind {
                                TokenKind::Int(idx) => {
                                    expr = RawExpr::TupleField {
                                        expr: Box::new(expr),
                                        index: idx.max(0) as usize,
                                    };
                                }
                                TokenKind::Ident(field) => {
                                    expr = RawExpr::FieldAccess {
                                        expr: Box::new(expr),
                                        field,
                                        span: dot_span,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Ok(expr)
                    } else {
                        Ok(RawExpr::Path2 {
                            head: name,
                            member,
                            span: tok.span,
                        })
                    }
                } else if matches!(self.peek().kind, TokenKind::Dot) {
                    if matches!(self.peek_next(), TokenKind::Int(_)) {
                        let mut expr = RawExpr::Var(name);
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_))
                        {
                            self.advance();
                            if let TokenKind::Int(idx) = self.advance().kind {
                                expr = RawExpr::TupleField {
                                    expr: Box::new(expr),
                                    index: idx.max(0) as usize,
                                };
                            }
                        }
                        return Ok(expr);
                    }
                    self.advance();
                    let (field, _) = self.expect_ident("Expected layout field name after '.'")?;
                    let index = if matches!(self.peek().kind, TokenKind::LBracket) {
                        self.advance();
                        let idx_expr = self.parse_expr()?;
                        self.expect(&TokenKind::RBracket, "Expected ']' after layout index")?;
                        Some(Box::new(idx_expr))
                    } else {
                        None
                    };
                    let subfield = if matches!(self.peek().kind, TokenKind::Dot) {
                        self.advance();
                        let (sf, _) = self.expect_ident("Expected struct field name after '.'")?;
                        Some(sf)
                    } else {
                        None
                    };
                    Ok(RawExpr::LayoutLoad {
                        layout: name,
                        field,
                        index,
                        subfield,
                        span: tok.span,
                    })
                } else if matches!(self.peek().kind, TokenKind::LParen) {
                    let args = self.parse_arg_list()?;
                    let target = if name == "$self" {
                        TargetRef::SelfRec
                    } else {
                        TargetRef::Named(name)
                    };
                    let mut expr = RawExpr::Call { target, args };
                    while matches!(self.peek().kind, TokenKind::Dot)
                        && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                    {
                        let dot_span = self.advance().span;
                        match self.advance().kind {
                            TokenKind::Int(idx) => {
                                expr = RawExpr::TupleField {
                                    expr: Box::new(expr),
                                    index: idx.max(0) as usize,
                                };
                            }
                            TokenKind::Ident(field) => {
                                expr = RawExpr::FieldAccess {
                                    expr: Box::new(expr),
                                    field,
                                    span: dot_span,
                                };
                            }
                            _ => {}
                        }
                    }
                    Ok(expr)
                } else if matches!(self.peek().kind, TokenKind::LBrace)
                    && matches!(self.peek_at(1), TokenKind::Ident(_))
                    && matches!(self.peek_at(2), TokenKind::Colon)
                {
                    self.advance(); // '{'
                    let mut fields = Vec::new();
                    while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                        let (fname, fspan) = self.expect_ident("Expected struct field name")?;
                        self.expect(&TokenKind::Colon, "Expected ':' after struct field name")?;
                        let fval = self.parse_expr()?;
                        fields.push((fname, fval, fspan));
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RBrace, "Expected '}' to close struct literal")?;
                    let mut expr = RawExpr::StructLit {
                        name,
                        fields,
                        span: tok.span,
                    };
                    while matches!(self.peek().kind, TokenKind::Dot)
                        && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                    {
                        let dot_span = self.advance().span;
                        match self.advance().kind {
                            TokenKind::Int(idx) => {
                                expr = RawExpr::TupleField {
                                    expr: Box::new(expr),
                                    index: idx.max(0) as usize,
                                };
                            }
                            TokenKind::Ident(field) => {
                                expr = RawExpr::FieldAccess {
                                    expr: Box::new(expr),
                                    field,
                                    span: dot_span,
                                };
                            }
                            _ => {}
                        }
                    }
                    Ok(expr)
                } else {
                    Ok(RawExpr::Var(name))
                }
            }
            other => Err(InfimumDiagnostic::new(
                "E016_EXPECTED_EXPR",
                "parse",
                format!("Expected expression, found {other:?}"),
                tok.span,
                "Provide a valid expression (literal, variable, call, block, if/else, match, mem_load/mem_store, or 'do Effect.op(...)')",
            )),
        }
    }

    fn parse_arg_list(&mut self) -> Result<Vec<RawExpr>, InfimumDiagnostic> {
        self.expect(&TokenKind::LParen, "Expected '(' to start argument list")?;
        let mut args = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "Expected ')' to close argument list")?;
        Ok(args)
    }
}

// ---------------------------------------------------------------------------
// Stage B: Compile-Time Evaluation, Static Layout Overlap Proof, & Desugaring
// ---------------------------------------------------------------------------

struct ModuleResolver {
    consts: BTreeMap<String, ConstDef>,
    enums: BTreeMap<String, EnumDef>,
    structs: BTreeMap<String, StructDef>,
    layouts: BTreeMap<String, LayoutDef>,
    layout_field_meta: BTreeMap<(String, String), LayoutFieldMeta>,
    consts_order: Vec<ConstDef>,
    enums_order: Vec<EnumDef>,
    layouts_order: Vec<LayoutDef>,
    layout_spans: Vec<(i64, i64, String, Span)>,
    auto_layout_cursor: i64,
    mod_consts: BTreeMap<(String, String), (ConstValue, bool)>,
    mod_fns: BTreeMap<(String, String), (String, bool)>,
    mod_extra_fns: Vec<RawFnDef>,
    mod_extra_formas: Vec<crate::forma::FormaDef<RawExpr>>,
    current_mod: Option<String>,
    match_counter: usize,
    addr_counter: usize,
    loop_counter: usize,
    loop_stack: Vec<LoopCtrlCtx>,
    struct_vars: Vec<BTreeMap<String, String>>,
    fn_ret_struct: BTreeMap<String, String>,
}

fn rewrite_forma_mod_slots<E>(
    node: &mut crate::forma::FormaNode<E>,
    mname: &str,
    mod_fns: &BTreeMap<(String, String), (String, bool)>,
) {
    if let Some(ref mut rfn) = node.props.render {
        if let Some((mangled, _)) = mod_fns.get(&(mname.to_string(), rfn.clone())) {
            *rfn = mangled.clone();
        }
    }
    if let Some(ref mut wfn) = node.props.on_wheel {
        if let Some((mangled, _)) = mod_fns.get(&(mname.to_string(), wfn.clone())) {
            *wfn = mangled.clone();
        }
    }
    for ch in &mut node.children {
        rewrite_forma_mod_slots(ch, mname, mod_fns);
    }
    if let Some((_, ref mut else_nodes)) = node.forma_if {
        for ch in else_nodes {
            rewrite_forma_mod_slots(ch, mname, mod_fns);
        }
    }
}

#[derive(Clone)]
struct LoopCtrlCtx {
    brk_var: Option<String>,
    cont_var: Option<String>,
}

fn expr_has_loop_ctrl(e: &RawExpr) -> (bool, bool) {
    match e {
        RawExpr::If {
            then_branch,
            else_branch,
            ..
        } => {
            let (b1, c1) = expr_has_loop_ctrl(then_branch);
            let (b2, c2) = expr_has_loop_ctrl(else_branch);
            (b1 || b2, c1 || c2)
        }
        RawExpr::Block { stmts, tail } => {
            let (b1, c1) = stmts_have_loop_ctrl(stmts);
            let (b2, c2) = expr_has_loop_ctrl(tail);
            (b1 || b2, c1 || c2)
        }
        RawExpr::Match { arms, .. } => {
            let mut b = false;
            let mut c = false;
            for arm in arms {
                let (ab, ac) = expr_has_loop_ctrl(&arm.body);
                b |= ab;
                c |= ac;
            }
            (b, c)
        }
        _ => (false, false),
    }
}

fn stmt_has_loop_ctrl(s: &RawStmt) -> (bool, bool) {
    match s {
        RawStmt::Break(_) => (true, false),
        RawStmt::Continue(_) => (false, true),
        RawStmt::Expr(e) => expr_has_loop_ctrl(e),
        // Nested loops handle their own break/continue
        RawStmt::While { .. }
        | RawStmt::For { .. }
        | RawStmt::Let { .. }
        | RawStmt::LetTuple { .. }
        | RawStmt::Assign { .. }
        | RawStmt::LayoutStore { .. } => (false, false),
    }
}

fn stmts_have_loop_ctrl(stmts: &[RawStmt]) -> (bool, bool) {
    let mut b = false;
    let mut c = false;
    for s in stmts {
        let (sb, sc) = stmt_has_loop_ctrl(s);
        b |= sb;
        c |= sc;
    }
    (b, c)
}

impl ModuleResolver {
    fn new() -> Self {
        Self {
            consts: BTreeMap::new(),
            enums: BTreeMap::new(),
            structs: BTreeMap::new(),
            layouts: BTreeMap::new(),
            layout_field_meta: BTreeMap::new(),
            consts_order: Vec::new(),
            enums_order: Vec::new(),
            layouts_order: Vec::new(),
            layout_spans: Vec::new(),
            auto_layout_cursor: 705_536,
            mod_consts: BTreeMap::new(),
            mod_fns: BTreeMap::new(),
            mod_extra_fns: Vec::new(),
            mod_extra_formas: Vec::new(),
            current_mod: None,
            match_counter: 0,
            addr_counter: 0,
            loop_counter: 0,
            loop_stack: Vec::new(),
            struct_vars: Vec::new(),
            fn_ret_struct: BTreeMap::new(),
        }
    }

    fn struct_tuple_type(&self, sdef: &StructDef) -> Type {
        let elems = sdef
            .field_names
            .iter()
            .map(|fname| {
                if sdef.field_fixed.contains(fname) {
                    Type::Fixed
                } else {
                    Type::I64
                }
            })
            .collect();
        Type::Tuple(elems)
    }

    fn resolve_raw_type(&self, rt: &RawType) -> Result<Type, InfimumDiagnostic> {
        match rt {
            RawType::Builtin(t) => Ok(t.clone()),
            RawType::Secret(inner) => Ok(Type::Secret(Box::new(self.resolve_raw_type(inner)?))),
            RawType::Tuple(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.resolve_raw_type(e)?);
                }
                Ok(Type::Tuple(out))
            }
            RawType::Named(name, span) => {
                if let Some(sdef) = self.structs.get(name) {
                    Ok(self.struct_tuple_type(sdef))
                } else {
                    Err(InfimumDiagnostic::new(
                        "E014_UNKNOWN_TYPE",
                        "parse",
                        format!("Unknown type '{name}'"),
                        *span,
                        "Use a built-in type (i64, fixed, bool, str, unit, Secret<T>, (T1, T2)) or a declared 'struct' name",
                    ))
                }
            }
        }
    }

    fn lookup_struct_var(&self, name: &str) -> Option<String> {
        for scope in self.struct_vars.iter().rev() {
            if let Some(sname) = scope.get(name) {
                return Some(sname.clone());
            }
        }
        None
    }

    fn infer_raw_expr_struct(&self, expr: &RawExpr) -> Option<String> {
        match expr {
            RawExpr::StructLit { name, .. } => Some(name.clone()),
            RawExpr::Var(v) => self.lookup_struct_var(v),
            RawExpr::Call {
                target: TargetRef::Named(fname),
                ..
            } => {
                if let Some(ref cur_m) = self.current_mod {
                    let mangled = format!("{cur_m}__{fname}");
                    if let Some(s) = self.fn_ret_struct.get(&mangled) {
                        return Some(s.clone());
                    }
                }
                self.fn_ret_struct.get(fname).cloned()
            }
            RawExpr::ModCall { module, func, .. } => {
                let mangled = format!("{module}__{func}");
                self.fn_ret_struct.get(&mangled).cloned()
            }
            RawExpr::LayoutLoad {
                layout,
                field,
                subfield: None,
                ..
            } => {
                let meta = self
                    .layout_field_meta
                    .get(&(layout.clone(), field.clone()))?;
                meta.struct_type.clone()
            }
            RawExpr::If {
                then_branch,
                else_branch,
                ..
            } => self
                .infer_raw_expr_struct(then_branch)
                .or_else(|| self.infer_raw_expr_struct(else_branch)),
            RawExpr::Block { tail, .. } => self.infer_raw_expr_struct(tail),
            _ => None,
        }
    }

    fn eval_const(&self, expr: &RawExpr, span: Span) -> Result<ConstValue, InfimumDiagnostic> {
        match expr {
            RawExpr::IntLit(v) => Ok(ConstValue::Int(*v)),
            RawExpr::FixedLit(v) => Ok(ConstValue::Fixed(*v)),
            RawExpr::BoolLit(b) => Ok(ConstValue::Bool(*b)),
            RawExpr::Var(name) => {
                if let Some(ref cur_m) = self.current_mod {
                    if let Some((val, _)) = self.mod_consts.get(&(cur_m.clone(), name.clone())) {
                        return Ok(*val);
                    }
                }
                self.consts.get(name).map(|c| c.value).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!("Unknown constant '{name}' in constant expression"),
                        span,
                        "Declare the constant with 'const NAME: i64 = ...;' before referencing it",
                    )
                })
            }
            RawExpr::Path2 {
                head,
                member,
                span: pspan,
            } => Ok(ConstValue::Int(self.resolve_path2(head, member, *pspan)?)),
            RawExpr::Path3 {
                head,
                member,
                prop,
                span: pspan,
            } => Ok(ConstValue::Int(
                self.resolve_path3(head, member, prop, *pspan)?,
            )),
            RawExpr::Unary { op, expr: inner } => {
                let v = self.eval_const(inner, span)?;
                match (op, v) {
                    (UnaryOp::Neg, ConstValue::Int(n)) => n
                        .checked_neg()
                        .map(ConstValue::Int)
                        .ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                format!("Integer overflow negating {n} in constant expression"),
                                span,
                                "Keep constant expressions within signed 64-bit integer bounds",
                            )
                        }),
                    (UnaryOp::Neg, ConstValue::Fixed(n)) => n
                        .checked_neg()
                        .map(ConstValue::Fixed)
                        .ok_or_else(|| const_overflow("-", span)),
                    (UnaryOp::Not, ConstValue::Bool(b)) => Ok(ConstValue::Bool(!b)),
                    _ => Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!("Invalid unary operator '{}' in constant expression", op.as_str()),
                        span,
                        "Use '-' on i64 constants and '!' on bool constants",
                    )),
                }
            }
            RawExpr::Binary { op, lhs, rhs } => {
                let lv = self.eval_const(lhs, span)?;
                let rv = self.eval_const(rhs, span)?;
                match (op, lv, rv) {
                    (BinOp::Add, ConstValue::Int(a), ConstValue::Int(b)) => a
                        .checked_add(b)
                        .map(ConstValue::Int)
                        .ok_or_else(|| const_overflow("+", span)),
                    (BinOp::Sub, ConstValue::Int(a), ConstValue::Int(b)) => a
                        .checked_sub(b)
                        .map(ConstValue::Int)
                        .ok_or_else(|| const_overflow("-", span)),
                    (BinOp::Add, ConstValue::Fixed(a), ConstValue::Fixed(b)) => a
                        .checked_add(b)
                        .map(ConstValue::Fixed)
                        .ok_or_else(|| const_overflow("+", span)),
                    (BinOp::Sub, ConstValue::Fixed(a), ConstValue::Fixed(b)) => a
                        .checked_sub(b)
                        .map(ConstValue::Fixed)
                        .ok_or_else(|| const_overflow("-", span)),
                    (BinOp::Mul, ConstValue::Fixed(a), ConstValue::Fixed(b)) => {
                        let prod = (a as i128 * b as i128) / 65536;
                        i64::try_from(prod)
                            .map(ConstValue::Fixed)
                            .map_err(|_| const_overflow("*", span))
                    }
                    (BinOp::Div, ConstValue::Fixed(a), ConstValue::Fixed(b)) => {
                        if b == 0 {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                "Division by zero in fixed-point constant expression",
                                span,
                                "Ensure divisor is non-zero",
                            ));
                        }
                        let q = (a as i128 * 65536) / (b as i128);
                        i64::try_from(q)
                            .map(ConstValue::Fixed)
                            .map_err(|_| const_overflow("/", span))
                    }
                    (BinOp::Mul, ConstValue::Int(a), ConstValue::Int(b)) => a
                        .checked_mul(b)
                        .map(ConstValue::Int)
                        .ok_or_else(|| const_overflow("*", span)),
                    (BinOp::Div, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if b == 0 {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                "Division by zero in constant expression",
                                span,
                                "Ensure the divisor in a constant expression is non-zero",
                            ));
                        }
                        a.checked_div(b)
                            .map(ConstValue::Int)
                            .ok_or_else(|| const_overflow("/", span))
                    }
                    (BinOp::Mod, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if b == 0 {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                "Modulo by zero in constant expression",
                                span,
                                "Ensure the divisor in a constant expression is non-zero",
                            ));
                        }
                        a.checked_rem(b)
                            .map(ConstValue::Int)
                            .ok_or_else(|| const_overflow("%", span))
                    }
                    (BinOp::BitAnd, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Int(a & b))
                    }
                    (BinOp::BitOr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Int(a | b))
                    }
                    (BinOp::BitXor, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Int(a ^ b))
                    }
                    (BinOp::Shl, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if !(0..64).contains(&b) {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                format!("Invalid shift amount {b} in constant expression"),
                                span,
                                "Use a shift count in the range 0..63",
                            ));
                        }
                        Ok(ConstValue::Int(a.wrapping_shl(b as u32)))
                    }
                    (BinOp::Shr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if !(0..64).contains(&b) {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                format!("Invalid shift amount {b} in constant expression"),
                                span,
                                "Use a shift count in the range 0..63",
                            ));
                        }
                        Ok(ConstValue::Int(a.wrapping_shr(b as u32)))
                    }
                    (BinOp::Eq, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a == b))
                    }
                    (BinOp::Ne, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a != b))
                    }
                    (BinOp::Lt, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a < b))
                    }
                    (BinOp::Le, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a <= b))
                    }
                    (BinOp::Gt, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a > b))
                    }
                    (BinOp::Ge, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a >= b))
                    }
                    (BinOp::And, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a && b))
                    }
                    (BinOp::Or, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a || b))
                    }
                    (BinOp::Eq, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a == b))
                    }
                    (BinOp::Ne, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a != b))
                    }
                    _ => Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!(
                            "Type mismatch for operator '{}' in constant expression",
                            op.as_str()
                        ),
                        span,
                        "Ensure operand types match the operator in constant expressions",
                    )),
                }
            }
            _ => Err(InfimumDiagnostic::new(
                "E064_CONST_EVAL",
                "const-eval",
                "Non-constant expression used where a compile-time constant is required",
                span,
                "Use integer/bool literals, arithmetic/bitwise operators, const names, Enum::Variant, or Layout::field",
            )),
        }
    }

    fn eval_const_i64(&self, expr: &RawExpr, span: Span, context: &str) -> Result<i64, InfimumDiagnostic> {
        match self.eval_const(expr, span)? {
            ConstValue::Int(v) | ConstValue::Fixed(v) => Ok(v),
            ConstValue::Bool(_) => Err(InfimumDiagnostic::new(
                "E064_CONST_EVAL",
                "const-eval",
                format!("Expected numeric constant for {context}, found bool"),
                span,
                "Provide an integer or fixed-point constant expression",
            )),
        }
    }

    fn resolve_path2(&self, head: &str, member: &str, span: Span) -> Result<i64, InfimumDiagnostic> {
        if let Some((val, is_pub)) = self.mod_consts.get(&(head.to_string(), member.to_string())) {
            if !*is_pub && self.current_mod.as_deref() != Some(head) {
                return Err(InfimumDiagnostic::new(
                    "E068_PRIVATE_SYMBOL",
                    "resolve",
                    format!("Constant '{head}::{member}' is private to module '{head}'"),
                    span,
                    format!("Mark it 'pub const {member}' inside 'mod {head}' to access it externally"),
                ));
            }
            return match val {
                ConstValue::Int(v) | ConstValue::Fixed(v) => Ok(*v),
                ConstValue::Bool(b) => Ok(if *b { 1 } else { 0 }),
            };
        }
        if let Some(edef) = self.enums.get(head) {
            if let Some(v) = edef.variants.iter().find(|v| v.name == member) {
                return Ok(v.value);
            }
            return Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_ENUM_VARIANT",
                "resolve",
                format!("Enum '{head}' has no variant '{member}'"),
                span,
                format!("Check the variants declared in 'enum {head}'"),
            ));
        }
        if let Some(ldef) = self.layouts.get(head) {
            if let Some(f) = ldef.fields.iter().find(|f| f.name == member) {
                return Ok(f.offset);
            }
            return Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Layout '{head}' has no field '{member}'"),
                span,
                format!("Check the fields declared in 'layout {head}'"),
            ));
        }
        if let Some(sdef) = self.structs.get(head) {
            if member == "SIZE" || member == "size" {
                return Ok(sdef.size);
            }
            if let Some(&off) = sdef.fields.get(member) {
                return Ok(off);
            }
            return Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Struct '{head}' has no field '{member}'"),
                span,
                format!(
                    "Available fields in 'struct {head}': {} (plus '::SIZE')",
                    sdef.field_names.join(", ")
                ),
            ));
        }
        Err(InfimumDiagnostic::new(
            "E061_UNKNOWN_ENUM_VARIANT",
            "resolve",
            format!("Unknown enum, struct, or layout '{head}' in '{head}::{member}'"),
            span,
            format!("Declare 'enum {head}', 'struct {head}', or 'layout {head}' (or import it with 'use')"),
        ))
    }

    fn resolve_path3(
        &self,
        head: &str,
        member: &str,
        prop: &str,
        span: Span,
    ) -> Result<i64, InfimumDiagnostic> {
        let ldef = self.layouts.get(head).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout '{head}' in '{head}::{member}::{prop}'"),
                span,
                format!("Declare 'layout {head}' before referencing its field properties"),
            )
        })?;
        let field = ldef
            .fields
            .iter()
            .find(|f| f.name == member)
            .ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Layout '{head}' has no field '{member}'"),
                    span,
                    format!("Check the fields declared in 'layout {head}'"),
                )
            })?;
        let meta = self
            .layout_field_meta
            .get(&(head.to_string(), member.to_string()));
        match prop {
            "len" => Ok(field.len),
            "end" => Ok(field.offset + field.len),
            "stride" => Ok(meta.map(|m| m.stride).unwrap_or(1)),
            "count" => Ok(meta.map(|m| m.count).unwrap_or(field.len)),
            other => Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout field property '{other}' on '{head}::{member}'"),
                span,
                "Supported layout field properties are '::len', '::end', '::stride', and '::count'",
            )),
        }
    }

    fn lower_layout_addr_expr(
        &mut self,
        layout: &str,
        field: &str,
        index: Option<&RawExpr>,
        subfield: Option<&str>,
        locals: &mut Vec<BTreeSet<String>>,
        span: Span,
    ) -> Result<Expr, InfimumDiagnostic> {
        let ldef = self.layouts.get(layout).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout '{layout}' in '{layout}.{field}'"),
                span,
                format!("Declare 'layout {layout}' before accessing '{layout}.{field}'"),
            )
        })?;
        if !ldef.fields.iter().any(|f| f.name == field) {
            let names: Vec<&str> = ldef.fields.iter().take(12).map(|f| f.name.as_str()).collect();
            return Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Layout '{layout}' has no field '{field}'"),
                span,
                format!("Available fields in 'layout {layout}': {}", names.join(", ")),
            ));
        }
        let meta = self
            .layout_field_meta
            .get(&(layout.to_string(), field.to_string()))
            .cloned()
            .ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Missing layout metadata for '{layout}.{field}'"),
                    span,
                    "Internal layout metadata error",
                )
            })?;

        if meta.is_array && index.is_none() {
            let hint = match &meta.struct_type {
                Some(_) => format!(
                    "Use '{layout}.{field}[i].<field>' to access an element, or '{layout}::{field}' for the base word address"
                ),
                None => format!(
                    "Use '{layout}.{field}[i]' to access an element, or '{layout}::{field}' for the base word address"
                ),
            };
            return Err(InfimumDiagnostic::new(
                "E061_LAYOUT_ARRAY_ACCESS",
                "resolve",
                format!("Array field '{layout}.{field}' requires an index '[i]'"),
                span,
                hint,
            ));
        }
        if !meta.is_array && index.is_some() {
            return Err(InfimumDiagnostic::new(
                "E061_LAYOUT_ARRAY_ACCESS",
                "resolve",
                format!("Field '{layout}.{field}' is not an array; cannot index with '[...]'"),
                span,
                format!("Remove '[...]' or declare '{field}' as 'words[N]' / 'Struct[N]' in 'layout {layout}'"),
            ));
        }

        let sub_off: i64 = match (&meta.struct_type, subfield) {
            (Some(sname), Some(sf)) => {
                let sdef = self.structs.get(sname).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_LAYOUT_FIELD",
                        "resolve",
                        format!("Unknown struct type '{sname}'"),
                        span,
                        format!("Declare 'struct {sname} {{ ... }}'"),
                    )
                })?;
                *sdef.fields.get(sf).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_LAYOUT_FIELD",
                        "resolve",
                        format!("Struct '{sname}' has no field '{sf}'"),
                        span,
                        format!(
                            "Available fields in 'struct {sname}': {}",
                            sdef.field_names.join(", ")
                        ),
                    )
                })?
            }
            (Some(sname), None) => {
                let first = self
                    .structs
                    .get(sname)
                    .and_then(|s| s.field_names.first())
                    .map(|s| s.as_str())
                    .unwrap_or("x");
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!(
                        "Struct field '{layout}.{field}' of type '{sname}' requires a subfield access"
                    ),
                    span,
                    format!(
                        "Access a specific field, e.g. '{layout}.{field}{}.{first}'",
                        if meta.is_array { "[i]" } else { "" }
                    ),
                ));
            }
            (None, Some(sf)) => {
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!(
                        "Field '{layout}.{field}' is a plain word, not a struct; cannot access '.{sf}'"
                    ),
                    span,
                    format!("Remove '.{sf}' or declare '{field}' with a struct type"),
                ));
            }
            (None, None) => 0,
        };

        let base_word = meta.offset + sub_off;
        match index {
            None => Ok(Expr::IntLit(base_word)),
            Some(idx_raw) => {
                let idx_expr = self.lower_expr(idx_raw, locals)?;
                let scaled = if meta.stride == 1 {
                    idx_expr
                } else {
                    Expr::Binary {
                        op: BinOp::Mul,
                        lhs: Box::new(idx_expr),
                        rhs: Box::new(Expr::IntLit(meta.stride)),
                    }
                };
                if base_word == 0 {
                    Ok(scaled)
                } else {
                    Ok(Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(Expr::IntLit(base_word)),
                        rhs: Box::new(scaled),
                    })
                }
            }
        }
    }

    fn register_item(&mut self, item: RawItem) -> Result<(), InfimumDiagnostic> {
        match item {
            RawItem::Const(rc) => {
                if self.consts.contains_key(&rc.name)
                    || self.enums.contains_key(&rc.name)
                    || self.layouts.contains_key(&rc.name)
                {
                    return Err(InfimumDiagnostic::new(
                        "E065_DUPLICATE_DECL",
                        "resolve",
                        format!("Duplicate top-level declaration '{}'", rc.name),
                        rc.span,
                        "Rename the constant so every top-level const/enum/layout name is unique",
                    ));
                }
                let val = self.eval_const(&rc.expr, rc.span)?;
                let matches_ty = matches!(
                    (&rc.ty, val),
                    (Type::I64, ConstValue::Int(_))
                        | (Type::Fixed, ConstValue::Fixed(_))
                        | (Type::Bool, ConstValue::Bool(_))
                );
                if !matches_ty {
                    return Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!(
                            "Constant '{}' declared as '{}' does not match initializer value",
                            rc.name, rc.ty
                        ),
                        rc.span,
                        "Ensure the constant's declared type matches its initializer expression",
                    ));
                }
                let def = ConstDef {
                    name: rc.name.clone(),
                    ty: rc.ty,
                    value: val,
                };
                self.consts.insert(rc.name, def.clone());
                self.consts_order.push(def);
            }
            RawItem::Enum(re) => {
                if self.consts.contains_key(&re.name)
                    || self.enums.contains_key(&re.name)
                    || self.layouts.contains_key(&re.name)
                {
                    return Err(InfimumDiagnostic::new(
                        "E065_DUPLICATE_DECL",
                        "resolve",
                        format!("Duplicate top-level declaration '{}'", re.name),
                        re.span,
                        "Rename the enum so every top-level const/enum/layout name is unique",
                    ));
                }
                let mut next_val: i64 = 0;
                let mut seen_variants = BTreeSet::new();
                let mut variants = Vec::with_capacity(re.variants.len());
                for rv in re.variants {
                    if !seen_variants.insert(rv.name.clone()) {
                        return Err(InfimumDiagnostic::new(
                            "E065_DUPLICATE_DECL",
                            "resolve",
                            format!("Duplicate variant '{}' in enum '{}'", rv.name, re.name),
                            rv.span,
                            "Ensure all enum variant names are unique",
                        ));
                    }
                    let val = match rv.value {
                        Some(expr) => self.eval_const_i64(
                            &expr,
                            rv.span,
                            &format!("enum variant {}::{}", re.name, rv.name),
                        )?,
                        None => next_val,
                    };
                    next_val = val.checked_add(1).ok_or_else(|| const_overflow("+", rv.span))?;
                    variants.push(EnumVariant {
                        name: rv.name,
                        value: val,
                    });
                }
                let def = EnumDef {
                    name: re.name.clone(),
                    variants,
                };
                self.enums.insert(re.name, def.clone());
                self.enums_order.push(def);
            }
            RawItem::Struct(rs) => {
                if self.consts.contains_key(&rs.name)
                    || self.enums.contains_key(&rs.name)
                    || self.structs.contains_key(&rs.name)
                    || self.layouts.contains_key(&rs.name)
                {
                    return Err(InfimumDiagnostic::new(
                        "E065_DUPLICATE_DECL",
                        "resolve",
                        format!("Duplicate top-level declaration '{}'", rs.name),
                        rs.span,
                        "Rename the struct so every top-level const/enum/struct/layout name is unique",
                    ));
                }
                if rs.fields.is_empty() {
                    return Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "layout",
                        format!("Struct '{}' must declare at least one field", rs.name),
                        rs.span,
                        "Add at least one 'field: word' inside the struct body",
                    ));
                }
                let mut fields = BTreeMap::new();
                let mut field_invs = BTreeMap::new();
                let mut field_fixed = BTreeSet::new();
                let mut field_names = Vec::with_capacity(rs.fields.len());
                for (idx, (fname, is_fixed, finv, fspan)) in rs.fields.into_iter().enumerate() {
                    if fields.insert(fname.clone(), idx as i64).is_some() {
                        return Err(InfimumDiagnostic::new(
                            "E065_DUPLICATE_DECL",
                            "layout",
                            format!("Duplicate field '{}' in struct '{}'", fname, rs.name),
                            fspan,
                            "Ensure all field names within a struct are unique",
                        ));
                    }
                    if is_fixed {
                        field_fixed.insert(fname.clone());
                    }
                    if let Some(ri) = finv {
                        let lo = self.eval_const_i64(&ri.lo, ri.span, "struct field invariant lower bound")?;
                        let hi_raw = self.eval_const_i64(&ri.hi, ri.span, "struct field invariant upper bound")?;
                        let hi = if ri.inclusive { hi_raw } else { hi_raw.saturating_sub(1) };
                        if lo > hi {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "layout",
                                format!("Empty invariant range {lo}..={hi} on struct field '{}::{}'", rs.name, fname),
                                ri.span,
                                "Ensure lower bound <= upper bound in 'inv lo..=hi'",
                            ));
                        }
                        field_invs.insert(fname.clone(), (lo, hi));
                    }
                    field_names.push(fname);
                }
                let size = field_names.len() as i64;
                self.structs.insert(
                    rs.name,
                    StructDef {
                        fields,
                        field_invs,
                        field_fixed,
                        field_names,
                        size,
                    },
                );
            }
            RawItem::Layout(rl) => {
                if self.consts.contains_key(&rl.name)
                    || self.enums.contains_key(&rl.name)
                    || self.structs.contains_key(&rl.name)
                    || self.layouts.contains_key(&rl.name)
                {
                    return Err(InfimumDiagnostic::new(
                        "E065_DUPLICATE_DECL",
                        "resolve",
                        format!("Duplicate top-level declaration '{}'", rl.name),
                        rl.span,
                        "Rename the layout so every top-level const/enum/struct/layout name is unique",
                    ));
                }
                let auto_placed = rl.base_expr.is_none();
                let base = match rl.base_expr {
                    Some(ref expr) => {
                        self.eval_const_i64(expr, rl.span, &format!("layout '{}' base", rl.name))?
                    }
                    None => self.auto_layout_cursor,
                };
                if base < 0 {
                    return Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "layout",
                        format!("Layout '{}' base offset ({base}) cannot be negative", rl.name),
                        rl.span,
                        "Specify a non-negative base word offset (e.g. 'at 0')",
                    ));
                }
                let mut cursor = base;
                let mut seen_fields = BTreeSet::new();
                let mut fields = Vec::with_capacity(rl.fields.len());
                for rf in rl.fields {
                    if !seen_fields.insert(rf.name.clone()) {
                        return Err(InfimumDiagnostic::new(
                            "E065_DUPLICATE_DECL",
                            "layout",
                            format!("Duplicate field '{}' in layout '{}'", rf.name, rl.name),
                            rf.span,
                            "Ensure all field names within a layout are unique",
                        ));
                    }
                    let stride = match &rf.struct_type {
                        Some(sname) => {
                            let sdef = self.structs.get(sname).ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E061_UNKNOWN_LAYOUT_FIELD",
                                    "layout",
                                    format!(
                                        "Unknown type '{}' for layout field '{}::{}'",
                                        sname, rl.name, rf.name
                                    ),
                                    rf.span,
                                    "Use 'word', 'words[N]', or declare 'struct TypeName { ... }' before this layout",
                                )
                            })?;
                            sdef.size
                        }
                        None => 1,
                    };
                    let count = match rf.len_expr {
                        Some(ref expr) => {
                            let l = self.eval_const_i64(
                                expr,
                                rf.span,
                                &format!("layout field '{}::{}' length", rl.name, rf.name),
                            )?;
                            if l <= 0 {
                                return Err(InfimumDiagnostic::new(
                                    "E064_CONST_EVAL",
                                    "layout",
                                    format!(
                                        "Layout buffer '{}::{}' length ({l}) must be positive",
                                        rl.name, rf.name
                                    ),
                                    rf.span,
                                    "Use a positive integer length for '[N]'",
                                ));
                            }
                            l
                        }
                        None => 1,
                    };
                    let len = count
                        .checked_mul(stride)
                        .ok_or_else(|| const_overflow("*", rf.span))?;
                    let offset = match rf.at_expr {
                        Some(ref expr) => {
                            let o = self.eval_const_i64(
                                expr,
                                rf.span,
                                &format!("layout field '{}::{}' offset", rl.name, rf.name),
                            )?;
                            if o < 0 {
                                return Err(InfimumDiagnostic::new(
                                    "E064_CONST_EVAL",
                                    "layout",
                                    format!(
                                        "Layout field '{}::{}' offset ({o}) cannot be negative",
                                        rl.name, rf.name
                                    ),
                                    rf.span,
                                    "Specify a non-negative word offset after 'at'",
                                ));
                            }
                            o
                        }
                        None => cursor,
                    };
                    let end = offset
                        .checked_add(len)
                        .ok_or_else(|| const_overflow("+", rf.span))?;
                    cursor = end;
                    self.layout_spans.push((
                        offset,
                        end,
                        format!("{}::{}", rl.name, rf.name),
                        rf.span,
                    ));
                    let field_inv = match rf.inv {
                        Some(ri) => {
                            let lo = self.eval_const_i64(&ri.lo, ri.span, "layout field invariant lower bound")?;
                            let hi_raw = self.eval_const_i64(&ri.hi, ri.span, "layout field invariant upper bound")?;
                            let hi = if ri.inclusive { hi_raw } else { hi_raw.saturating_sub(1) };
                            if lo > hi {
                                return Err(InfimumDiagnostic::new(
                                    "E064_CONST_EVAL",
                                    "layout",
                                    format!("Empty invariant range {lo}..={hi} on layout field '{}::{}'", rl.name, rf.name),
                                    ri.span,
                                    "Ensure lower bound <= upper bound in 'inv lo..=hi'",
                                ));
                            }
                            Some((lo, hi))
                        }
                        None => None,
                    };
                    self.layout_field_meta.insert(
                        (rl.name.clone(), rf.name.clone()),
                        LayoutFieldMeta {
                            offset,
                            is_array: rf.is_array,
                            is_fixed: rf.is_fixed,
                            struct_type: rf.struct_type,
                            stride,
                            count,
                            inv: field_inv,
                        },
                    );
                    fields.push(LayoutField {
                        name: rf.name,
                        offset,
                        len,
                        is_array: rf.is_array,
                    });
                }
                if auto_placed && cursor > self.auto_layout_cursor {
                    self.auto_layout_cursor = cursor;
                }
                let def = LayoutDef {
                    name: rl.name.clone(),
                    base,
                    fields,
                };
                self.layouts.insert(rl.name, def.clone());
                self.layouts_order.push(def);
            }
            RawItem::Mod(rm) => {
                let mname = rm.name.clone();
                let prev_mod = self.current_mod.replace(mname.clone());
                for sub_item in rm.items {
                    match sub_item {
                        RawItem::Const(rc) => {
                            let val = self.eval_const(&rc.expr, rc.span)?;
                            self.mod_consts
                                .insert((mname.clone(), rc.name), (val, rc.is_pub));
                        }
                        other => self.register_item(other)?,
                    }
                }
                for mut rf in rm.functions {
                    let orig_name = rf.name.clone();
                    let mangled = format!("{mname}__{orig_name}");
                    self.mod_fns
                        .insert((mname.clone(), orig_name), (mangled.clone(), rf.is_pub));
                    rf.name = mangled;
                    rf.module = Some(mname.clone());
                    self.mod_extra_fns.push(rf);
                }
                for mut f in rm.formas {
                    rewrite_forma_mod_slots(&mut f.root, &mname, &self.mod_fns);
                    self.mod_extra_formas.push(f);
                }
                self.current_mod = prev_mod;
            }
        }
        Ok(())
    }

    fn verify_no_layout_overlaps(&self) -> Result<(), InfimumDiagnostic> {
        // Protect std/gfx reserved runtime words (11 = cursor_kind, 12 = prim_count) in GPU apps
        if self.consts.contains_key("GPU_CMD_BYTE_OFFSET") {
            for (start, end, label, span) in &self.layout_spans {
                let field_name = label.rsplit("::").next().unwrap_or("");
                if *start <= 11 && 11 < *end && field_name != "cursor_kind" {
                    return Err(InfimumDiagnostic::new(
                        "E066_RESERVED_RUNTIME_ADDR",
                        "layout",
                        format!(
                            "Layout field '{label}' ([{start}..{end})) overwrites reserved std/gfx slot 11 (cursor_kind)"
                        ),
                        *span,
                        "Omit 'at 0' (layouts without 'at' auto-place safely at word 1000+) or explicitly pin 'cursor_kind: word at 11'",
                    ));
                }
                if *start <= 12 && 12 < *end && field_name != "prim_count" {
                    return Err(InfimumDiagnostic::new(
                        "E066_RESERVED_RUNTIME_ADDR",
                        "layout",
                        format!(
                            "Layout field '{label}' ([{start}..{end})) overwrites reserved std/gfx slot 12 (prim_count), which causes a black screen"
                        ),
                        *span,
                        "Omit 'at 0' (layouts without 'at' auto-place safely at word 1000+) or explicitly pin 'prim_count: word at 12'",
                    ));
                }
            }
        }

        const LINEAR_MEM_MAX_WORDS: i64 = 16_777_216; // Up to 1024 Wasm pages = 64 MiB (auto-sized 8/16/32/64 MiB)
        for (start, end, label, span) in &self.layout_spans {
            if *start < 0 || *end > LINEAR_MEM_MAX_WORDS {
                return Err(InfimumDiagnostic::new(
                    "E067_LAYOUT_OUT_OF_MEMORY",
                    "layout",
                    format!(
                        "Layout field '{label}' ([{start}..{end})) exceeds the 8 MiB linear memory ceiling ({LINEAR_MEM_MAX_WORDS} words)"
                    ),
                    *span,
                    format!(
                        "Place '{label}' strictly within [0..{LINEAR_MEM_MAX_WORDS}) words (e.g. shrink large buffers or lower its 'at' offset) to prevent address wrap-around corruption"
                    ),
                ));
            }
        }

        let mut intervals = self.layout_spans.clone();
        intervals.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut max_end: Option<(i64, i64, String)> = None;
        for (start, end, label, span) in intervals {
            if let Some((prev_start, prev_end, ref prev_label)) = max_end {
                if prev_end > start {
                    return Err(InfimumDiagnostic::new(
                        "E062_LAYOUT_OVERLAP",
                        "layout",
                        format!(
                            "Linear memory layout overlap: '{prev_label}' ([{prev_start}..{prev_end})) overlaps '{label}' ([{start}..{end}))"
                        ),
                        span,
                        format!(
                            "Move '{label}' to at least word offset {prev_end} (or shrink '{prev_label}') so all layout regions are strictly disjoint"
                        ),
                    ));
                }
            }
            let replace = match &max_end {
                Some((_, prev_end, _)) => end > *prev_end,
                None => true,
            };
            if replace {
                max_end = Some((start, end, label));
            }
        }
        Ok(())
    }

    fn lower_fn(&mut self, raw: RawFnDef) -> Result<FnDef, InfimumDiagnostic> {
        let prev_mod = self.current_mod.clone();
        self.current_mod = raw.module.clone();

        let mut params = Vec::with_capacity(raw.params.len());
        let mut init_struct_vars = BTreeMap::new();
        for (pname, raw_ty) in &raw.params {
            let resolved_ty = self.resolve_raw_type(raw_ty)?;
            if let RawType::Named(sname, _) = raw_ty {
                init_struct_vars.insert(pname.clone(), sname.clone());
            }
            params.push(Param {
                name: pname.clone(),
                ty: resolved_ty,
            });
        }
        let ret_type = self.resolve_raw_type(&raw.ret_type)?;
        if let RawType::Named(sname, _) = &raw.ret_type {
            init_struct_vars.insert("result".to_string(), sname.clone());
        }
        self.struct_vars.push(init_struct_vars);

        let mut locals: Vec<BTreeSet<String>> = vec![params.iter().map(|p| p.name.clone()).collect()];

        let mut requires = Vec::with_capacity(raw.requires.len());
        for r in &raw.requires {
            requires.push(self.lower_expr(r, &mut locals)?);
        }
        let mut ensures = Vec::with_capacity(raw.ensures.len());
        for e in &raw.ensures {
            ensures.push(self.lower_expr(e, &mut locals)?);
        }
        let body = self.lower_expr(&raw.body, &mut locals)?;
        self.struct_vars.pop();
        self.current_mod = prev_mod;

        Ok(FnDef {
            name: raw.name,
            exported: raw.exported,
            params,
            effects: raw.effects,
            ret_type,
            requires,
            ensures,
            body,
            source_file: raw.source_file,
            span: Some(raw.span),
        })
    }

    fn lookup_layout_inv(
        &self,
        layout: &str,
        field: &str,
        subfield: Option<&str>,
    ) -> Option<(i64, i64)> {
        let meta = self
            .layout_field_meta
            .get(&(layout.to_string(), field.to_string()))?;
        if let Some(sub) = subfield {
            let sname = meta.struct_type.as_deref()?;
            let sdef = self.structs.get(sname)?;
            sdef.field_invs.get(sub).copied()
        } else {
            meta.inv
        }
    }

    fn lookup_layout_fixed(
        &self,
        layout: &str,
        field: &str,
        subfield: Option<&str>,
    ) -> bool {
        let Some(meta) = self
            .layout_field_meta
            .get(&(layout.to_string(), field.to_string()))
        else {
            return false;
        };
        if let Some(sub) = subfield {
            let Some(sname) = meta.struct_type.as_deref() else {
                return false;
            };
            let Some(sdef) = self.structs.get(sname) else {
                return false;
            };
            sdef.field_fixed.contains(sub)
        } else {
            meta.is_fixed
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn desugar_ui_fmt(
        &mut self,
        fmt_str: &str,
        x_raw: &RawExpr,
        y_raw: &RawExpr,
        sz_raw: &RawExpr,
        st_raw: &RawExpr,
        col_raw: &RawExpr,
        clip_raw: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        enum Seg {
            Lit(String),
            Expr(RawExpr, Option<i64>),
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut lit = String::new();
        let chars: Vec<char> = fmt_str.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '{' {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    lit.push('{');
                    i += 2;
                    continue;
                }
                if !lit.is_empty() {
                    segs.push(Seg::Lit(std::mem::take(&mut lit)));
                }
                i += 1;
                let start = i;
                let mut depth = 1usize;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(InfimumDiagnostic::new(
                        "E070_INVALID_FMT_STRING",
                        "parse",
                        format!("Unclosed '{{' in ui_fmt string \"{fmt_str}\""),
                        Span::zero(),
                        "Close each placeholder with '}' (or escape a literal brace as '{{')",
                    ));
                }
                let expr_src: String = chars[start..i].iter().collect();
                i += 1; // skip '}'
                let (expr_body, fixed_prec) = if let Some((lhs, prec_str)) = expr_src.rsplit_once(":.") {
                    let prec = prec_str.trim().parse::<i64>().map_err(|_| {
                        InfimumDiagnostic::new(
                            "E070_INVALID_FMT_STRING",
                            "parse",
                            format!("Invalid precision specifier ':.{prec_str}' in ui_fmt string \"{fmt_str}\""),
                            Span::zero(),
                            "Use ':.0', ':.1', ':.2', ':.3', or ':.4' for fixed-point formatting",
                        )
                    })?;
                    (lhs.trim(), Some(prec.clamp(0, 4)))
                } else {
                    (expr_src.trim(), None)
                };
                let toks = tokenize(expr_body)?;
                let mut sub_parser = Parser { tokens: toks, idx: 0 };
                let parsed_expr = sub_parser.parse_expr()?;
                segs.push(Seg::Expr(parsed_expr, fixed_prec));
            } else if chars[i] == '}' && i + 1 < chars.len() && chars[i + 1] == '}' {
                lit.push('}');
                i += 2;
            } else {
                lit.push(chars[i]);
                i += 1;
            }
        }
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }

        let id = self.addr_counter;
        self.addr_counter += 1;
        let vx = format!("__ifm_fx_{id}");
        let vw = format!("__ifm_fw_{id}");
        let vy = format!("__ifm_fy_{id}");
        let vsz = format!("__ifm_fsz_{id}");
        let vst = format!("__ifm_fst_{id}");
        let vcol = format!("__ifm_fcol_{id}");
        let vclip = format!("__ifm_fclip_{id}");

        let mut stmts = vec![
            Stmt::Let {
                name: vx.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(x_raw, locals)?,
            },
            Stmt::Let {
                name: vw.clone(),
                mutable: true,
                ty: Some(Type::I64),
                value: Expr::IntLit(0),
            },
            Stmt::Let {
                name: vy.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(y_raw, locals)?,
            },
            Stmt::Let {
                name: vsz.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(sz_raw, locals)?,
            },
            Stmt::Let {
                name: vst.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(st_raw, locals)?,
            },
            Stmt::Let {
                name: vcol.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(col_raw, locals)?,
            },
            Stmt::Let {
                name: vclip.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(clip_raw, locals)?,
            },
        ];

        for seg in segs {
            let cur_x = Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Var(vx.clone())),
                rhs: Box::new(Expr::Var(vw.clone())),
            };
            let call = match seg {
                Seg::Lit(s) => Expr::Call {
                    target: TargetRef::Named("ui_text".to_string()),
                    args: vec![
                        cur_x,
                        Expr::Var(vy.clone()),
                        Expr::StrLit(s),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                        Expr::Var(vcol.clone()),
                        Expr::Var(vclip.clone()),
                    ],
                },
                Seg::Expr(e, Some(prec)) => Expr::Call {
                    target: TargetRef::Named("ui_fixed".to_string()),
                    args: vec![
                        cur_x,
                        Expr::Var(vy.clone()),
                        self.lower_expr(&e, locals)?,
                        Expr::IntLit(prec),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                        Expr::Var(vcol.clone()),
                        Expr::Var(vclip.clone()),
                    ],
                },
                Seg::Expr(e, None) => Expr::Call {
                    target: TargetRef::Named("ui_int".to_string()),
                    args: vec![
                        cur_x,
                        Expr::Var(vy.clone()),
                        self.lower_expr(&e, locals)?,
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                        Expr::Var(vcol.clone()),
                        Expr::Var(vclip.clone()),
                    ],
                },
            };
            stmts.push(Stmt::Assign {
                name: vw.clone(),
                value: Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Var(vw.clone())),
                    rhs: Box::new(call),
                },
            });
        }

        Ok(Expr::Block {
            stmts,
            tail: Box::new(Expr::Var(vw)),
        })
    }

    fn desugar_ui_fmt_width(
        &mut self,
        fmt_str: &str,
        sz_raw: &RawExpr,
        st_raw: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        enum Seg {
            Lit(String),
            Expr(RawExpr, Option<i64>),
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut lit = String::new();
        let chars: Vec<char> = fmt_str.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '{' {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    lit.push('{');
                    i += 2;
                    continue;
                }
                if !lit.is_empty() {
                    segs.push(Seg::Lit(std::mem::take(&mut lit)));
                }
                i += 1;
                let start = i;
                let mut depth = 1usize;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(InfimumDiagnostic::new(
                        "E070_INVALID_FMT_STRING",
                        "parse",
                        format!("Unclosed '{{' in ui_fmt_width string \"{fmt_str}\""),
                        Span::zero(),
                        "Close each placeholder with '}'",
                    ));
                }
                let expr_src: String = chars[start..i].iter().collect();
                i += 1;
                let (expr_body, fixed_prec) = if let Some((lhs, prec_str)) = expr_src.rsplit_once(":.") {
                    let prec = prec_str.trim().parse::<i64>().map_err(|_| {
                        InfimumDiagnostic::new(
                            "E070_INVALID_FMT_STRING",
                            "parse",
                            format!("Invalid precision specifier ':.{prec_str}' in ui_fmt_width string \"{fmt_str}\""),
                            Span::zero(),
                            "Use ':.0' .. ':.4' for fixed-point formatting",
                        )
                    })?;
                    (lhs.trim(), Some(prec.clamp(0, 4)))
                } else {
                    (expr_src.trim(), None)
                };
                let toks = tokenize(expr_body)?;
                let mut sub_parser = Parser { tokens: toks, idx: 0 };
                let parsed_expr = sub_parser.parse_expr()?;
                segs.push(Seg::Expr(parsed_expr, fixed_prec));
            } else if chars[i] == '}' && i + 1 < chars.len() && chars[i + 1] == '}' {
                lit.push('}');
                i += 2;
            } else {
                lit.push(chars[i]);
                i += 1;
            }
        }
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }

        let id = self.addr_counter;
        self.addr_counter += 1;
        let vw = format!("__ifm_fw_{id}");
        let vsz = format!("__ifm_fwsz_{id}");
        let vst = format!("__ifm_fwst_{id}");
        let mut stmts = vec![
            Stmt::Let {
                name: vw.clone(),
                mutable: true,
                ty: Some(Type::I64),
                value: Expr::IntLit(0),
            },
            Stmt::Let {
                name: vsz.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(sz_raw, locals)?,
            },
            Stmt::Let {
                name: vst.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(st_raw, locals)?,
            },
        ];
        for seg in segs {
            let add_call = match seg {
                Seg::Lit(s) => Expr::Call {
                    target: TargetRef::Named("ui_text_width".to_string()),
                    args: vec![Expr::StrLit(s), Expr::Var(vsz.clone()), Expr::Var(vst.clone())],
                },
                Seg::Expr(e, Some(prec)) => Expr::Call {
                    target: TargetRef::Named("ui_fixed_width".to_string()),
                    args: vec![
                        self.lower_expr(&e, locals)?,
                        Expr::IntLit(prec),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                    ],
                },
                Seg::Expr(e, None) => Expr::Call {
                    target: TargetRef::Named("ui_int_width".to_string()),
                    args: vec![
                        self.lower_expr(&e, locals)?,
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                    ],
                },
            };
            stmts.push(Stmt::Assign {
                name: vw.clone(),
                value: Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Var(vw.clone())),
                    rhs: Box::new(add_call),
                },
            });
        }
        Ok(Expr::Block {
            stmts,
            tail: Box::new(Expr::Var(vw)),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn desugar_ui_fmt_centered(
        &mut self,
        fmt_str: &str,
        cx_raw: &RawExpr,
        y_raw: &RawExpr,
        sz_raw: &RawExpr,
        st_raw: &RawExpr,
        col_raw: &RawExpr,
        clip_raw: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        let id = self.addr_counter;
        self.addr_counter += 1;
        let vtw = format!("__ifm_fctw_{id}");
        let vcx = format!("__ifm_fccx_{id}");
        let w_expr = self.desugar_ui_fmt_width(fmt_str, sz_raw, st_raw, locals)?;
        let start_x_raw = RawExpr::Binary {
            op: BinOp::Sub,
            lhs: Box::new(RawExpr::Var(vcx.clone())),
            rhs: Box::new(RawExpr::Binary {
                op: BinOp::Div,
                lhs: Box::new(RawExpr::Var(vtw.clone())),
                rhs: Box::new(RawExpr::IntLit(2)),
            }),
        };
        let draw_expr = self.desugar_ui_fmt(
            fmt_str,
            &start_x_raw,
            y_raw,
            sz_raw,
            st_raw,
            col_raw,
            clip_raw,
            locals,
        )?;
        Ok(Expr::Block {
            stmts: vec![
                Stmt::Let {
                    name: vcx,
                    mutable: false,
                    ty: Some(Type::I64),
                    value: self.lower_expr(cx_raw, locals)?,
                },
                Stmt::Let {
                    name: vtw,
                    mutable: false,
                    ty: Some(Type::I64),
                    value: w_expr,
                },
            ],
            tail: Box::new(draw_expr),
        })
    }

    fn is_shadowed(locals: &[BTreeSet<String>], name: &str) -> bool {
        locals.iter().any(|frame| frame.contains(name))
    }

    fn active_loop_guard(&self) -> Option<Expr> {
        let ctx = self.loop_stack.last()?;
        match (&ctx.brk_var, &ctx.cont_var) {
            (Some(b), Some(c)) => Some(Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(Expr::Var(b.clone())),
                }),
                rhs: Box::new(Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(Expr::Var(c.clone())),
                }),
            }),
            (Some(b), None) => Some(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Var(b.clone())),
            }),
            (None, Some(c)) => Some(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Var(c.clone())),
            }),
            (None, None) => None,
        }
    }

    fn lower_stmts(
        &mut self,
        stmts: &[RawStmt],
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Vec<Stmt>, InfimumDiagnostic> {
        let mut out = Vec::with_capacity(stmts.len());
        let mut idx = 0;
        while idx < stmts.len() {
            let s = &stmts[idx];
            match s {
                RawStmt::Let {
                    name,
                    mutable,
                    ty,
                    value,
                } => {
                    let resolved_ty = match ty {
                        Some(rt) => Some(self.resolve_raw_type(rt)?),
                        None => None,
                    };
                    let inferred_struct = match ty {
                        Some(RawType::Named(sname, _)) => Some(sname.clone()),
                        _ => self.infer_raw_expr_struct(value),
                    };
                    let v = self.lower_expr(value, locals)?;
                    if let Some(top) = locals.last_mut() {
                        top.insert(name.clone());
                    }
                    if let Some(sname) = inferred_struct {
                        if let Some(sv_top) = self.struct_vars.last_mut() {
                            sv_top.insert(name.clone(), sname);
                        }
                    }
                    out.push(Stmt::Let {
                        name: name.clone(),
                        mutable: *mutable,
                        ty: resolved_ty,
                        value: v,
                    });
                }
                RawStmt::LetTuple { names, value } => {
                    let v = self.lower_expr(value, locals)?;
                    if let Some(top) = locals.last_mut() {
                        for (n, _) in names {
                            top.insert(n.clone());
                        }
                    }
                    out.push(Stmt::LetTuple {
                        names: names.clone(),
                        value: v,
                    });
                }
                RawStmt::Assign { name, op, value } => {
                    let rhs = self.lower_expr(value, locals)?;
                    let final_val = match op {
                        None => rhs,
                        Some(binop) => Expr::Binary {
                            op: *binop,
                            lhs: Box::new(Expr::Var(name.clone())),
                            rhs: Box::new(rhs),
                        },
                    };
                    out.push(Stmt::Assign {
                        name: name.clone(),
                        value: final_val,
                    });
                }
                RawStmt::LayoutStore {
                    layout,
                    field,
                    index,
                    subfield,
                    op,
                    value,
                    span,
                } => {
                    // Case A: Local value-struct field mutation (`v.x = e;` / `v.x += e;`)
                    if index.is_none() && subfield.is_none() {
                        if let Some(sname) = self.lookup_struct_var(layout) {
                            let sdef = self.structs.get(&sname).cloned().ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E061_UNKNOWN_STRUCT",
                                    "parse",
                                    format!("Unknown struct '{sname}'"),
                                    *span,
                                    "Declare the struct before using it",
                                )
                            })?;
                            let fidx = *sdef.fields.get(field).ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E063_UNKNOWN_FIELD",
                                    "parse",
                                    format!("Struct '{sname}' has no field '{field}'"),
                                    *span,
                                    format!("Available fields on '{sname}': {}", sdef.field_names.join(", ")),
                                )
                            })? as usize;
                            let rhs = self.lower_expr(value, locals)?;
                            let new_fval = match op {
                                None => rhs,
                                Some(binop) => Expr::Binary {
                                    op: *binop,
                                    lhs: Box::new(Expr::TupleField {
                                        expr: Box::new(Expr::Var(layout.clone())),
                                        index: fidx,
                                    }),
                                    rhs: Box::new(rhs),
                                },
                            };
                            let mut elems = Vec::with_capacity(sdef.field_names.len());
                            for k in 0..sdef.field_names.len() {
                                if k == fidx {
                                    elems.push(new_fval.clone());
                                } else {
                                    elems.push(Expr::TupleField {
                                        expr: Box::new(Expr::Var(layout.clone())),
                                        index: k,
                                    });
                                }
                            }
                            out.push(Stmt::Assign {
                                name: layout.clone(),
                                value: Expr::TupleLit(elems),
                            });
                            idx += 1;
                            continue;
                        }
                    }
                    // Case B: Whole-struct store into layout (`World.hero = v;` or `World.flock[i] = s;`)
                    if subfield.is_none() && op.is_none() {
                        if let Some(meta) = self
                            .layout_field_meta
                            .get(&(layout.clone(), field.clone()))
                            .cloned()
                        {
                            if let Some(sname) = meta.struct_type {
                                let sdef = self.structs.get(&sname).cloned().ok_or_else(|| {
                                    InfimumDiagnostic::new(
                                        "E061_UNKNOWN_STRUCT",
                                        "layout",
                                        format!("Unknown struct '{sname}'"),
                                        *span,
                                        "Declare the struct before using it",
                                    )
                                })?;
                                let rhs = self.lower_expr(value, locals)?;
                                let sv_tmp = format!("__ifm_sv_{}", self.addr_counter);
                                self.addr_counter += 1;
                                out.push(Stmt::Let {
                                    name: sv_tmp.clone(),
                                    mutable: false,
                                    ty: Some(self.struct_tuple_type(&sdef)),
                                    value: rhs,
                                });
                                let base_addr_var = if let Some(idx_raw) = index {
                                    let idx_e = self.lower_expr(idx_raw, locals)?;
                                    let clamped = Expr::Call {
                                        target: TargetRef::Named("clamp_i64".to_string()),
                                        args: vec![
                                            idx_e,
                                            Expr::IntLit(0),
                                            Expr::IntLit((meta.count - 1).max(0)),
                                        ],
                                    };
                                    let base_expr = Expr::Binary {
                                        op: BinOp::Add,
                                        lhs: Box::new(Expr::IntLit(meta.offset)),
                                        rhs: Box::new(Expr::Binary {
                                            op: BinOp::Mul,
                                            lhs: Box::new(clamped),
                                            rhs: Box::new(Expr::IntLit(meta.stride)),
                                        }),
                                    };
                                    let bvar = format!("__ifm_saddr_{}", self.addr_counter);
                                    self.addr_counter += 1;
                                    out.push(Stmt::Let {
                                        name: bvar.clone(),
                                        mutable: false,
                                        ty: Some(Type::I64),
                                        value: base_expr,
                                    });
                                    Some(bvar)
                                } else {
                                    None
                                };
                                for (k, fname) in sdef.field_names.iter().enumerate() {
                                    let f_addr = if let Some(ref bvar) = base_addr_var {
                                        if k == 0 {
                                            Expr::Var(bvar.clone())
                                        } else {
                                            Expr::Binary {
                                                op: BinOp::Add,
                                                lhs: Box::new(Expr::Var(bvar.clone())),
                                                rhs: Box::new(Expr::IntLit(k as i64)),
                                            }
                                        }
                                    } else {
                                        Expr::IntLit(meta.offset + k as i64)
                                    };
                                    out.push(Stmt::Expr(Expr::MemStore {
                                        index: Box::new(f_addr),
                                        value: Box::new(Expr::TupleField {
                                            expr: Box::new(Expr::Var(sv_tmp.clone())),
                                            index: k,
                                        }),
                                        inv: sdef.field_invs.get(fname).copied(),
                                        fixed_ty: sdef.field_fixed.contains(fname),
                                    }));
                                }
                                idx += 1;
                                continue;
                            }
                        }
                    }
                    let addr_expr = self.lower_layout_addr_expr(
                        layout,
                        field,
                        index.as_ref(),
                        subfield.as_deref(),
                        locals,
                        *span,
                    )?;
                    let field_inv = self.lookup_layout_inv(layout, field, subfield.as_deref());
                    let field_fixed = self.lookup_layout_fixed(layout, field, subfield.as_deref());
                    let rhs = self.lower_expr(value, locals)?;
                    match op {
                        None => {
                            out.push(Stmt::Expr(Expr::MemStore {
                                index: Box::new(addr_expr),
                                value: Box::new(rhs),
                                inv: field_inv,
                                fixed_ty: field_fixed,
                            }));
                        }
                        Some(binop) => {
                            if index.is_some() {
                                let tmp = format!("__ifm_laddr_{}", self.addr_counter);
                                self.addr_counter += 1;
                                out.push(Stmt::Let {
                                    name: tmp.clone(),
                                    mutable: false,
                                    ty: Some(Type::I64),
                                    value: addr_expr,
                                });
                                let final_val = Expr::Binary {
                                    op: *binop,
                                    lhs: Box::new(Expr::MemLoad {
                                        index: Box::new(Expr::Var(tmp.clone())),
                                        inv: field_inv,
                                        fixed_ty: field_fixed,
                                    }),
                                    rhs: Box::new(rhs),
                                };
                                out.push(Stmt::Expr(Expr::MemStore {
                                    index: Box::new(Expr::Var(tmp)),
                                    value: Box::new(final_val),
                                    inv: field_inv,
                                    fixed_ty: field_fixed,
                                }));
                            } else {
                                let final_val = Expr::Binary {
                                    op: *binop,
                                    lhs: Box::new(Expr::MemLoad {
                                        index: Box::new(addr_expr.clone()),
                                        inv: field_inv,
                                        fixed_ty: field_fixed,
                                    }),
                                    rhs: Box::new(rhs),
                                };
                                out.push(Stmt::Expr(Expr::MemStore {
                                    index: Box::new(addr_expr),
                                    value: Box::new(final_val),
                                    inv: field_inv,
                                    fixed_ty: field_fixed,
                                }));
                            }
                        }
                    }
                }
                RawStmt::While {
                    cond,
                    max_iters,
                    body,
                    span,
                } => {
                    let max_val = self.eval_const_i64(max_iters, *span, "'while ... max' bound")?;
                    if max_val <= 0 || max_val > u32::MAX as i64 {
                        return Err(InfimumDiagnostic::new(
                            "E017_INVALID_LOOP_BOUND",
                            "parse",
                            format!("Loop bound 'max {max_val}' must be in range 1..={}", u32::MAX),
                            *span,
                            "Provide a positive 32-bit compile-time iteration bound after 'max'",
                        ));
                    }
                    let (has_brk, has_cont) = stmts_have_loop_ctrl(body);
                    let lid = self.loop_counter;
                    self.loop_counter += 1;
                    let brk_var = if has_brk {
                        let v = format!("__ifm_brk_{lid}");
                        out.push(Stmt::Let {
                            name: v.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                        Some(v)
                    } else {
                        None
                    };
                    let cont_var = if has_cont {
                        Some(format!("__ifm_cont_{lid}"))
                    } else {
                        None
                    };

                    let orig_c = self.lower_expr(cond, locals)?;
                    let c = match &brk_var {
                        Some(bv) => Expr::Binary {
                            op: BinOp::And,
                            lhs: Box::new(Expr::Unary {
                                op: UnaryOp::Not,
                                expr: Box::new(Expr::Var(bv.clone())),
                            }),
                            rhs: Box::new(orig_c),
                        },
                        None => orig_c,
                    };

                    locals.push(BTreeSet::new());
                    let mut b = Vec::new();
                    if let Some(cv) = &cont_var {
                        b.push(Stmt::Let {
                            name: cv.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                    }
                    self.loop_stack.push(LoopCtrlCtx {
                        brk_var: brk_var.clone(),
                        cont_var: cont_var.clone(),
                    });
                    b.extend(self.lower_stmts(body, locals)?);
                    self.loop_stack.pop();
                    locals.pop();

                    out.push(Stmt::While {
                        cond: c,
                        max_iters: max_val as u32,
                        body: b,
                    });
                }
                RawStmt::For {
                    var,
                    start,
                    end,
                    inclusive,
                    max_iters,
                    body,
                    span,
                } => {
                    let max_val: i64 = match max_iters {
                        Some(m_expr) => {
                            let mv = self.eval_const_i64(m_expr, *span, "'for ... max' bound")?;
                            if mv <= 0 || mv > u32::MAX as i64 {
                                return Err(InfimumDiagnostic::new(
                                    "E017_INVALID_LOOP_BOUND",
                                    "parse",
                                    format!("Loop bound 'max {mv}' must be in range 1..={}", u32::MAX),
                                    *span,
                                    "Provide a positive 32-bit compile-time iteration bound after 'max'",
                                ));
                            }
                            mv
                        }
                        None => {
                            let s_const = self.eval_const_i64(start, *span, "'for' range start");
                            let e_const = self.eval_const_i64(end, *span, "'for' range end");
                            match (s_const, e_const) {
                                (Ok(sv), Ok(ev)) => {
                                    let diff = ev.saturating_sub(sv) + if *inclusive { 1 } else { 0 };
                                    if diff > u32::MAX as i64 {
                                        return Err(InfimumDiagnostic::new(
                                            "E017_INVALID_LOOP_BOUND",
                                            "parse",
                                            format!("'for' range length ({diff}) exceeds u32::MAX"),
                                            *span,
                                            "Use a smaller static range or an explicit 'max N'",
                                        ));
                                    }
                                    diff.max(1)
                                }
                                _ => {
                                    return Err(InfimumDiagnostic::new(
                                        "E017_INVALID_LOOP_BOUND",
                                        "parse",
                                        "'for' loop with non-constant bounds requires an explicit 'max N'",
                                        *span,
                                        "Add a static upper bound: e.g. 'for i in 0..count max 128 { ... }'",
                                    ));
                                }
                            }
                        }
                    };

                    let lid = self.loop_counter;
                    self.loop_counter += 1;
                    let end_var = format!("__ifm_fend_{lid}");
                    let start_expr = self.lower_expr(start, locals)?;
                    let end_expr = self.lower_expr(end, locals)?;

                    let (has_brk, has_cont) = stmts_have_loop_ctrl(body);
                    let mut scope_stmts = Vec::new();
                    let brk_var = if has_brk {
                        let v = format!("__ifm_brk_{lid}");
                        scope_stmts.push(Stmt::Let {
                            name: v.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                        Some(v)
                    } else {
                        None
                    };
                    let cont_var = if has_cont {
                        Some(format!("__ifm_cont_{lid}"))
                    } else {
                        None
                    };

                    scope_stmts.push(Stmt::Let {
                        name: var.clone(),
                        mutable: true,
                        ty: Some(Type::I64),
                        value: start_expr,
                    });
                    scope_stmts.push(Stmt::Let {
                        name: end_var.clone(),
                        mutable: false,
                        ty: Some(Type::I64),
                        value: end_expr,
                    });

                    let cmp_op = if *inclusive { BinOp::Le } else { BinOp::Lt };
                    let range_cond = Expr::Binary {
                        op: cmp_op,
                        lhs: Box::new(Expr::Var(var.clone())),
                        rhs: Box::new(Expr::Var(end_var)),
                    };
                    let while_cond = match &brk_var {
                        Some(bv) => Expr::Binary {
                            op: BinOp::And,
                            lhs: Box::new(Expr::Unary {
                                op: UnaryOp::Not,
                                expr: Box::new(Expr::Var(bv.clone())),
                            }),
                            rhs: Box::new(range_cond),
                        },
                        None => range_cond,
                    };

                    locals.push(BTreeSet::new());
                    if let Some(top) = locals.last_mut() {
                        top.insert(var.clone());
                    }
                    let mut b = Vec::new();
                    if let Some(cv) = &cont_var {
                        b.push(Stmt::Let {
                            name: cv.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                    }
                    self.loop_stack.push(LoopCtrlCtx {
                        brk_var: brk_var.clone(),
                        cont_var: cont_var.clone(),
                    });
                    b.extend(self.lower_stmts(body, locals)?);
                    self.loop_stack.pop();
                    locals.pop();

                    let inc_stmt = Stmt::Assign {
                        name: var.clone(),
                        value: Expr::Binary {
                            op: BinOp::Add,
                            lhs: Box::new(Expr::Var(var.clone())),
                            rhs: Box::new(Expr::IntLit(1)),
                        },
                    };
                    match &brk_var {
                        Some(bv) => {
                            b.push(Stmt::Expr(Expr::If {
                                cond: Box::new(Expr::Unary {
                                    op: UnaryOp::Not,
                                    expr: Box::new(Expr::Var(bv.clone())),
                                }),
                                then_branch: Box::new(Expr::Block {
                                    stmts: vec![inc_stmt],
                                    tail: Box::new(Expr::UnitLit),
                                }),
                                else_branch: Box::new(Expr::UnitLit),
                            }));
                        }
                        None => b.push(inc_stmt),
                    }

                    scope_stmts.push(Stmt::While {
                        cond: while_cond,
                        max_iters: max_val as u32,
                        body: b,
                    });
                    out.push(Stmt::Expr(Expr::Block {
                        stmts: scope_stmts,
                        tail: Box::new(Expr::UnitLit),
                    }));
                }
                RawStmt::Break(span) => {
                    let Some(ctx) = self.loop_stack.last() else {
                        return Err(InfimumDiagnostic::new(
                            "E067_BREAK_OUTSIDE_LOOP",
                            "resolve",
                            "'break' can only be used inside a 'while' or 'for' loop",
                            *span,
                            "Move 'break' inside an enclosing loop body",
                        ));
                    };
                    if let Some(bv) = &ctx.brk_var {
                        out.push(Stmt::Assign {
                            name: bv.clone(),
                            value: Expr::BoolLit(true),
                        });
                    }
                }
                RawStmt::Continue(span) => {
                    let Some(ctx) = self.loop_stack.last() else {
                        return Err(InfimumDiagnostic::new(
                            "E067_CONTINUE_OUTSIDE_LOOP",
                            "resolve",
                            "'continue' can only be used inside a 'while' or 'for' loop",
                            *span,
                            "Move 'continue' inside an enclosing loop body",
                        ));
                    };
                    if let Some(cv) = &ctx.cont_var {
                        out.push(Stmt::Assign {
                            name: cv.clone(),
                            value: Expr::BoolLit(true),
                        });
                    }
                }
                RawStmt::Expr(e) => {
                    out.push(Stmt::Expr(self.lower_expr(e, locals)?));
                }
            }

            // If this statement may trigger break/continue and more statements follow, guard the remainder
            let (sb, sc) = stmt_has_loop_ctrl(s);
            if (sb || sc) && idx + 1 < stmts.len() {
                if let Some(guard) = self.active_loop_guard() {
                    let rest = self.lower_stmts(&stmts[idx + 1..], locals)?;
                    out.push(Stmt::Expr(Expr::If {
                        cond: Box::new(guard),
                        then_branch: Box::new(Expr::Block {
                            stmts: rest,
                            tail: Box::new(Expr::UnitLit),
                        }),
                        else_branch: Box::new(Expr::UnitLit),
                    }));
                    break;
                }
            }
            idx += 1;
        }
        Ok(out)
    }

    fn lower_expr(
        &mut self,
        expr: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        match expr {
            RawExpr::IntLit(v) => Ok(Expr::IntLit(*v)),
            RawExpr::FixedLit(v) => Ok(Expr::FixedLit(*v)),
            RawExpr::BoolLit(b) => Ok(Expr::BoolLit(*b)),
            RawExpr::StrLit(s) => Ok(Expr::StrLit(s.clone())),
            RawExpr::UnitLit => Ok(Expr::UnitLit),
            RawExpr::Var(name) => {
                if !Self::is_shadowed(locals, name) {
                    if let Some(ref cur_m) = self.current_mod {
                        if let Some((val, _)) = self.mod_consts.get(&(cur_m.clone(), name.clone())) {
                            return Ok(match val {
                                ConstValue::Int(v) => Expr::IntLit(*v),
                                ConstValue::Fixed(v) => Expr::FixedLit(*v),
                                ConstValue::Bool(b) => Expr::BoolLit(*b),
                            });
                        }
                    }
                    if let Some(cdef) = self.consts.get(name) {
                        return Ok(match cdef.value {
                            ConstValue::Int(v) => Expr::IntLit(v),
                            ConstValue::Fixed(v) => Expr::FixedLit(v),
                            ConstValue::Bool(b) => Expr::BoolLit(b),
                        });
                    }
                }
                Ok(Expr::Var(name.clone()))
            }
            RawExpr::Path2 { head, member, span } => {
                let val = self.resolve_path2(head, member, *span)?;
                Ok(Expr::IntLit(val))
            }
            RawExpr::Path3 {
                head,
                member,
                prop,
                span,
            } => {
                let val = self.resolve_path3(head, member, prop, *span)?;
                Ok(Expr::IntLit(val))
            }
            RawExpr::LayoutLoad {
                layout,
                field,
                index,
                subfield,
                span,
            } => {
                // Case A: Local value-struct field read (`v.x`)
                if index.is_none() && subfield.is_none() {
                    if let Some(sname) = self.lookup_struct_var(layout) {
                        let sdef = self.structs.get(&sname).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E061_UNKNOWN_STRUCT",
                                "parse",
                                format!("Unknown struct '{sname}'"),
                                *span,
                                "Declare the struct before using it",
                            )
                        })?;
                        let fidx = *sdef.fields.get(field).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E063_UNKNOWN_FIELD",
                                "parse",
                                format!("Struct '{sname}' has no field '{field}'"),
                                *span,
                                format!(
                                    "Available fields on '{sname}': {}",
                                    sdef.field_names.join(", ")
                                ),
                            )
                        })? as usize;
                        return Ok(Expr::TupleField {
                            expr: Box::new(Expr::Var(layout.clone())),
                            index: fidx,
                        });
                    }
                }
                // Case B: Whole-struct load from layout (`let h = World.hero;` or `let s = World.flock[i];`)
                if subfield.is_none() {
                    if let Some(meta) = self
                        .layout_field_meta
                        .get(&(layout.clone(), field.clone()))
                        .cloned()
                    {
                        if let Some(sname) = meta.struct_type {
                            let sdef = self.structs.get(&sname).cloned().ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E061_UNKNOWN_STRUCT",
                                    "layout",
                                    format!("Unknown struct '{sname}'"),
                                    *span,
                                    "Declare the struct before using it",
                                )
                            })?;
                            if let Some(idx_raw) = index {
                                let idx_e = self.lower_expr(idx_raw, locals)?;
                                let clamped = Expr::Call {
                                    target: TargetRef::Named("clamp_i64".to_string()),
                                    args: vec![
                                        idx_e,
                                        Expr::IntLit(0),
                                        Expr::IntLit((meta.count - 1).max(0)),
                                    ],
                                };
                                let base_expr = Expr::Binary {
                                    op: BinOp::Add,
                                    lhs: Box::new(Expr::IntLit(meta.offset)),
                                    rhs: Box::new(Expr::Binary {
                                        op: BinOp::Mul,
                                        lhs: Box::new(clamped),
                                        rhs: Box::new(Expr::IntLit(meta.stride)),
                                    }),
                                };
                                let bvar = format!("__ifm_sladdr_{}", self.addr_counter);
                                self.addr_counter += 1;
                                let mut loads = Vec::with_capacity(sdef.field_names.len());
                                for (k, fname) in sdef.field_names.iter().enumerate() {
                                    let f_addr = if k == 0 {
                                        Expr::Var(bvar.clone())
                                    } else {
                                        Expr::Binary {
                                            op: BinOp::Add,
                                            lhs: Box::new(Expr::Var(bvar.clone())),
                                            rhs: Box::new(Expr::IntLit(k as i64)),
                                        }
                                    };
                                    loads.push(Expr::MemLoad {
                                        index: Box::new(f_addr),
                                        inv: sdef.field_invs.get(fname).copied(),
                                        fixed_ty: sdef.field_fixed.contains(fname),
                                    });
                                }
                                return Ok(Expr::Block {
                                    stmts: vec![Stmt::Let {
                                        name: bvar,
                                        mutable: false,
                                        ty: Some(Type::I64),
                                        value: base_expr,
                                    }],
                                    tail: Box::new(Expr::TupleLit(loads)),
                                });
                            } else {
                                let mut loads = Vec::with_capacity(sdef.field_names.len());
                                for (k, fname) in sdef.field_names.iter().enumerate() {
                                    loads.push(Expr::MemLoad {
                                        index: Box::new(Expr::IntLit(meta.offset + k as i64)),
                                        inv: sdef.field_invs.get(fname).copied(),
                                        fixed_ty: sdef.field_fixed.contains(fname),
                                    });
                                }
                                return Ok(Expr::TupleLit(loads));
                            }
                        }
                    }
                }
                let addr_expr = self.lower_layout_addr_expr(
                    layout,
                    field,
                    index.as_deref(),
                    subfield.as_deref(),
                    locals,
                    *span,
                )?;
                let field_inv = self.lookup_layout_inv(layout, field, subfield.as_deref());
                let field_fixed = self.lookup_layout_fixed(layout, field, subfield.as_deref());
                Ok(Expr::MemLoad {
                    index: Box::new(addr_expr),
                    inv: field_inv,
                    fixed_ty: field_fixed,
                })
            }
            RawExpr::Unary { op, expr: inner } => Ok(Expr::Unary {
                op: *op,
                expr: Box::new(self.lower_expr(inner, locals)?),
            }),
            RawExpr::Binary { op, lhs, rhs } => Ok(Expr::Binary {
                op: *op,
                lhs: Box::new(self.lower_expr(lhs, locals)?),
                rhs: Box::new(self.lower_expr(rhs, locals)?),
            }),
            RawExpr::If {
                cond,
                then_branch,
                else_branch,
            } => Ok(Expr::If {
                cond: Box::new(self.lower_expr(cond, locals)?),
                then_branch: Box::new(self.lower_expr(then_branch, locals)?),
                else_branch: Box::new(self.lower_expr(else_branch, locals)?),
            }),
            RawExpr::Match {
                scrutinee,
                arms,
                span,
            } => self.desugar_match(scrutinee, arms, *span, locals),
            RawExpr::Call { target, args } => {
                if args.len() == 1 {
                    if let TargetRef::Named(n) = target {
                        let fixed_uop = match n.as_str() {
                            "fixed" => Some(UnaryOp::IntToFixed),
                            "fixed_to_int" => Some(UnaryOp::FixedToInt),
                            "fixed_raw" => Some(UnaryOp::FixedToRaw),
                            "fixed_from_raw" => Some(UnaryOp::RawToFixed),
                            _ => None,
                        };
                        if let Some(op) = fixed_uop {
                            let inner = self.lower_expr(&args[0], locals)?;
                            return Ok(Expr::Unary {
                                op,
                                expr: Box::new(inner),
                            });
                        }
                    }
                }
                if matches!(target, TargetRef::Named(n) if n == "ui_fmt")
                    && args.len() == 7
                {
                    if let RawExpr::StrLit(fmt_str) = &args[2] {
                        return self.desugar_ui_fmt(
                            fmt_str,
                            &args[0],
                            &args[1],
                            &args[3],
                            &args[4],
                            &args[5],
                            &args[6],
                            locals,
                        );
                    }
                }
                if matches!(target, TargetRef::Named(n) if n == "ui_fmt_centered")
                    && args.len() == 7
                {
                    if let RawExpr::StrLit(fmt_str) = &args[2] {
                        return self.desugar_ui_fmt_centered(
                            fmt_str,
                            &args[0],
                            &args[1],
                            &args[3],
                            &args[4],
                            &args[5],
                            &args[6],
                            locals,
                        );
                    }
                }
                if matches!(target, TargetRef::Named(n) if n == "ui_fmt_width")
                    && args.len() == 3
                {
                    if let RawExpr::StrLit(fmt_str) = &args[0] {
                        return self.desugar_ui_fmt_width(fmt_str, &args[1], &args[2], locals);
                    }
                }
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                let resolved_target = match target {
                    TargetRef::Named(fname) => {
                        if let Some(ref cur_m) = self.current_mod {
                            if let Some((mangled, _)) =
                                self.mod_fns.get(&(cur_m.clone(), fname.clone()))
                            {
                                TargetRef::Named(mangled.clone())
                            } else {
                                TargetRef::Named(fname.clone())
                            }
                        } else {
                            TargetRef::Named(fname.clone())
                        }
                    }
                    other => other.clone(),
                };
                Ok(Expr::Call {
                    target: resolved_target,
                    args: largs,
                })
            }
            RawExpr::ModCall {
                module,
                func,
                args,
                span,
            } => {
                let Some((mangled, is_pub)) = self
                    .mod_fns
                    .get(&(module.clone(), func.clone()))
                    .cloned()
                else {
                    return Err(InfimumDiagnostic::new(
                        "E069_UNKNOWN_MOD_SYMBOL",
                        "resolve",
                        format!("Unknown function '{module}::{func}'"),
                        *span,
                        format!("Declare 'pub fn {func}(...)' inside 'mod {module}'"),
                    ));
                };
                if !is_pub && self.current_mod.as_deref() != Some(module.as_str()) {
                    return Err(InfimumDiagnostic::new(
                        "E068_PRIVATE_SYMBOL",
                        "resolve",
                        format!("Function '{module}::{func}' is private to module '{module}'"),
                        *span,
                        format!("Mark it 'pub fn {func}' inside 'mod {module}' to call it externally"),
                    ));
                }
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                Ok(Expr::Call {
                    target: TargetRef::Named(mangled),
                    args: largs,
                })
            }
            RawExpr::Perform { effect, op, args } => {
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                Ok(Expr::Perform {
                    effect: effect.clone(),
                    op: op.clone(),
                    args: largs,
                })
            }
            RawExpr::MemLoad { index } => Ok(Expr::MemLoad {
                index: Box::new(self.lower_expr(index, locals)?),
                inv: None,
                fixed_ty: false,
            }),
            RawExpr::MemStore { index, value } => Ok(Expr::MemStore {
                index: Box::new(self.lower_expr(index, locals)?),
                value: Box::new(self.lower_expr(value, locals)?),
                inv: None,
                fixed_ty: false,
            }),
            RawExpr::SecretWrap(inner) => {
                Ok(Expr::SecretWrap(Box::new(self.lower_expr(inner, locals)?)))
            }
            RawExpr::Reveal { domain, expr: inner } => Ok(Expr::Reveal {
                domain: domain.clone(),
                expr: Box::new(self.lower_expr(inner, locals)?),
            }),
            RawExpr::Block { stmts, tail } => {
                locals.push(BTreeSet::new());
                self.struct_vars.push(BTreeMap::new());
                let lstmts = self.lower_stmts(stmts, locals)?;
                let ltail = self.lower_expr(tail, locals)?;
                self.struct_vars.pop();
                locals.pop();
                if lstmts.is_empty() {
                    Ok(ltail)
                } else {
                    Ok(Expr::Block {
                        stmts: lstmts,
                        tail: Box::new(ltail),
                    })
                }
            }
            RawExpr::TupleLit(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.lower_expr(e, locals)?);
                }
                Ok(Expr::TupleLit(out))
            }
            RawExpr::TupleField { expr, index } => Ok(Expr::TupleField {
                expr: Box::new(self.lower_expr(expr, locals)?),
                index: *index,
            }),
            RawExpr::StructLit { name, fields, span } => {
                let sdef = self.structs.get(name).cloned().ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_STRUCT",
                        "parse",
                        format!("Unknown struct '{name}'"),
                        *span,
                        "Declare the struct with 'struct Name { field: i64, ... }' before constructing it",
                    )
                })?;
                let mut provided: BTreeMap<String, Expr> = BTreeMap::new();
                for (fname, fraw, fspan) in fields {
                    if !sdef.fields.contains_key(fname) {
                        return Err(InfimumDiagnostic::new(
                            "E063_UNKNOWN_FIELD",
                            "parse",
                            format!("Struct '{name}' has no field '{fname}'"),
                            *fspan,
                            format!(
                                "Available fields on '{name}': {}",
                                sdef.field_names.join(", ")
                            ),
                        ));
                    }
                    if provided.contains_key(fname) {
                        return Err(InfimumDiagnostic::new(
                            "E063_DUPLICATE_FIELD",
                            "parse",
                            format!("Duplicate field '{fname}' in struct literal '{name}'"),
                            *fspan,
                            "Specify each struct field at most once",
                        ));
                    }
                    let lowered = self.lower_expr(fraw, locals)?;
                    provided.insert(fname.clone(), lowered);
                }
                let mut missing = Vec::new();
                let mut ordered = Vec::with_capacity(sdef.field_names.len());
                for fname in &sdef.field_names {
                    if let Some(val) = provided.remove(fname) {
                        ordered.push(val);
                    } else {
                        missing.push(fname.clone());
                    }
                }
                if !missing.is_empty() {
                    return Err(InfimumDiagnostic::new(
                        "E063_MISSING_FIELD",
                        "parse",
                        format!(
                            "Struct literal '{name}' is missing field(s): {}",
                            missing.join(", ")
                        ),
                        *span,
                        format!(
                            "Provide all fields of '{name}': {}",
                            sdef.field_names.join(", ")
                        ),
                    ));
                }
                Ok(Expr::TupleLit(ordered))
            }
            RawExpr::FieldAccess { expr, field, span } => {
                let sname = self.infer_raw_expr_struct(expr).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E063_UNKNOWN_FIELD",
                        "parse",
                        format!("Cannot access field '.{field}' on non-struct expression"),
                        *span,
                        "Field access '.name' is supported on struct values (e.g. 'let v = Vec3 { ... }; v.x' or 'add(a, b).x')",
                    )
                })?;
                let sdef = self.structs.get(&sname).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_STRUCT",
                        "parse",
                        format!("Unknown struct '{sname}'"),
                        *span,
                        "Declare the struct before using it",
                    )
                })?;
                let fidx = *sdef.fields.get(field).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E063_UNKNOWN_FIELD",
                        "parse",
                        format!("Struct '{sname}' has no field '{field}'"),
                        *span,
                        format!(
                            "Available fields on '{sname}': {}",
                            sdef.field_names.join(", ")
                        ),
                    )
                })? as usize;
                let lowered = self.lower_expr(expr, locals)?;
                Ok(Expr::TupleField {
                    expr: Box::new(lowered),
                    index: fidx,
                })
            }
        }
    }

    fn desugar_match(
        &mut self,
        scrutinee: &RawExpr,
        arms: &[RawMatchArm],
        span: Span,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        if arms.is_empty() {
            return Err(InfimumDiagnostic::new(
                "E063_NON_EXHAUSTIVE_MATCH",
                "resolve",
                "Empty 'match' expression is not exhaustive",
                span,
                "Add at least one arm ending with a wildcard '_ => ...' or covering all enum variants",
            ));
        }

        enum ResolvedPat {
            Wildcard,
            Int(i64),
            Bool(bool),
            Enum {
                enum_name: String,
                variant: String,
                value: i64,
            },
        }

        let mut resolved_arms: Vec<(Vec<ResolvedPat>, Expr)> = Vec::with_capacity(arms.len());
        let mut wildcard_arm_idx: Option<usize> = None;

        for (arm_idx, arm) in arms.iter().enumerate() {
            let mut rpats = Vec::with_capacity(arm.patterns.len());
            for pat in &arm.patterns {
                let rp = match pat {
                    RawMatchPattern::Wildcard(pspan) => {
                        if arm_idx + 1 != arms.len() {
                            return Err(InfimumDiagnostic::new(
                                "E063_UNREACHABLE_MATCH_ARM",
                                "resolve",
                                "Wildcard '_' arm must be the final arm of a 'match' expression",
                                *pspan,
                                "Move the '_ => ...' fallback arm to the end of the match block",
                            ));
                        }
                        wildcard_arm_idx = Some(arm_idx);
                        ResolvedPat::Wildcard
                    }
                    RawMatchPattern::Int(v) => ResolvedPat::Int(*v),
                    RawMatchPattern::Bool(b) => ResolvedPat::Bool(*b),
                    RawMatchPattern::ConstRef(name, pspan) => {
                        let cdef = self.consts.get(name).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "resolve",
                                format!("Unknown constant '{name}' in match pattern"),
                                *pspan,
                                "Use a declared 'const', literal, Enum::Variant, or '_' wildcard in match patterns",
                            )
                        })?;
                        match cdef.value {
                            ConstValue::Int(v) | ConstValue::Fixed(v) => ResolvedPat::Int(v),
                            ConstValue::Bool(b) => ResolvedPat::Bool(b),
                        }
                    }
                    RawMatchPattern::Path2(head, member, pspan) => {
                        if let Some(edef) = self.enums.get(head) {
                            let v = edef
                                .variants
                                .iter()
                                .find(|v| v.name == *member)
                                .ok_or_else(|| {
                                    InfimumDiagnostic::new(
                                        "E061_UNKNOWN_ENUM_VARIANT",
                                        "resolve",
                                        format!("Enum '{head}' has no variant '{member}'"),
                                        *pspan,
                                        format!("Check the variants declared in 'enum {head}'"),
                                    )
                                })?;
                            ResolvedPat::Enum {
                                enum_name: head.clone(),
                                variant: member.clone(),
                                value: v.value,
                            }
                        } else {
                            let v = self.resolve_path2(head, member, *pspan)?;
                            ResolvedPat::Int(v)
                        }
                    }
                    RawMatchPattern::Path3(head, member, prop, pspan) => {
                        let v = self.resolve_path3(head, member, prop, *pspan)?;
                        ResolvedPat::Int(v)
                    }
                };
                rpats.push(rp);
            }
            let body = self.lower_expr(&arm.body, locals)?;
            resolved_arms.push((rpats, body));
        }

        // Verify exhaustiveness when there is no wildcard arm
        if wildcard_arm_idx.is_none() {
            let mut all_bool = true;
            let mut seen_true = false;
            let mut seen_false = false;
            let mut single_enum: Option<String> = None;
            let mut all_same_enum = true;
            let mut covered_variants: BTreeSet<String> = BTreeSet::new();

            for (pats, _) in &resolved_arms {
                for p in pats {
                    match p {
                        ResolvedPat::Bool(true) => {
                            seen_true = true;
                            all_same_enum = false;
                        }
                        ResolvedPat::Bool(false) => {
                            seen_false = true;
                            all_same_enum = false;
                        }
                        ResolvedPat::Enum {
                            enum_name, variant, ..
                        } => {
                            all_bool = false;
                            match &single_enum {
                                None => single_enum = Some(enum_name.clone()),
                                Some(prev) if prev != enum_name => all_same_enum = false,
                                Some(_) => {}
                            }
                            covered_variants.insert(variant.clone());
                        }
                        ResolvedPat::Int(_) | ResolvedPat::Wildcard => {
                            all_bool = false;
                            all_same_enum = false;
                        }
                    }
                }
            }

            let exhaustive = if all_bool && seen_true && seen_false {
                true
            } else if all_same_enum {
                if let Some(ref ename) = single_enum {
                    if let Some(edef) = self.enums.get(ename) {
                        let missing: Vec<String> = edef
                            .variants
                            .iter()
                            .filter(|v| !covered_variants.contains(&v.name))
                            .map(|v| format!("{ename}::{}", v.name))
                            .collect();
                        if !missing.is_empty() {
                            return Err(InfimumDiagnostic::new(
                                "E063_NON_EXHAUSTIVE_MATCH",
                                "resolve",
                                format!(
                                    "Non-exhaustive 'match' on enum '{ename}': missing variant(s) {}",
                                    missing.join(", ")
                                ),
                                span,
                                "Cover all enum variants or add a wildcard fallback arm '_ => ...'",
                            ));
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !exhaustive {
                return Err(InfimumDiagnostic::new(
                    "E063_NON_EXHAUSTIVE_MATCH",
                    "resolve",
                    "Non-exhaustive 'match' expression: missing wildcard fallback '_ => ...'",
                    span,
                    "Add a wildcard fallback arm '_ => ...' (or cover all variants of a single enum / both bool values)",
                ));
            }
        }

        let lowered_scrut = self.lower_expr(scrutinee, locals)?;
        let scrut_var = format!("$match_{}", self.match_counter);
        self.match_counter += 1;

        // Build nested if/else chain from the last arm backwards
        let (_, last_body) = resolved_arms.pop().unwrap();
        let mut acc = last_body;

        for (pats, arm_body) in resolved_arms.into_iter().rev() {
            let mut cond_opt: Option<Expr> = None;
            for p in pats {
                let lit = match p {
                    ResolvedPat::Int(v) | ResolvedPat::Enum { value: v, .. } => Expr::IntLit(v),
                    ResolvedPat::Bool(b) => Expr::BoolLit(b),
                    ResolvedPat::Wildcard => continue,
                };
                let eq = Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var(scrut_var.clone())),
                    rhs: Box::new(lit),
                };
                cond_opt = Some(match cond_opt {
                    None => eq,
                    Some(prev) => Expr::Binary {
                        op: BinOp::Or,
                        lhs: Box::new(prev),
                        rhs: Box::new(eq),
                    },
                });
            }
            if let Some(cond) = cond_opt {
                acc = Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(arm_body),
                    else_branch: Box::new(acc),
                };
            }
        }

        Ok(Expr::Block {
            stmts: vec![Stmt::Let {
                name: scrut_var,
                mutable: false,
                ty: None,
                value: lowered_scrut,
            }],
            tail: Box::new(acc),
        })
    }
}

fn const_overflow(op: &str, span: Span) -> InfimumDiagnostic {
    InfimumDiagnostic::new(
        "E064_CONST_EVAL",
        "const-eval",
        format!("Signed 64-bit overflow evaluating '{op}' in constant expression"),
        span,
        "Keep constant arithmetic within signed 64-bit integer bounds",
    )
}

impl crate::forma::parser::FormaHostParser for Parser {
    type Expr = RawExpr;

    fn peek(&self) -> &Token {
        Parser::peek(self)
    }

    fn peek_next_is_colon(&self) -> bool {
        *self.peek_at(1) == TokenKind::Colon
    }

    fn advance(&mut self) -> Token {
        Parser::advance(self)
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, InfimumDiagnostic> {
        let (name, _) = Parser::expect_ident(self, what)?;
        Ok(name)
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, InfimumDiagnostic> {
        self.expect(&TokenKind::LParen, "Expected '(' to start parameter list")?;
        let mut params = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                let (pname, _) = Parser::expect_ident(self, "Expected parameter name")?;
                self.expect(&TokenKind::Colon, "Expected ':' after parameter name")?;
                let pty = self.parse_type()?;
                params.push(Param {
                    name: pname,
                    ty: pty,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "Expected ')' after parameter list")?;
        Ok(params)
    }

    fn parse_effect_and_ret(&mut self) -> Result<(Vec<EffectCap>, Type), InfimumDiagnostic> {
        let mut effects = Vec::new();
        match &self.peek().kind {
            TokenKind::Arrow => {
                self.advance();
            }
            TokenKind::EffectOpen => {
                self.advance();
                if !matches!(self.peek().kind, TokenKind::EffectClose) {
                    loop {
                        let (ename, _) = Parser::expect_ident(
                            self,
                            "Expected effect capability name inside '-[...]->'",
                        )?;
                        let scope = if matches!(self.peek().kind, TokenKind::LParen) {
                            self.advance();
                            let s = match self.peek().kind.clone() {
                                TokenKind::Str(val) => {
                                    self.advance();
                                    val
                                }
                                other => {
                                    return Err(InfimumDiagnostic::new(
                                        "E012_EXPECTED_SCOPE_STR",
                                        "parse",
                                        format!("Expected string literal scope, found {other:?}"),
                                        self.peek().span,
                                        "Write capability scope as a string literal, e.g. Fs(\"/tmp\")",
                                    ));
                                }
                            };
                            self.expect(
                                &TokenKind::RParen,
                                "Expected ')' after capability scope literal",
                            )?;
                            Some(s)
                        } else {
                            None
                        };
                        effects.push(EffectCap { name: ename, scope });
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(
                    &TokenKind::EffectClose,
                    "Close capability list with ']->' before return type",
                )?;
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E013_EXPECTED_RETURN_ARROW",
                    "parse",
                    format!("Expected '->' or '-[...]->', found {other:?}"),
                    self.peek().span,
                    "Specify return type with '-> <Type>' or effects + return type with '-[Mem]-> <Type>'",
                ));
            }
        }
        let ret_type = self.parse_type()?;
        Ok((effects, ret_type))
    }

    fn parse_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        Parser::parse_expr(self)
    }
}

fn resolve_and_lower(
    items: Vec<RawItem>,
    mut raw_formas: Vec<crate::forma::FormaDef<RawExpr>>,
    mut raw_fns: Vec<RawFnDef>,
) -> Result<Module, InfimumDiagnostic> {
    let mut resolver = ModuleResolver::new();
    for item in items {
        resolver.register_item(item)?;
    }
    raw_formas.extend(std::mem::take(&mut resolver.mod_extra_formas));
    raw_fns.extend(std::mem::take(&mut resolver.mod_extra_fns));
    resolver.verify_no_layout_overlaps()?;

    let forma_defs_by_name: BTreeMap<String, crate::forma::FormaDef<RawExpr>> = raw_formas
        .iter()
        .map(|f| (f.name.clone(), f.clone()))
        .collect();

    let mut formas = Vec::with_capacity(raw_formas.len());
    for mut rf in raw_formas {
        let mut visiting = Vec::new();
        visiting.push(rf.name.clone());
        let expanded_roots = expand_forma_node(
            rf.root,
            &forma_defs_by_name,
            &resolver,
            &mut visiting,
        )?;
        rf.root = if expanded_roots.len() == 1 {
            expanded_roots.into_iter().next().unwrap()
        } else {
            crate::forma::FormaNode {
                kind: crate::forma::FormaNodeKind::Box,
                props: crate::forma::FormaProps::default(),
                children: expanded_roots,
                span: rf.span,
                component_call: None,
                forma_for: None,
                forma_if: None,
            }
        };
        let param_set: BTreeSet<String> = rf.params.iter().map(|p| p.name.clone()).collect();
        let mut param_locals = vec![param_set];
        let resolved = rf.try_map(&mut |e| resolver.lower_expr(&e, &mut param_locals))?;
        formas.push(resolved);
    }

    let mut functions = Vec::with_capacity(
        raw_fns.len() + formas.len() + if formas.is_empty() { 0 } else { 3 },
    );
    if !formas.is_empty() {
        let user_fn_names: HashSet<&str> = raw_fns.iter().map(|rf| rf.name.as_str()).collect();
        for h in crate::forma::forma_runtime_helpers() {
            if !user_fn_names.contains(h.name.as_str()) {
                functions.push(h);
            }
        }
    }
    for rf in &raw_fns {
        if let RawType::Named(sname, _) = &rf.ret_type {
            resolver.fn_ret_struct.insert(rf.name.clone(), sname.clone());
        }
    }
    let mut tests = Vec::new();
    for rf in raw_fns {
        if let Some(label) = rf.test_label.clone() {
            tests.push((rf.name.clone(), label));
        }
        functions.push(resolver.lower_fn(rf)?);
    }
    for forma in &formas {
        functions.push(crate::forma::lower_forma_def(forma));
    }

    // Auto-generate standard .ifw GPU runtime exports when std/gfx is imported and step_ui is exported
    if resolver.consts.contains_key("GPU_CMD_BYTE_OFFSET")
        && functions.iter().any(|f| f.exported && f.name == "step_ui")
    {
        let existing: HashSet<String> = functions.iter().map(|f| f.name.clone()).collect();
        let mem_eff = vec![EffectCap {
            name: "Mem".to_string(),
            scope: None,
        }];
        if !existing.contains("ifw_unicode_atlas") {
            functions.push(FnDef {
                name: "ifw_unicode_atlas".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::IntLit(1),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_cmd_byte_offset") {
            functions.push(FnDef {
                name: "get_cmd_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(262144)),
                }],
                body: Expr::IntLit(262144),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_clips_byte_offset") {
            functions.push(FnDef {
                name: "get_clips_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(256)),
                }],
                body: Expr::IntLit(256),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_font_metrics_byte_offset") {
            functions.push(FnDef {
                name: "get_font_metrics_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(1024)),
                }],
                body: Expr::IntLit(1024),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_input_byte_offset") {
            functions.push(FnDef {
                name: "get_input_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(768)),
                }],
                body: Expr::IntLit(768),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_mesh3d_byte_offset") {
            if let Some(m3d) = resolver.layouts.get("Mesh3dMem") {
                let byte_off = m3d.base * 4;
                functions.push(FnDef {
                    name: "get_mesh3d_byte_offset".to_string(),
                    exported: true,
                    params: vec![],
                    effects: vec![],
                    ret_type: Type::I64,
                    requires: vec![],
                    ensures: vec![Expr::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(Expr::Var("result".to_string())),
                        rhs: Box::new(Expr::IntLit(byte_off)),
                    }],
                    body: Expr::IntLit(byte_off),
                    source_file: None,
                    span: None,
                });
            }
        }
        if !existing.contains("get_prim_count") {
            functions.push(FnDef {
                name: "get_prim_count".to_string(),
                exported: true,
                params: vec![],
                effects: mem_eff.clone(),
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::MemLoad {
                    index: Box::new(Expr::IntLit(12)),
                    inv: None,
                    fixed_ty: false,
                },
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_cursor_kind") {
            functions.push(FnDef {
                name: "get_cursor_kind".to_string(),
                exported: true,
                params: vec![],
                effects: mem_eff,
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::MemLoad {
                    index: Box::new(Expr::IntLit(11)),
                    inv: None,
                    fixed_ty: false,
                },
                source_file: None,
                span: None,
            });
        }
    }

    resolve_fixed_ops_in_fns(&mut functions);

    Ok(Module {
        consts: resolver.consts_order,
        enums: resolver.enums_order,
        layouts: resolver.layouts_order,
        formas,
        functions,
        tests,
    })
}

fn infer_and_rewrite_fixed_expr(
    expr: &mut Expr,
    env: &mut BTreeMap<String, Type>,
    fn_rets: &BTreeMap<String, Type>,
) -> Type {
    match expr {
        Expr::IntLit(_) => Type::I64,
        Expr::FixedLit(_) => Type::Fixed,
        Expr::BoolLit(_) => Type::Bool,
        Expr::StrLit(_) => Type::Str,
        Expr::UnitLit => Type::Unit,
        Expr::Var(n) => env.get(n).cloned().unwrap_or(Type::I64),
        Expr::MemLoad { index, fixed_ty, .. } => {
            infer_and_rewrite_fixed_expr(index, env, fn_rets);
            if *fixed_ty {
                Type::Fixed
            } else {
                Type::I64
            }
        }
        Expr::MemStore { index, value, .. } => {
            infer_and_rewrite_fixed_expr(index, env, fn_rets);
            infer_and_rewrite_fixed_expr(value, env, fn_rets);
            Type::Unit
        }
        Expr::Unary { op, expr: inner } => {
            let t = infer_and_rewrite_fixed_expr(inner, env, fn_rets);
            match op {
                UnaryOp::Neg => t,
                UnaryOp::Not => Type::Bool,
                UnaryOp::IntToFixed | UnaryOp::RawToFixed => Type::Fixed,
                UnaryOp::FixedToInt | UnaryOp::FixedToRaw => Type::I64,
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = infer_and_rewrite_fixed_expr(lhs, env, fn_rets);
            let rt = infer_and_rewrite_fixed_expr(rhs, env, fn_rets);
            if lt == Type::Fixed && rt == Type::Fixed {
                if *op == BinOp::Mul {
                    *op = BinOp::FixedMul;
                } else if *op == BinOp::Div {
                    *op = BinOp::FixedDiv;
                }
            }
            match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::And | BinOp::Or => Type::Bool,
                BinOp::FixedMul | BinOp::FixedDiv => Type::Fixed,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                    if lt == Type::Fixed || rt == Type::Fixed {
                        Type::Fixed
                    } else {
                        Type::I64
                    }
                }
                _ => Type::I64,
            }
        }
        Expr::If { cond, then_branch, else_branch } => {
            infer_and_rewrite_fixed_expr(cond, env, fn_rets);
            let tt = infer_and_rewrite_fixed_expr(then_branch, env, fn_rets);
            let _ = infer_and_rewrite_fixed_expr(else_branch, env, fn_rets);
            tt
        }
        Expr::Call { target, args } => {
            let mut arg_types = Vec::with_capacity(args.len());
            for a in args.iter_mut() {
                arg_types.push(infer_and_rewrite_fixed_expr(a, env, fn_rets));
            }
            if matches!(target, TargetRef::Named(n) if n == "ui_int")
                && args.len() == 7
                && arg_types.get(2) == Some(&Type::Fixed)
            {
                *target = TargetRef::Named("ui_fixed".to_string());
                args.insert(3, Expr::IntLit(2));
            } else if matches!(target, TargetRef::Named(n) if n == "ui_int_width")
                && args.len() == 3
                && arg_types.first() == Some(&Type::Fixed)
            {
                *target = TargetRef::Named("ui_fixed_width".to_string());
                args.insert(1, Expr::IntLit(2));
            }
            match target {
                TargetRef::Named(n) => fn_rets.get(n).cloned().unwrap_or(Type::I64),
                _ => Type::I64,
            }
        }
        Expr::Perform { args, .. } => {
            for a in args {
                infer_and_rewrite_fixed_expr(a, env, fn_rets);
            }
            Type::Unit
        }
        Expr::SecretWrap(inner) => Type::Secret(Box::new(infer_and_rewrite_fixed_expr(inner, env, fn_rets))),
        Expr::Reveal { expr: inner, .. } => match infer_and_rewrite_fixed_expr(inner, env, fn_rets) {
            Type::Secret(t) => *t,
            other => other,
        },
        Expr::TupleLit(elems) => Type::Tuple(
            elems
                .iter_mut()
                .map(|e| infer_and_rewrite_fixed_expr(e, env, fn_rets))
                .collect(),
        ),
        Expr::TupleField { expr: inner, index } => match infer_and_rewrite_fixed_expr(inner, env, fn_rets) {
            Type::Tuple(ts) => ts.get(*index).cloned().unwrap_or(Type::I64),
            _ => Type::I64,
        },
        Expr::Block { stmts, tail } => {
            let mut inner_env = env.clone();
            infer_and_rewrite_fixed_stmts(stmts, &mut inner_env, fn_rets);
            infer_and_rewrite_fixed_expr(tail, &mut inner_env, fn_rets)
        }
    }
}

fn infer_and_rewrite_fixed_stmts(
    stmts: &mut [Stmt],
    env: &mut BTreeMap<String, Type>,
    fn_rets: &BTreeMap<String, Type>,
) {
    for s in stmts {
        match s {
            Stmt::Let { name, ty, value, .. } => {
                let vt = infer_and_rewrite_fixed_expr(value, env, fn_rets);
                env.insert(name.clone(), ty.clone().unwrap_or(vt));
            }
            Stmt::LetTuple { names, value } => {
                let vt = infer_and_rewrite_fixed_expr(value, env, fn_rets);
                if let Type::Tuple(ts) = vt {
                    for (i, (n, _)) in names.iter().enumerate() {
                        env.insert(n.clone(), ts.get(i).cloned().unwrap_or(Type::I64));
                    }
                } else {
                    for (n, _) in names {
                        env.insert(n.clone(), Type::I64);
                    }
                }
            }
            Stmt::Assign { value, .. } | Stmt::Expr(value) => {
                infer_and_rewrite_fixed_expr(value, env, fn_rets);
            }
            Stmt::While { cond, body, .. } => {
                infer_and_rewrite_fixed_expr(cond, env, fn_rets);
                let mut loop_env = env.clone();
                infer_and_rewrite_fixed_stmts(body, &mut loop_env, fn_rets);
            }
        }
    }
}

fn resolve_fixed_ops_in_fns(functions: &mut [FnDef]) {
    let mut fn_rets = BTreeMap::new();
    for f in functions.iter() {
        fn_rets.insert(f.name.clone(), f.ret_type.clone());
    }
    for f in functions.iter_mut() {
        let mut env = BTreeMap::new();
        for p in &f.params {
            env.insert(p.name.clone(), p.ty.clone());
        }
        for r in &mut f.requires {
            infer_and_rewrite_fixed_expr(r, &mut env, &fn_rets);
        }
        let mut ens_env = env.clone();
        ens_env.insert("result".to_string(), f.ret_type.clone());
        for e in &mut f.ensures {
            infer_and_rewrite_fixed_expr(e, &mut ens_env, &fn_rets);
        }
        infer_and_rewrite_fixed_expr(&mut f.body, &mut env, &fn_rets);
    }
}

fn subst_raw_expr(expr: RawExpr, env: &BTreeMap<String, RawExpr>) -> RawExpr {
    if env.is_empty() {
        return expr;
    }
    match expr {
        RawExpr::Var(ref name) => {
            if let Some(rep) = env.get(name) {
                rep.clone()
            } else {
                expr
            }
        }
        RawExpr::Unary { op, expr: inner } => RawExpr::Unary {
            op,
            expr: Box::new(subst_raw_expr(*inner, env)),
        },
        RawExpr::Binary { op, lhs, rhs } => RawExpr::Binary {
            op,
            lhs: Box::new(subst_raw_expr(*lhs, env)),
            rhs: Box::new(subst_raw_expr(*rhs, env)),
        },
        RawExpr::If {
            cond,
            then_branch,
            else_branch,
        } => RawExpr::If {
            cond: Box::new(subst_raw_expr(*cond, env)),
            then_branch: Box::new(subst_raw_expr(*then_branch, env)),
            else_branch: Box::new(subst_raw_expr(*else_branch, env)),
        },
        RawExpr::Call { target, args } => RawExpr::Call {
            target,
            args: args.into_iter().map(|a| subst_raw_expr(a, env)).collect(),
        },
        RawExpr::LayoutLoad {
            layout,
            field,
            index,
            subfield,
            span,
        } => RawExpr::LayoutLoad {
            layout,
            field,
            index: index.map(|i| Box::new(subst_raw_expr(*i, env))),
            subfield,
            span,
        },
        RawExpr::MemLoad { index } => RawExpr::MemLoad {
            index: Box::new(subst_raw_expr(*index, env)),
        },
        RawExpr::TupleLit(elems) => {
            RawExpr::TupleLit(elems.into_iter().map(|e| subst_raw_expr(e, env)).collect())
        }
        RawExpr::TupleField { expr: inner, index } => RawExpr::TupleField {
            expr: Box::new(subst_raw_expr(*inner, env)),
            index,
        },
        RawExpr::StructLit { name, fields, span } => RawExpr::StructLit {
            name,
            fields: fields
                .into_iter()
                .map(|(k, v, sp)| (k, subst_raw_expr(v, env), sp))
                .collect(),
            span,
        },
        RawExpr::FieldAccess { expr: inner, field, span } => RawExpr::FieldAccess {
            expr: Box::new(subst_raw_expr(*inner, env)),
            field,
            span,
        },
        other => other,
    }
}

fn subst_forma_node(
    node: crate::forma::FormaNode<RawExpr>,
    env: &BTreeMap<String, RawExpr>,
) -> crate::forma::FormaNode<RawExpr> {
    node.try_map(&mut |e| Ok::<RawExpr, ()>(subst_raw_expr(e, env)))
        .unwrap()
}

fn attach_forma_visible(
    mut node: crate::forma::FormaNode<RawExpr>,
    cond: RawExpr,
) -> crate::forma::FormaNode<RawExpr> {
    node.props.visible = match node.props.visible {
        Some(prev) => Some(RawExpr::Binary {
            op: BinOp::And,
            lhs: Box::new(cond),
            rhs: Box::new(prev),
        }),
        None => Some(cond),
    };
    node
}

fn expand_forma_node(
    mut node: crate::forma::FormaNode<RawExpr>,
    defs: &BTreeMap<String, crate::forma::FormaDef<RawExpr>>,
    resolver: &ModuleResolver,
    visiting: &mut Vec<String>,
) -> Result<Vec<crate::forma::FormaNode<RawExpr>>, InfimumDiagnostic> {
    // 1. Conditional UI: `if cond { then... } [else { else... }]`
    if let Some((cond, else_nodes)) = node.forma_if.take() {
        let mut out = Vec::new();
        for ch in node.children {
            for exp in expand_forma_node(ch, defs, resolver, visiting)? {
                out.push(attach_forma_visible(exp, cond.clone()));
            }
        }
        if !else_nodes.is_empty() {
            let neg_cond = RawExpr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(cond),
            };
            for ch in else_nodes {
                for exp in expand_forma_node(ch, defs, resolver, visiting)? {
                    out.push(attach_forma_visible(exp, neg_cond.clone()));
                }
            }
        }
        return Ok(out);
    }

    // 2. Static unrolled loop: `for i in start..end { body... }`
    if let Some((var, start_expr, end_expr, inclusive)) = node.forma_for.take() {
        let sv = resolver.eval_const_i64(&start_expr, node.span, "forma 'for' start")?;
        let ev = resolver.eval_const_i64(&end_expr, node.span, "forma 'for' end")?;
        let hi = if inclusive { ev } else { ev.saturating_sub(1) };
        let count = if hi >= sv { hi - sv + 1 } else { 0 };
        if count > 256 {
            return Err(InfimumDiagnostic::new(
                "E094_FORMA_FOR_TOO_LARGE",
                "forma-expand",
                format!("Forma 'for' loop unrolls to {count} iterations (max 256)"),
                node.span,
                "Use a smaller static range (<= 256) or a custom Slot(render: ...) for large dynamic lists",
            ));
        }
        let mut out = Vec::new();
        for k in sv..=hi {
            let mut env = BTreeMap::new();
            env.insert(var.clone(), RawExpr::IntLit(k));
            for ch in &node.children {
                let inst = subst_forma_node(ch.clone(), &env);
                out.extend(expand_forma_node(inst, defs, resolver, visiting)?);
            }
        }
        return Ok(out);
    }

    // 3. Nested custom `forma` component call: `ScoreCard("Вы", score)`
    if let Some((comp_name, args)) = node.component_call.take() {
        let Some(target_def) = defs.get(&comp_name) else {
            return Err(InfimumDiagnostic::new(
                "E091_FORMA_SYNTAX",
                "forma-expand",
                format!("Unknown Forma node or component '{comp_name}'"),
                node.span,
                "Use a built-in node (Box, Col, Row, Stack, Scroll, Overlay, Button, Dropdown, Spacer, Divider, Rect, Slot, Text) or declare 'forma Name(...)'",
            ));
        };
        if visiting.iter().any(|v| v == &comp_name) {
            return Err(InfimumDiagnostic::new(
                "E095_FORMA_COMPONENT_CYCLE",
                "forma-expand",
                format!("Recursive Forma component cycle detected involving '{comp_name}'"),
                node.span,
                "Forma component trees must be statically acyclic",
            ));
        }
        if args.len() != target_def.params.len() {
            return Err(InfimumDiagnostic::new(
                "E096_FORMA_COMPONENT_ARITY",
                "forma-expand",
                format!(
                    "Forma component '{comp_name}' expects {} argument(s), got {}",
                    target_def.params.len(),
                    args.len()
                ),
                node.span,
                "Match the arguments passed to the nested Forma component",
            ));
        }
        let mut env = BTreeMap::new();
        for (param, arg) in target_def.params.iter().zip(args) {
            env.insert(param.name.clone(), arg);
        }
        visiting.push(comp_name);
        let instantiated = subst_forma_node(target_def.root.clone(), &env);
        let expanded = expand_forma_node(instantiated, defs, resolver, visiting)?;
        visiting.pop();
        return Ok(expanded);
    }

    // 4. Regular built-in Forma node: recursively expand children
    let mut new_children = Vec::with_capacity(node.children.len());
    for ch in node.children {
        new_children.extend(expand_forma_node(ch, defs, resolver, visiting)?);
    }
    node.children = new_children;
    Ok(vec![node])
}

fn rewrite_assert_stmt(stmt: RawStmt, default_line: i64) -> RawStmt {
    match stmt {
        RawStmt::Expr(RawExpr::Call {
            target: TargetRef::Named(name),
            args,
        }) if name == "assert" && args.len() == 1 => {
            let cond = args.into_iter().next().unwrap();
            RawStmt::Expr(RawExpr::If {
                cond: Box::new(RawExpr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(cond),
                }),
                then_branch: Box::new(RawExpr::If {
                    cond: Box::new(RawExpr::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(RawExpr::Var("__ifm_test_fail".to_string())),
                        rhs: Box::new(RawExpr::IntLit(0)),
                    }),
                    then_branch: Box::new(RawExpr::Block {
                        stmts: vec![RawStmt::Assign {
                            name: "__ifm_test_fail".to_string(),
                            op: None,
                            value: RawExpr::IntLit(default_line),
                        }],
                        tail: Box::new(RawExpr::UnitLit),
                    }),
                    else_branch: Box::new(RawExpr::UnitLit),
                }),
                else_branch: Box::new(RawExpr::UnitLit),
            })
        }
        RawStmt::While {
            cond,
            max_iters,
            body,
            span,
        } => RawStmt::While {
            cond,
            max_iters,
            body: body
                .into_iter()
                .map(|s| rewrite_assert_stmt(s, span.line as i64))
                .collect(),
            span,
        },
        RawStmt::For {
            var,
            start,
            end,
            inclusive,
            max_iters,
            body,
            span,
        } => RawStmt::For {
            var,
            start,
            end,
            inclusive,
            max_iters,
            body: body
                .into_iter()
                .map(|s| rewrite_assert_stmt(s, span.line as i64))
                .collect(),
            span,
        },
        other => other,
    }
}

#[cfg(test)]
mod struct_layout_tests {
    use super::*;

    #[test]
    fn parses_and_lowers_structs_and_indexed_arrays() {
        let src = r#"
struct Sheep {
  x: word,
  z: word,
  vx: word,
  alive: word,
}

layout GameMem at 0 {
  tick: word,
  hero: Sheep,
  flock: Sheep[8] at 20,
  board: words[16] at 100,
}

fn step(i: i64) -[Mem]-> i64 req i >= 0 && i < 8 {
  GameMem.tick += 1;
  GameMem.hero.x = 42;
  GameMem.hero.vx += 3;
  GameMem.flock[i].x += GameMem.flock[i].vx;
  GameMem.board[i] = Sheep::SIZE + GameMem::flock::stride + GameMem::flock::count;
  GameMem.flock[i].x + GameMem.board[i] + GameMem.hero.x
}
"#;
        let module = parse_module(src).expect("struct + indexed layout should parse cleanly");
        assert_eq!(module.layouts.len(), 1);
        let l = &module.layouts[0];
        let flock = l.fields.iter().find(|f| f.name == "flock").unwrap();
        assert_eq!(flock.offset, 20);
        assert_eq!(flock.len, 32); // 8 * 4 words
        assert!(flock.is_array);
    }

    #[test]
    fn unknown_struct_field_lists_available_fields_in_hint() {
        let src = r#"
struct Vec2 {
  x: word,
  y: word,
}
layout M at 0 {
  p: Vec2,
}
fn bad() -[Mem]-> i64 {
  M.p.z
}
"#;
        let err = parse_module(src).expect_err("should fail on unknown subfield");
        assert!(err.repair_hint.contains("x, y"), "hint was: {}", err.repair_hint);
    }

    #[test]
    fn auto_layout_places_safely_above_gpu_prims_and_packs_multiple_layouts() {
        let src = r#"
layout GameState {
  score: word,
  board: words[10],
}
layout UiState {
  hover: word,
}
fn f() -[Mem]-> i64 {
  GameState.score = 1;
  UiState.hover = 2;
  GameState.score + UiState.hover
}
"#;
        let module = parse_module(src).expect("auto layouts should parse");
        assert_eq!(module.layouts[0].base, 705_536);
        assert_eq!(module.layouts[0].fields[0].offset, 705_536);
        assert_eq!(module.layouts[1].base, 705_547);
        assert_eq!(module.layouts[1].fields[0].offset, 705_547);
    }

    #[test]
    fn gpu_layout_overwriting_prim_count_word_12_is_rejected() {
        let src = r#"
const GPU_CMD_BYTE_OFFSET: i64 = 262144;
layout Bad at 0 {
  cells: words[20],
}
fn f() -[Mem]-> i64 { Bad.cells[0] }
"#;
        let err = parse_module(src).expect_err("overwriting word 12 must fail");
        assert_eq!(err.code, "E066_RESERVED_RUNTIME_ADDR");
    }

    #[test]
    fn layout_exceeding_64mib_linear_memory_is_rejected() {
        let src = r#"
layout TooBig at 16770000 {
  buf: words[10000],
}
fn f() -[Mem]-> i64 { TooBig.buf[0] }
"#;
        let err = parse_module(src).expect_err("layout past 16,777,216 words must fail");
        assert_eq!(err.code, "E067_LAYOUT_OUT_OF_MEMORY");
    }

    #[test]
    fn auto_exports_are_generated_for_gpu_capsules() {
        let src = r#"
const GPU_CMD_BYTE_OFFSET: i64 = 262144;
const GPU_CLIPS_BYTE_OFFSET: i64 = 256;
export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, md: i64, mc: i64, wd: i64, kc: i64, ks: i64) -[Mem, Gpu]-> i64 {
  do Gpu.submit(0, 0);
  0
}
"#;
        let module = parse_module(src).expect("minimal GPU capsule should auto-export");
        for name in [
            "ifw_unicode_atlas",
            "get_cmd_byte_offset",
            "get_clips_byte_offset",
            "get_font_metrics_byte_offset",
            "get_input_byte_offset",
            "get_prim_count",
            "get_cursor_kind",
        ] {
            assert!(
                module.functions.iter().any(|f| f.exported && f.name == name),
                "missing auto-export {name}"
            );
        }
    }

    #[test]
    fn std_mem_scratch_arena_pool_and_16mib_auto_sizing_work() {
        let src = r#"
use "std/mem.ifm";

layout BigApp {
  arena_cur: word,
  arena_buf: words[2500000],
  pool_flags: words[16],
}

export fn run_mem_demo() -[Mem]-> i64 {
  scratch_reset();
  let s1 = scratch_alloc(32);
  let s2 = scratch_alloc(16);
  arena_reset(BigApp::arena_cur);
  let a1 = arena_alloc(BigApp::arena_buf, 2500000, BigApp::arena_cur, 100);
  pool_init(BigApp::pool_flags, 16);
  let p0 = pool_alloc(BigApp::pool_flags, 16);
  let p1 = pool_alloc(BigApp::pool_flags, 16);
  pool_free(BigApp::pool_flags, p0, 16);
  let p2 = pool_alloc(BigApp::pool_flags, 16);
  if s1 == 4096 && s2 == 4128 && a1 == BigApp::arena_buf && p0 == 0 && p1 == 1 && p2 == 0 {
    1
  } else {
    0
  }
}
"#;
        let files = BTreeMap::from([("main.ifm".to_string(), src.to_string())]);
        let (wasm, meta) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("std/mem.ifm + 16MiB layout should compile");
        assert!(meta.ok);
        // Verify Wasm Memory section requests 256 pages (16 MiB) because BigApp ends > 2,097,152 words.
        // Memory section id = 5: [0x05, size, 0x01, 0x00, 0x80, 0x02] (256 in ULEB128 = 0x80 0x02)
        assert!(
            wasm.windows(6).any(|w| w == [0x05, 0x04, 0x01, 0x00, 0x80, 0x02]),
            "expected Wasm Memory section with 256 pages (16 MiB)"
        );
    }

    #[test]
    fn for_break_continue_desugar_and_typecheck_cleanly() {
        let src = r#"
fn sum_filtered(n: i64) -> i64 {
  let mut sum = 0;
  for i in 0..10 {
    if i == 3 { continue; }
    if i == 7 { break; }
    sum += i;
  }
  for j in 1..=3 {
    sum += j;
  }
  for k in 0..n max 16 {
    if k == 2 { break; }
    sum += 1;
  }
  sum
}
"#;
        let module = parse_module(src).expect("for/break/continue should parse and lower");
        assert_eq!(module.functions.len(), 1);
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm_bytes, meta) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("typecheck + light verify + wasm compile should pass for/break/continue");
        assert!(!wasm_bytes.is_empty());
        assert_eq!(meta.compiled_nodes, 1);
    }

    #[test]
    fn break_outside_loop_is_rejected() {
        let src = r#"
fn bad() -> i64 {
  break;
  0
}
"#;
        let err = parse_module(src).expect_err("break outside loop must fail");
        assert_eq!(err.code, "E067_BREAK_OUTSIDE_LOOP");
    }

    #[test]
    fn field_invariants_propagate_on_reads_and_reject_invalid_writes() {
        let good_src = r#"
struct Cell {
  color: word inv 1..=7,
}
layout Board {
  turn: word inv 1..=2,
  cells: Cell[9],
}
fn div_by_turn(x: i64) -[Mem]-> i64 {
  Board.turn = 2;
  Board.cells[0].color = 5;
  x / Board.turn + 100 / Board.cells[0].color
}
"#;
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), good_src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("field invariants should prove non-zero divisors without clamp");
        assert!(!wasm.is_empty());

        let bad_src = r#"
layout Board {
  turn: word inv 1..=2,
}
fn bad_write() -[Mem]-> i64 {
  Board.turn = 99;
  Board.turn
}
"#;
        let mut bad_files = BTreeMap::new();
        bad_files.insert("main.ifm".to_string(), bad_src.to_string());
        let err = crate::wasm_api::compile_project_in_memory("main.ifm", &bad_files)
            .expect_err("storing 99 into inv 1..=2 must fail");
        assert_eq!(err.code, "E054_FIELD_INVARIANT_VIOLATED");
    }

    #[test]
    fn builtin_test_blocks_run_and_report_pass_and_fail() {
        let src = r#"
layout Board {
  cells: words[9] inv 0..=2,
}
fn sum_range(n: i64) -> i64 {
  let mut s = 0;
  for i in 1..=n max 32 {
    s += i;
  }
  s
}
test "sum_range and board layout" {
  assert(sum_range(5) == 15);
  Board.cells[4] = 2;
  assert(Board.cells[4] == 2);
}
test "intentional failure" {
  assert(sum_range(3) == 999);
}
"#;
        let module = parse_module(src).expect("test blocks should parse");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 2);
        assert!(results[0].passed, "first test should pass: {:?}", results[0]);
        assert!(!results[1].passed, "second test should fail: {:?}", results[1]);
        assert!(results[1].fail_line > 0);
    }

    #[test]
    fn tuples_return_destructure_field_access_eval_and_wasm_compile() {
        let src = r#"
fn divmod_18(idx: i64) -> (i64, i64)
  req idx >= 0
{
  (idx / 18, idx % 18)
}

export fn sum_coords(idx: i64) -> i64
  req idx >= 0
{
  let (r, c) = divmod_18(idx);
  let r2 = divmod_18(idx).0;
  let c2 = (10, 20).1;
  r + c + r2 + c2
}

test "divmod_18 and destructuring" {
  let (r, c) = divmod_18(40);
  assert(r == 2);
  assert(c == 4);
  assert(sum_coords(40) == 28);
}
"#;
        let module = parse_module(src).expect("tuples should parse and lower");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "tuple test failed: {:?}", results[0]);

        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("tuple multi-value Wasm compilation should succeed");
        assert!(!wasm.is_empty());
    }

    #[test]
    fn forma_nested_components_if_else_and_for_expand_and_compile() {
        let src = r#"
forma PillBtn(label: str, act_id: i64) -[Mem]-> i64 {
  Button(w: 96, h: 36, bg: 0xFF334455, hover_bg: 0xFF445566, radius: 8, on_click: act_id, align: center) {
    Text(label, w: 80, h: 20, size: 14, style: 1, color: 0xFFFFFFFF, align: center)
  }
}

forma MainPanel(vw: i64, vh: i64, show_extra: bool)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: 0xFF111318, align: center) {
    Col(w: 420, h: 240, pad: 16, gap: 10, bg: 0xFF1D2230, radius: 12, align: center) {
      Row(w: 360, h: 40, gap: 8, align: center) {
        for i in 0..3 {
          PillBtn("Btn", i + 1)
        }
      }
      if show_extra {
        PillBtn("Extra", 10)
      } else {
        Text("Hidden", w: 120, h: 24, size: 14, style: 0, color: 0xFF99AABB, align: center)
      }
    }
  }
}
"#;
        let module = parse_module(src).expect("Forma components + if/else + for should expand");
        assert_eq!(module.formas.len(), 2);
        let main_forma = module.formas.iter().find(|f| f.name == "MainPanel").unwrap();
        let col = &main_forma.root.children[0];
        let row = &col.children[0];
        // `for i in 0..3 { PillBtn(...) }` expanded into 3 Button nodes!
        assert_eq!(row.children.len(), 3);
        assert_eq!(row.children[0].kind, crate::forma::FormaNodeKind::Button);
        // `if show_extra { PillBtn(...) } else { Text(...) }` expanded into 2 children with opposite visible guards!
        assert_eq!(col.children.len(), 3);
        assert!(col.children[1].props.visible.is_some());
        assert!(col.children[2].props.visible.is_some());
    }

    #[test]
    fn mod_pub_namespaces_isolate_private_helpers_and_block_external_private_access() {
        let ok_src = r#"
mod alpha {
  const SECRET: i64 = 10;
  pub const SCALE: i64 = 2;
  fn helper(x: i64) -> i64 { x + SECRET }
  pub fn compute(x: i64) -> i64 { helper(x) * SCALE }
}

mod beta {
  const SECRET: i64 = 100;
  fn helper(x: i64) -> i64 { x + SECRET }
  pub fn compute(x: i64) -> i64 { helper(x) }
}

export fn run(x: i64) -> i64 {
  alpha::compute(x) + beta::compute(x) + alpha::SCALE
}

test "two modules with identical private helpers do not collide" {
  assert(run(5) == 137);
}
"#;
        let module = parse_module(ok_src).expect("mod/pub namespaces should parse and lower");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "mod test failed: {:?}", results[0]);

        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), ok_src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("mod/pub Wasm compile should pass");
        assert!(!wasm.is_empty());

        // Calling private function externally must fail with E068_PRIVATE_SYMBOL
        let bad_fn = r#"
mod m {
  fn hidden_fn() -> i64 { 42 }
}
fn main() -> i64 { m::hidden_fn() }
"#;
        let err = parse_module(bad_fn).expect_err("calling private mod fn must fail");
        assert_eq!(err.code, "E068_PRIVATE_SYMBOL");

        // Reading private constant externally must fail with E068_PRIVATE_SYMBOL
        let bad_const = r#"
mod m {
  const SECRET: i64 = 42;
}
fn main() -> i64 { m::SECRET }
"#;
        let err2 = parse_module(bad_const).expect_err("reading private mod const must fail");
        assert_eq!(err2.code, "E068_PRIVATE_SYMBOL");

        // File-based `mod ai from "ai.ifm";` in-memory (Online Studio)
        let mut proj = BTreeMap::new();
        proj.insert(
            "ai.ifm".to_string(),
            "const BONUS: i64 = 7;\nfn hidden(x: i64) -> i64 { x + BONUS }\npub fn pick(x: i64) -> i64 { hidden(x) * 2 }\n".to_string(),
        );
        proj.insert(
            "main.ifm".to_string(),
            "mod ai from \"ai.ifm\";\nexport fn play(x: i64) -> i64 { ai::pick(x) }\n".to_string(),
        );
        let (wasm2, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &proj)
            .expect("file-based mod ... from should compile in memory");
        assert!(!wasm2.is_empty());
    }

    #[test]
    fn ui_fmt_desugars_zero_heap_into_ui_text_and_ui_int() {
        let src = r#"
use "std/prelude.ifm";

layout Game {
  score: word,
  speed: fixed,
}

export fn draw_hud(turn: i64, vel: fixed) -[Mem]-> i64 {
  Game.score = 42;
  Game.speed = vel;
  let w = ui_fmt_width("v={Game.speed} ({vel:.1})", 16, 1);
  let _ = ui_fmt_centered(320, 40, "Ход {turn + 1} — счёт {Game.score} — v={Game.speed} ({vel:.1})", 16, 1, pack_rgba(255, 255, 255, 255), 0);
  w
}
"#;
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("ui_fmt should desugar and compile cleanly");
        assert!(!wasm.is_empty());
    }

    #[test]
    fn fixed_q16_16_type_arithmetic_layout_and_safety_work() {
        let ok_src = r#"
use "std/math.ifm";

const GRAVITY: fixed = 9.81;
const DT: fixed = 0.5;

struct Body {
  y: fixed inv 0.0..=1000.0,
  vy: fixed,
}

layout Physics {
  bodies: Body[4],
  wind: fixed,
}

export fn step_fall(v0: fixed, dt: fixed) -> fixed
  req dt > 0.0
{
  let v1 = v0 + GRAVITY * dt;
  v1 / 2.0
}

export fn init_physics() -[Mem]-> i64 {
  Physics.wind = -0.25;
  for i in 0..4 {
    if i >= 0 {
      Physics.bodies[i].y = fixed(10 + i);
      Physics.bodies[i].vy = 1.5;
    }
  }
  fixed_to_int(Physics.bodies[2].y + Physics.bodies[2].vy * 2)
}

test "fixed Q16.16 arithmetic, layout fields, and trig" {
  let _ = init_physics();
  assert(fixed_to_int(Physics.bodies[0].y) == 10);
  assert(fixed_to_int(Physics.bodies[2].y + Physics.bodies[2].vy * 2) == 15);
  assert(fixed_to_int(1.5 * 4.0) == 6);
  assert(fixed_to_int(7.5 / 2.5) == 3);
  assert(fixed_to_int(fixed_abs(-3.5)) == 3);
  assert(fixed_sin(1024) == 1.0);
  assert(fixed_cos(0) == 1.0);
  assert(step_fall(0.0, DT) > 2.4 && step_fall(0.0, DT) < 2.5);
}
"#;
        let module = parse_module(ok_src).expect("fixed Q16.16 module should parse and lower");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "fixed test failed: {:?}", results[0]);

        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), ok_src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("fixed Q16.16 Wasm compile should pass");
        assert!(!wasm.is_empty());

        // Type mismatch: cannot add raw i64 and fixed without fixed(...)
        let bad_mix = "fn bad(a: i64) -> fixed { a + 1.5 }\n";
        let mut f_bad = BTreeMap::new();
        f_bad.insert("main.ifm".to_string(), bad_mix.to_string());
        let err = crate::wasm_api::compile_project_in_memory("main.ifm", &f_bad)
            .expect_err("mixing i64 + fixed must fail typecheck");
        assert_eq!(err.code, "E060_TYPE_MISMATCH");

        // Division by 0.0 must be caught statically
        let bad_div = "fn bad_div(a: fixed) -> fixed { a / 0.0 }\n";
        let mut f_div = BTreeMap::new();
        f_div.insert("main.ifm".to_string(), bad_div.to_string());
        let err_div = crate::wasm_api::compile_project_in_memory("main.ifm", &f_div)
            .expect_err("dividing fixed by 0.0 must fail");
        assert_eq!(err_div.code, "E053_POSSIBLE_DIV_BY_ZERO");
    }

    #[test]
    fn value_structs_construct_pass_return_mutate_layout_and_wasm_compile() {
        let src = r#"
use "std/prelude.ifm";

struct Vec3 {
  x: fixed,
  y: fixed,
  z: fixed,
}

layout World {
  hero: Vec3,
  pts: Vec3[4],
}

fn vec_add(a: Vec3, b: Vec3) -> Vec3 {
  Vec3 { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }
}

fn vec_dot(a: Vec3, b: Vec3) -> fixed {
  a.x * b.x + a.y * b.y + a.z * b.z
}

fn safe_ratio(v: Vec3) -> fixed
  req v.y != 0.0
{
  v.x / v.y
}

export fn init_app() -[Mem]-> i64 {
  let start = Vec3 { z: 3.0, x: 1.0, y: 2.0 };
  World.hero = start;
  World.pts[1] = vec_add(start, Vec3 { x: 0.5, y: 1.5, z: 2.5 });
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, md: i64, mc: i64, wd: i64, kc: i64, ks: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  gfx_begin_frame(w, h);
  let mut p = World.hero;
  p.x += 0.5;
  World.hero = p;
  ui_fmt(12, 12, "Pos: ({p.x:.1}, {p.y:.1}, {p.z:.1})", 14, 1, pack_rgba(255, 255, 255, 255), 0);
  gfx_end_frame()
}

test "value structs math, mutation, and layout roundtrip" {
  let _ = init_app();
  let mut v: Vec3 = World.hero;
  assert(v.x == 1.0 && v.y == 2.0 && v.z == 3.0);
  v.x += 2.5;
  v.z = 10.0;
  assert(v.x == 3.5 && v.z == 10.0);
  let sum = vec_add(v, World.pts[1]);
  assert(sum.x == 5.0 && sum.y == 5.5 && sum.z == 15.5);
  assert(vec_add(Vec3 { x: 1.0, y: 2.0, z: 3.0 }, Vec3 { x: 4.0, y: 5.0, z: 6.0 }).y == 7.0);
  assert(vec_dot(Vec3 { x: 1.0, y: 2.0, z: 3.0 }, Vec3 { x: 2.0, y: 3.0, z: 4.0 }) == 20.0);
  assert(safe_ratio(Vec3 { x: 9.0, y: 3.0, z: 0.0 }) == 3.0);
}
"#;
        let mut files = BTreeMap::new();
        files.insert("main.ifm".to_string(), src.to_string());
        let (wasm, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files)
            .expect("value-struct Wasm compile should pass");
        assert!(!wasm.is_empty());

        let module = parse_module(src).expect("value-struct module should parse");
        let results = crate::eval::run_module_tests(&module);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "value-struct test failed: {:?}", results[0]);

        // Missing field error
        let bad_missing = "struct P { x: i64, y: i64 }\nfn f() -> P { P { x: 1 } }\n";
        let err_m = parse_module(bad_missing).expect_err("missing field must fail");
        assert_eq!(err_m.code, "E063_MISSING_FIELD");

        // Unknown field error
        let bad_unk = "struct P { x: i64, y: i64 }\nfn f() -> P { P { x: 1, y: 2, z: 3 } }\n";
        let err_u = parse_module(bad_unk).expect_err("unknown field must fail");
        assert_eq!(err_u.code, "E063_UNKNOWN_FIELD");

        // Duplicate field error
        let bad_dup = "struct P { x: i64, y: i64 }\nfn f() -> P { P { x: 1, x: 2, y: 3 } }\n";
        let err_d = parse_module(bad_dup).expect_err("duplicate field must fail");
        assert_eq!(err_d.code, "E063_DUPLICATE_FIELD");
    }
}
