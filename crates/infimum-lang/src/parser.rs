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

#[derive(Clone)]
struct RawUseItem {
    name: String,
    alias: Option<String>,
    span: Span,
}

#[derive(Clone)]
enum RawUseKind {
    All,
    Alias(String, Span),
    Selective(Vec<RawUseItem>),
}

#[derive(Clone)]
struct RawUse {
    is_pub: bool,
    path: String,
    kind: RawUseKind,
    span: Span,
}

struct RawMod {
    name: String,
    is_pub: bool,
    span: Span,
    uses: Vec<RawUse>,
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
    is_pub: bool,
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
    is_pub: bool,
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
    struct_module: Option<String>,
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
    is_pub: bool,
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
    Path(Vec<String>, Span),
}

#[derive(Clone)]
pub(crate) struct RawMatchArm {
    patterns: Vec<RawMatchPattern>,
    body: RawExpr,
}

#[derive(Clone)]
pub(crate) enum RawType {
    Builtin(Type),
    Named {
        module: Option<String>,
        name: String,
        span: Span,
    },
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
        module: Option<String>,
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
pub(crate) struct RawLayoutLoad {
    module: Option<String>,
    layout: String,
    field: String,
    index: Option<Box<RawExpr>>,
    subfield: Option<String>,
    span: Span,
}

#[derive(Clone)]
pub(crate) struct RawModCall {
    module: String,
    func: String,
    args: Vec<RawExpr>,
    span: Span,
}

#[derive(Clone)]
pub(crate) struct RawStructLit {
    module: Option<String>,
    name: String,
    fields: Vec<(String, RawExpr, Span)>,
    span: Span,
}

#[derive(Clone)]
pub(crate) enum RawExpr {
    IntLit(i64),
    FixedLit(i64),
    BoolLit(bool),
    StrLit(String),
    UnitLit,
    Var(String),
    Path {
        segs: Vec<String>,
        span: Span,
    },
    LayoutLoad(Box<RawLayoutLoad>),
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
    ModCall(Box<RawModCall>),
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
    StructLit(Box<RawStructLit>),
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

// ---------------------------------------------------------------------------
// Unified UnitLoader & UnitGraph (Pascal-style isolated compilation units)
// ---------------------------------------------------------------------------

type UnitId = usize;

struct RawUnit {
    id: UnitId,
    display_label: String,
    parent: Option<UnitId>,
    inline_mod: Option<(String, bool, Span)>,
    is_entry: bool,
    is_forma_rt: bool,
    uses: Vec<(RawUse, UnitId)>,
    items: Vec<RawItem>,
    formas: Vec<crate::forma::FormaDef<RawExpr>>,
    functions: Vec<RawFnDef>,
    inline_children: Vec<UnitId>,
}

struct UnitGraph {
    units: Vec<RawUnit>,
    by_key: BTreeMap<String, UnitId>,
    topo_order: Vec<UnitId>,
    entry_uid: UnitId,
}

impl UnitGraph {
    fn find_unit(&self, canonical_key: &str) -> Option<UnitId> {
        self.by_key.get(canonical_key).copied()
    }
}

trait FileSource {
    /// Resolve a `use "<spec>"` relative to `requester_key` into `(canonical_key, display_label, source_text)`.
    fn resolve_use(
        &self,
        requester_key: &str,
        spec: &str,
        use_span: Span,
    ) -> Result<(String, String, String), InfimumDiagnostic>;
}

struct MemorySource;

impl FileSource for MemorySource {
    fn resolve_use(
        &self,
        _requester_key: &str,
        spec: &str,
        use_span: Span,
    ) -> Result<(String, String, String), InfimumDiagnostic> {
        Err(InfimumDiagnostic::new(
            "E018_USE_WITHOUT_PATH",
            "parse",
            format!("Cannot resolve 'use \"{spec}\"' without a source file path"),
            use_span,
            "Load multi-file modules with parse_module_from_path(entry_path) or CLI file commands",
        ))
    }
}

struct DiskSource {
    entry_dir: Option<PathBuf>,
}

impl DiskSource {
    fn display_for(&self, canon: &Path) -> String {
        if let Some(ed) = &self.entry_dir {
            if let Ok(rel) = canon.strip_prefix(ed) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
        canon
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| canon.display().to_string())
    }
}

impl FileSource for DiskSource {
    fn resolve_use(
        &self,
        requester_key: &str,
        spec: &str,
        use_span: Span,
    ) -> Result<(String, String, String), InfimumDiagnostic> {
        let base_dir = Path::new(requester_key)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let dep_path = base_dir.join(spec);
        let canon = fs::canonicalize(&dep_path).map_err(|e| {
            InfimumDiagnostic::new(
                "E040_FILE_READ",
                "parse",
                format!("Failed to resolve '{}': {e}", dep_path.display()),
                use_span,
                "Check that the imported .ifm file path exists relative to the importing file",
            )
        })?;
        let text = fs::read_to_string(&canon).map_err(|e| {
            InfimumDiagnostic::new(
                "E040_FILE_READ",
                "parse",
                format!("Failed to read '{}': {e}", canon.display()),
                use_span,
                "Ensure the .ifm file is readable UTF-8 text",
            )
        })?;
        let label = self.display_for(&canon);
        Ok((canon.to_string_lossy().to_string(), label, text))
    }
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

struct VirtualFsSource<'a> {
    files: &'a BTreeMap<String, String>,
}

impl<'a> FileSource for VirtualFsSource<'a> {
    fn resolve_use(
        &self,
        requester_key: &str,
        spec: &str,
        use_span: Span,
    ) -> Result<(String, String, String), InfimumDiagnostic> {
        let base_dir = virtual_parent_dir(requester_key);
        let dep_key = normalize_virtual_rel(base_dir, spec);
        let Some(text) = self.files.get(&dep_key) else {
            return Err(InfimumDiagnostic::new(
                "E040_FILE_READ",
                "parse",
                format!("Failed to resolve virtual import '{dep_key}'"),
                use_span,
                "Check that the imported .ifm file exists in the project workspace",
            ));
        };
        Ok((dep_key.clone(), dep_key, text.clone()))
    }
}

struct UnitLoader<'a, S: FileSource> {
    source: &'a S,
    units: Vec<RawUnit>,
    by_key: BTreeMap<String, UnitId>,
    visiting: Vec<(String, String)>, // (canonical_key, display_label)
    topo_order: Vec<UnitId>,
}

impl<'a, S: FileSource> UnitLoader<'a, S> {
    fn new(source: &'a S) -> Self {
        Self {
            source,
            units: Vec::new(),
            by_key: BTreeMap::new(),
            visiting: Vec::new(),
            topo_order: Vec::new(),
        }
    }

    fn resolve_spec(
        &self,
        requester_key: &str,
        spec: &str,
        use_span: Span,
    ) -> Result<(String, String, String), InfimumDiagnostic> {
        if spec == "__forma__/widgets_rt.ifm" {
            return Ok((
                "__forma__/widgets_rt.ifm".to_string(),
                "forma/widgets_rt.ifm".to_string(),
                include_str!("forma/widgets_rt.ifm").to_string(),
            ));
        }
        if spec.starts_with("std/") {
            let Some((virtual_key, text)) = resolve_embedded_std(spec) else {
                return Err(InfimumDiagnostic::new(
                    "E040_FILE_READ",
                    "parse",
                    format!("Unknown built-in Infimum stdlib module '{spec}'"),
                    use_span,
                    "Available stdlib modules: std/prelude.ifm, std/sys_mem.ifm, std/mem.ifm, std/math.ifm, std/color.ifm, std/gfx.ifm, std/mesh3d.ifm, std/input.ifm, std/text.ifm, std/audio.ifm, std/fmt.ifm, std/theme.ifm, std/icons.ifm, std/ui.ifm, std/text_edit.ifm, std/markdown.ifm, std/buf.ifm, std/json.ifm",
                ));
            };
            let label = format!("std/{}", virtual_key.trim_start_matches("__std__/"));
            return Ok((virtual_key.to_string(), label, text.to_string()));
        }
        self.source.resolve_use(requester_key, spec, use_span)
    }

    fn load_unit(
        &mut self,
        canonical_key: String,
        display_label: String,
        src: &str,
        is_entry: bool,
        is_forma_rt: bool,
        use_span: Span,
    ) -> Result<UnitId, InfimumDiagnostic> {
        if let Some(pos) = self.visiting.iter().position(|(k, _)| k == &canonical_key) {
            let mut chain: Vec<&str> = self.visiting[pos..]
                .iter()
                .map(|(_, lbl)| lbl.as_str())
                .collect();
            chain.push(&display_label);
            return Err(InfimumDiagnostic::new(
                "E019_IMPORT_CYCLE",
                "parse",
                format!("Circular import detected: {}", chain.join(" -> ")),
                use_span,
                "Break the circular 'use' dependency between modules",
            ));
        }
        if let Some(&uid) = self.by_key.get(&canonical_key) {
            return Ok(uid);
        }

        let mut raw = parse_raw_file(src).map_err(|d| {
            if is_entry && display_label == "<memory>" {
                d
            } else {
                diag_with_file(d, &display_label)
            }
        })?;
        tag_raw_file_source(&mut raw, &display_label);

        self.visiting
            .push((canonical_key.clone(), display_label.clone()));

        let mut resolved_uses = Vec::with_capacity(raw.uses.len());
        for u in raw.uses {
            let (dep_key, dep_label, dep_src) =
                self.resolve_spec(&canonical_key, &u.path, u.span)?;
            let dep_uid = self.load_unit(dep_key, dep_label, &dep_src, false, false, u.span)?;
            resolved_uses.push((u, dep_uid));
        }

        // Separate inline `mod name { ... }` items into child RawUnits
        let mut own_items = Vec::new();
        let mut raw_inline_mods = Vec::new();
        for item in raw.items {
            match item {
                RawItem::Mod(rm) => raw_inline_mods.push(rm),
                other => own_items.push(other),
            }
        }

        self.visiting.pop();

        let uid = self.units.len();
        self.units.push(RawUnit {
            id: uid,
            display_label: display_label.clone(),
            parent: None,
            inline_mod: None,
            is_entry,
            is_forma_rt,
            uses: resolved_uses,
            items: own_items,
            formas: raw.formas,
            functions: raw.functions,
            inline_children: Vec::new(),
        });
        self.by_key.insert(canonical_key.clone(), uid);
        self.topo_order.push(uid);

        // Register inline `mod` children (their `use` directives resolve relative to parent file)
        for rm in raw_inline_mods {
            let child_uid = self.register_inline_mod(uid, &canonical_key, &display_label, rm)?;
            self.units[uid].inline_children.push(child_uid);
        }

        Ok(uid)
    }

    fn register_inline_mod(
        &mut self,
        parent_uid: UnitId,
        parent_key: &str,
        parent_label: &str,
        rm: RawMod,
    ) -> Result<UnitId, InfimumDiagnostic> {
        let mut resolved_uses = Vec::with_capacity(rm.uses.len());
        for u in rm.uses {
            let (dep_key, dep_label, dep_src) = self.resolve_spec(parent_key, &u.path, u.span)?;
            let dep_uid = self.load_unit(dep_key, dep_label, &dep_src, false, false, u.span)?;
            resolved_uses.push((u, dep_uid));
        }

        let mut own_items = Vec::new();
        let mut nested_mods = Vec::new();
        for item in rm.items {
            match item {
                RawItem::Mod(nested) => nested_mods.push(nested),
                other => own_items.push(other),
            }
        }

        let child_uid = self.units.len();
        let child_label = parent_label.to_string();
        self.units.push(RawUnit {
            id: child_uid,
            display_label: child_label,
            parent: Some(parent_uid),
            inline_mod: Some((rm.name, rm.is_pub, rm.span)),
            is_entry: false,
            is_forma_rt: false,
            uses: resolved_uses,
            items: own_items,
            formas: rm.formas,
            functions: rm.functions,
            inline_children: Vec::new(),
        });
        self.topo_order.push(child_uid);

        for nested in nested_mods {
            let n_uid = self.register_inline_mod(child_uid, parent_key, parent_label, nested)?;
            self.units[child_uid].inline_children.push(n_uid);
        }

        Ok(child_uid)
    }

    fn finish(mut self, entry_uid: UnitId) -> Result<UnitGraph, InfimumDiagnostic> {
        let has_forma = self.units.iter().any(|u| !u.formas.is_empty());
        if has_forma {
            let (k, l, s) =
                self.resolve_spec("", "__forma__/widgets_rt.ifm", Span::zero())?;
            let _ = self.load_unit(k, l, &s, false, true, Span::zero())?;
        }
        Ok(UnitGraph {
            units: self.units,
            by_key: self.by_key,
            topo_order: self.topo_order,
            entry_uid,
        })
    }
}

pub fn parse_module(src: &str) -> Result<Module, InfimumDiagnostic> {
    let source = MemorySource;
    let mut loader = UnitLoader::new(&source);
    let entry_uid = loader.load_unit(
        "<memory>".to_string(),
        "<memory>".to_string(),
        src,
        true,
        false,
        Span::zero(),
    )?;
    let graph = loader.finish(entry_uid)?;
    resolve_and_lower(graph)
}

/// Load and parse a multi-file Infimum module starting at `entry_path`.
///
/// Resolves `[pub] use "..."` directives recursively relative to each file's directory,
/// isolates every file as a Pascal-style unit (`pub` interface / private implementation),
/// deduplicates diamond imports by canonical path, detects import cycles, and collects all
/// declarations in deterministic topological order (dependencies before dependents).
pub fn parse_module_from_path(entry_path: &Path) -> Result<Module, InfimumDiagnostic> {
    let canon = fs::canonicalize(entry_path).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Failed to resolve '{}': {e}", entry_path.display()),
            Span::zero(),
            "Check that the entry .ifm file path exists",
        )
    })?;
    let src = fs::read_to_string(&canon).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Failed to read '{}': {e}", canon.display()),
            Span::zero(),
            "Ensure the .ifm file is readable UTF-8 text",
        )
    })?;
    let entry_dir = canon.parent().map(|d| d.to_path_buf());
    let source = DiskSource { entry_dir };
    let label = source.display_for(&canon);
    let mut loader = UnitLoader::new(&source);
    let entry_uid = loader.load_unit(
        canon.to_string_lossy().to_string(),
        label,
        &src,
        true,
        false,
        Span::zero(),
    )?;
    let graph = loader.finish(entry_uid)?;
    resolve_and_lower(graph)
}

/// Load and parse a multi-file Infimum module from an in-memory virtual filesystem
/// (used by Online Infimum Code / `wasm32`) while preserving per-file names and spans.
pub fn parse_module_from_virtual_fs(
    entry_path: &str,
    files: &BTreeMap<String, String>,
) -> Result<Module, InfimumDiagnostic> {
    let norm = normalize_virtual_rel("", entry_path);
    let Some(src) = files.get(&norm) else {
        return Err(InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Failed to resolve virtual entry file '{norm}'"),
            Span::zero(),
            "Check that the entry .ifm file exists in the project workspace",
        ));
    };
    let source = VirtualFsSource { files };
    let mut loader = UnitLoader::new(&source);
    let entry_uid = loader.load_unit(norm.clone(), norm, src, true, false, Span::zero())?;
    let graph = loader.finish(entry_uid)?;
    resolve_and_lower(graph)
}

fn parse_raw_file(src: &str) -> Result<RawFile, InfimumDiagnostic> {
    let tokens = tokenize(src)?;
    let mut parser = Parser { tokens, idx: 0 };
    let mut uses = Vec::new();
    let mut items = Vec::new();
    let mut formas = Vec::new();
    let mut functions = Vec::new();

    while !parser.at_eof() {
        let mut is_pub = if parser.peek_is_contextual_kw("pub") {
            parser.advance();
            true
        } else {
            false
        };
        let mut is_export = false;
        if matches!(parser.peek().kind, TokenKind::Export) {
            parser.advance();
            is_export = true;
            if !is_pub && parser.peek_is_contextual_kw("pub") {
                parser.advance();
                is_pub = true;
            }
        }
        match parser.peek().kind {
            TokenKind::Use if !is_export => uses.push(parser.parse_use(is_pub)?),
            TokenKind::Const if !is_export => {
                let mut c = parser.parse_const()?;
                c.is_pub = is_pub;
                items.push(RawItem::Const(c));
            }
            TokenKind::Enum if !is_export => {
                let mut e = parser.parse_enum()?;
                e.is_pub = is_pub;
                items.push(RawItem::Enum(e));
            }
            TokenKind::Layout if !is_export => {
                let mut l = parser.parse_layout()?;
                l.is_pub = is_pub;
                items.push(RawItem::Layout(l));
            }
            _ if !is_export && parser.peek_is_contextual_kw("mod") => {
                let mut rm = parser.parse_mod()?;
                rm.is_pub = is_pub;
                items.push(RawItem::Mod(rm));
            }
            _ if !is_export && parser.peek_is_contextual_kw("struct") => {
                let mut st = parser.parse_struct()?;
                st.is_pub = is_pub;
                items.push(RawItem::Struct(st));
            }
            _ if !is_export && parser.peek_is_contextual_kw("test") => {
                functions.push(parser.parse_test_fn()?)
            }
            TokenKind::Forma => formas.push(crate::forma::parser::parse_forma_def(
                &mut parser,
                is_export,
                is_pub,
            )?),
            _ => {
                let mut f = parser.parse_fn_with_export(is_export)?;
                f.is_pub = is_pub;
                functions.push(f);
            }
        }
    }

    Ok(RawFile {
        uses,
        items,
        formas,
        functions,
    })
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

    fn parse_use(&mut self, is_pub: bool) -> Result<RawUse, InfimumDiagnostic> {
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
                    "Write import directives as: use \"relative/path.ifm\"; or use \"file.ifm\" as alias; or use \"file.ifm\" { a, b as c };",
                ));
            }
        };
        let kind = if self.peek_is_contextual_kw("as") {
            self.advance();
            let (alias, alias_span) = self.expect_ident("Expected namespace alias after 'as'")?;
            RawUseKind::Alias(alias, alias_span)
        } else if matches!(self.peek().kind, TokenKind::LBrace) {
            self.advance();
            let mut items = Vec::new();
            while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                let (item_name, item_span) =
                    self.expect_ident("Expected imported symbol name inside '{ ... }'")?;
                let alias = if self.peek_is_contextual_kw("as") {
                    self.advance();
                    let (al, _) =
                        self.expect_ident("Expected alias name after 'as' in selective import")?;
                    Some(al)
                } else {
                    None
                };
                items.push(RawUseItem {
                    name: item_name,
                    alias,
                    span: item_span,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::RBrace, "Expected '}' to close selective import list")?;
            RawUseKind::Selective(items)
        } else {
            RawUseKind::All
        };
        self.expect(&TokenKind::Semi, "Terminate 'use' directive with ';'")?;
        Ok(RawUse {
            is_pub,
            path,
            kind,
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

    fn parse_mod(&mut self) -> Result<RawMod, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'mod'")?;
        let (name, _) = self.expect_ident("Expected module name after 'mod'")?;
        if self.peek_is_contextual_kw("from") {
            let from_span = self.peek().span;
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("'mod {name} from \"...\";' has been removed; use 'use \"file.ifm\" as {name};' instead"),
                from_span,
                format!("Replace with: use \"<file>.ifm\" as {name};"),
            ));
        }
        self.expect(&TokenKind::LBrace, "Expected '{' after inline module name")?;
        let mut uses = Vec::new();
        let mut items = Vec::new();
        let mut formas = Vec::new();
        let mut functions = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let mut is_pub = if self.peek_is_contextual_kw("pub") {
                self.advance();
                true
            } else {
                false
            };
            let mut is_export = false;
            if matches!(self.peek().kind, TokenKind::Export) {
                self.advance();
                is_export = true;
                if !is_pub && self.peek_is_contextual_kw("pub") {
                    self.advance();
                    is_pub = true;
                }
            }
            match self.peek().kind {
                TokenKind::Use if !is_export => uses.push(self.parse_use(is_pub)?),
                TokenKind::Const if !is_export => {
                    let mut c = self.parse_const()?;
                    c.is_pub = is_pub;
                    items.push(RawItem::Const(c));
                }
                TokenKind::Enum if !is_export => {
                    let mut e = self.parse_enum()?;
                    e.is_pub = is_pub;
                    items.push(RawItem::Enum(e));
                }
                TokenKind::Layout if !is_export => {
                    let mut l = self.parse_layout()?;
                    l.is_pub = is_pub;
                    items.push(RawItem::Layout(l));
                }
                _ if !is_export && self.peek_is_contextual_kw("mod") => {
                    let mut nested = self.parse_mod()?;
                    nested.is_pub = is_pub;
                    items.push(RawItem::Mod(nested));
                }
                _ if !is_export && self.peek_is_contextual_kw("struct") => {
                    let mut st = self.parse_struct()?;
                    st.is_pub = is_pub;
                    items.push(RawItem::Struct(st));
                }
                TokenKind::Forma => {
                    formas.push(crate::forma::parser::parse_forma_def(self, is_export, is_pub)?)
                }
                _ => {
                    let mut f = self.parse_fn_with_export(is_export)?;
                    f.is_pub = is_pub;
                    functions.push(f);
                }
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close module body")?;
        Ok(RawMod {
            name,
            is_pub: false,
            span,
            uses,
            items,
            formas,
            functions,
        })
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
            is_pub: false,
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
        Ok(RawStruct {
            name,
            is_pub: false,
            fields,
            span,
        })
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
            let (struct_module, struct_type, is_array, is_fixed, len_expr) =
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (sname, _) =
                        self.expect_ident("Expected struct name after '::' in layout field")?;
                    if matches!(self.peek().kind, TokenKind::LBracket) {
                        self.advance();
                        let len = self.parse_expr()?;
                        self.expect(&TokenKind::RBracket, "Expected ']' after struct array length")?;
                        (Some(kind_kw), Some(sname), true, false, Some(len))
                    } else {
                        (Some(kind_kw), Some(sname), false, false, None)
                    }
                } else {
                    match kind_kw.as_str() {
                        "word" => (None, None, false, false, None),
                        "words" => {
                            self.expect(
                                &TokenKind::LBracket,
                                "Expected '[' after 'words' (e.g. words[8200])",
                            )?;
                            let len = self.parse_expr()?;
                            self.expect(&TokenKind::RBracket, "Expected ']' after words length")?;
                            (None, None, true, false, Some(len))
                        }
                        "fixed" => {
                            if matches!(self.peek().kind, TokenKind::LBracket) {
                                self.advance();
                                let len = self.parse_expr()?;
                                self.expect(
                                    &TokenKind::RBracket,
                                    "Expected ']' after fixed array length",
                                )?;
                                (None, None, true, true, Some(len))
                            } else {
                                (None, None, false, true, None)
                            }
                        }
                        other => {
                            if matches!(self.peek().kind, TokenKind::LBracket) {
                                self.advance();
                                let len = self.parse_expr()?;
                                self.expect(
                                    &TokenKind::RBracket,
                                    "Expected ']' after struct array length",
                                )?;
                                (None, Some(other.to_string()), true, false, Some(len))
                            } else {
                                (None, Some(other.to_string()), false, false, None)
                            }
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
                struct_module,
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
            is_pub: false,
            base_expr,
            fields,
            span: tok.span,
        })
    }

    fn parse_fn_with_export(&mut self, exported_already: bool) -> Result<RawFnDef, InfimumDiagnostic> {
        let exported = if exported_already {
            true
        } else if matches!(self.peek().kind, TokenKind::Export) {
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
            _ => {
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (sname, _) = self.expect_ident("Expected type name after '::'")?;
                    Ok(RawType::Named {
                        module: Some(tname),
                        name: sname,
                        span,
                    })
                } else {
                    Ok(RawType::Named {
                        module: None,
                        name: tname,
                        span,
                    })
                }
            }
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
            // Layout assignment: `[mod::]Layout.field`, `...field[i]`, `...field.sub`, `...field[i].sub`
            let is_direct_layout = matches!(self.peek_at(1), TokenKind::Dot)
                && matches!(self.peek_at(2), TokenKind::Ident(_));
            let is_mod_layout = matches!(self.peek_at(1), TokenKind::ColonColon)
                && matches!(self.peek_at(2), TokenKind::Ident(_))
                && matches!(self.peek_at(3), TokenKind::Dot)
                && matches!(self.peek_at(4), TokenKind::Ident(_));
            if is_direct_layout || is_mod_layout {
                let saved = self.idx;
                let (module, layout, span) = if is_mod_layout {
                    let (m, sp) = self.expect_ident("Expected module name")?;
                    self.advance(); // '::'
                    let (l, _) = self.expect_ident("Expected layout name")?;
                    (Some(m), l, sp)
                } else {
                    let (l, sp) = self.expect_ident("Expected layout name")?;
                    (None, l, sp)
                };
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
                        module,
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
                    let mut segs = vec![name];
                    while matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (seg, _) = self.expect_ident("Expected identifier after '::'")?;
                        segs.push(seg);
                    }
                    if segs.len() == 2 && matches!(self.peek().kind, TokenKind::LParen) {
                        let args = self.parse_arg_list()?;
                        let mut expr = RawExpr::ModCall(Box::new(RawModCall {
                            module: segs.remove(0),
                            func: segs.remove(0),
                            args,
                            span: tok.span,
                        }));
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
                        Ok(RawExpr::Path {
                            segs,
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
                    let mut segs = vec![name];
                    while matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (seg, _) = self.expect_ident("Expected variant or member name after '::'")?;
                        segs.push(seg);
                    }
                    Ok(RawMatchPattern::Path(segs, tok.span))
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
                    let mut segs = vec![name];
                    while matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (seg, _) = self.expect_ident("Expected member name after '::'")?;
                        segs.push(seg);
                    }
                    if segs.len() == 2 && matches!(self.peek().kind, TokenKind::LParen) {
                        let args = self.parse_arg_list()?;
                        let mut expr = RawExpr::ModCall(Box::new(RawModCall {
                            module: segs.remove(0),
                            func: segs.remove(0),
                            args,
                            span: tok.span,
                        }));
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
                    } else if segs.len() == 2
                        && matches!(self.peek().kind, TokenKind::LBrace)
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
                        let mut expr = RawExpr::StructLit(Box::new(RawStructLit {
                            module: Some(segs.remove(0)),
                            name: segs.remove(0),
                            fields,
                            span: tok.span,
                        }));
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
                    } else if segs.len() == 2
                        && matches!(self.peek().kind, TokenKind::Dot)
                        && matches!(self.peek_next(), TokenKind::Ident(_))
                    {
                        self.advance(); // '.'
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
                        Ok(RawExpr::LayoutLoad(Box::new(RawLayoutLoad {
                            module: Some(segs.remove(0)),
                            layout: segs.remove(0),
                            field,
                            index,
                            subfield,
                            span: tok.span,
                        })))
                    } else {
                        Ok(RawExpr::Path {
                            segs,
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
                    Ok(RawExpr::LayoutLoad(Box::new(RawLayoutLoad {
                        module: None,
                        layout: name,
                        field,
                        index,
                        subfield,
                        span: tok.span,
                    })))
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
                    let mut expr = RawExpr::StructLit(Box::new(RawStructLit {
                        module: None,
                        name,
                        fields,
                        span: tok.span,
                    }));
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

#[derive(Clone, PartialEq, Eq)]
struct SymRef {
    owner: UnitId,
    orig_name: String,
    canonical_name: String,
    decl_file: String,
    decl_span: Span,
}

#[derive(Clone, PartialEq, Eq)]
struct NamespaceRef {
    target_uid: UnitId,
    decl_file: String,
    decl_span: Span,
}

#[derive(Clone, Default)]
struct SymbolTable {
    fns: BTreeMap<String, SymRef>,
    consts: BTreeMap<String, SymRef>,
    enums: BTreeMap<String, SymRef>,
    structs: BTreeMap<String, SymRef>,
    layouts: BTreeMap<String, SymRef>,
    formas: BTreeMap<String, SymRef>,
    namespaces: BTreeMap<String, NamespaceRef>,
    // Insertion order for deterministic ABI export
    consts_order: Vec<String>,
    enums_order: Vec<String>,
    layouts_order: Vec<String>,
}

#[derive(Clone, Default)]
struct UnitTables {
    exports: SymbolTable,
    scope: SymbolTable,
}

fn unit_canonical_name(u: &RawUnit, name: &str, is_exported_fn: bool) -> String {
    if u.is_entry || is_exported_fn || (u.is_forma_rt && name.starts_with("__forma_")) {
        name.to_string()
    } else {
        format!("u{}__{name}", u.id)
    }
}

fn insert_sym(
    map: &mut BTreeMap<String, SymRef>,
    order: Option<&mut Vec<String>>,
    key: String,
    sym: SymRef,
    kind_label: &str,
    use_span: Span,
    is_explicit_alias: bool,
) -> Result<(), InfimumDiagnostic> {
    if let Some(existing) = map.get(&key) {
        if existing.owner == sym.owner && existing.orig_name == sym.orig_name {
            return Ok(());
        }
        let code = if is_explicit_alias {
            "E069_DUPLICATE_ALIAS"
        } else {
            "E024_DUPLICATE_SYMBOL"
        };
        let err_span = if use_span.line > 0 { use_span } else { sym.decl_span };
        return Err(InfimumDiagnostic::new(
            code,
            "resolve",
            format!(
                "Duplicate {kind_label} '{key}': defined at {}:{}:{} and {}:{}:{}",
                existing.decl_file,
                existing.decl_span.line,
                existing.decl_span.col,
                sym.decl_file,
                sym.decl_span.line,
                sym.decl_span.col
            ),
            err_span,
            format!(
                "Disambiguate '{key}' with a namespace import (use \"file.ifm\" as alias;) or selective alias (use \"file.ifm\" {{ {key} as other }};)"
            ),
        ));
    }
    if let Some(ord) = order {
        ord.push(key.clone());
    }
    map.insert(key, sym);
    Ok(())
}

fn insert_namespace(
    map: &mut BTreeMap<String, NamespaceRef>,
    alias: String,
    ns: NamespaceRef,
    use_span: Span,
) -> Result<(), InfimumDiagnostic> {
    if let Some(existing) = map.get(&alias) {
        if existing.target_uid == ns.target_uid {
            return Ok(());
        }
        return Err(InfimumDiagnostic::new(
            "E069_DUPLICATE_ALIAS",
            "resolve",
            format!(
                "Duplicate module alias '{alias}' (previously bound at {}:{}:{})",
                existing.decl_file, existing.decl_span.line, existing.decl_span.col
            ),
            if use_span.line > 0 { use_span } else { ns.decl_span },
            format!("Choose a distinct namespace alias instead of '{alias}'"),
        ));
    }
    map.insert(alias, ns);
    Ok(())
}

fn merge_table_all(
    dst: &mut SymbolTable,
    src: &SymbolTable,
    use_span: Span,
) -> Result<(), InfimumDiagnostic> {
    for (k, v) in &src.fns {
        insert_sym(&mut dst.fns, None, k.clone(), v.clone(), "function", use_span, false)?;
    }
    for k in &src.consts_order {
        if let Some(v) = src.consts.get(k) {
            insert_sym(
                &mut dst.consts,
                Some(&mut dst.consts_order),
                k.clone(),
                v.clone(),
                "const",
                use_span,
                false,
            )?;
        }
    }
    for k in &src.enums_order {
        if let Some(v) = src.enums.get(k) {
            insert_sym(
                &mut dst.enums,
                Some(&mut dst.enums_order),
                k.clone(),
                v.clone(),
                "enum",
                use_span,
                false,
            )?;
        }
    }
    for (k, v) in &src.structs {
        insert_sym(&mut dst.structs, None, k.clone(), v.clone(), "struct", use_span, false)?;
    }
    for k in &src.layouts_order {
        if let Some(v) = src.layouts.get(k) {
            insert_sym(
                &mut dst.layouts,
                Some(&mut dst.layouts_order),
                k.clone(),
                v.clone(),
                "layout",
                use_span,
                false,
            )?;
        }
    }
    for (k, v) in &src.formas {
        insert_sym(&mut dst.formas, None, k.clone(), v.clone(), "forma", use_span, false)?;
    }
    for (k, v) in &src.namespaces {
        insert_namespace(&mut dst.namespaces, k.clone(), v.clone(), use_span)?;
    }
    Ok(())
}

fn build_unit_tables(graph: &UnitGraph) -> Result<Vec<UnitTables>, InfimumDiagnostic> {
    let mut tables = vec![UnitTables::default(); graph.units.len()];
    let mut global_exports: BTreeMap<String, (String, Span)> = BTreeMap::new();

    for &uid in &graph.topo_order {
        let u = &graph.units[uid];

        // Inline `mod` inherits parent unit's visible scope before adding its own uses/decls
        if let Some(parent_uid) = u.parent {
            tables[uid].scope = tables[parent_uid].scope.clone();
        }

        // 1. Register own declarations
        {
            let ut = &mut tables[uid];
            for item in &u.items {
                match item {
                    RawItem::Const(rc) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: rc.name.clone(),
                            canonical_name: unit_canonical_name(u, &rc.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: rc.span,
                        };
                        if u.parent.is_some() {
                            ut.scope.consts.insert(rc.name.clone(), sym.clone());
                        } else {
                            insert_sym(
                                &mut ut.scope.consts,
                                Some(&mut ut.scope.consts_order),
                                rc.name.clone(),
                                sym.clone(),
                                "const",
                                rc.span,
                                false,
                            )?;
                        }
                        if rc.is_pub {
                            insert_sym(
                                &mut ut.exports.consts,
                                Some(&mut ut.exports.consts_order),
                                rc.name.clone(),
                                sym,
                                "const",
                                rc.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Enum(re) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: re.name.clone(),
                            canonical_name: unit_canonical_name(u, &re.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: re.span,
                        };
                        if u.parent.is_some() {
                            ut.scope.enums.insert(re.name.clone(), sym.clone());
                        } else {
                            insert_sym(
                                &mut ut.scope.enums,
                                Some(&mut ut.scope.enums_order),
                                re.name.clone(),
                                sym.clone(),
                                "enum",
                                re.span,
                                false,
                            )?;
                        }
                        if re.is_pub {
                            insert_sym(
                                &mut ut.exports.enums,
                                Some(&mut ut.exports.enums_order),
                                re.name.clone(),
                                sym,
                                "enum",
                                re.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Struct(rs) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: rs.name.clone(),
                            canonical_name: unit_canonical_name(u, &rs.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: rs.span,
                        };
                        if u.parent.is_some() {
                            ut.scope.structs.insert(rs.name.clone(), sym.clone());
                        } else {
                            insert_sym(
                                &mut ut.scope.structs,
                                None,
                                rs.name.clone(),
                                sym.clone(),
                                "struct",
                                rs.span,
                                false,
                            )?;
                        }
                        if rs.is_pub {
                            insert_sym(
                                &mut ut.exports.structs,
                                None,
                                rs.name.clone(),
                                sym,
                                "struct",
                                rs.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Layout(rl) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: rl.name.clone(),
                            canonical_name: unit_canonical_name(u, &rl.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: rl.span,
                        };
                        if u.parent.is_some() {
                            ut.scope.layouts.insert(rl.name.clone(), sym.clone());
                        } else {
                            insert_sym(
                                &mut ut.scope.layouts,
                                Some(&mut ut.scope.layouts_order),
                                rl.name.clone(),
                                sym.clone(),
                                "layout",
                                rl.span,
                                false,
                            )?;
                        }
                        if rl.is_pub {
                            insert_sym(
                                &mut ut.exports.layouts,
                                Some(&mut ut.exports.layouts_order),
                                rl.name.clone(),
                                sym,
                                "layout",
                                rl.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Mod(_) => {}
                }
            }

            for rf in &u.formas {
                let sym = SymRef {
                    owner: uid,
                    orig_name: rf.name.clone(),
                    canonical_name: unit_canonical_name(u, &rf.name, rf.exported),
                    decl_file: u.display_label.clone(),
                    decl_span: rf.span,
                };
                if u.parent.is_some() {
                    ut.scope.formas.insert(rf.name.clone(), sym.clone());
                    ut.scope.fns.insert(rf.name.clone(), sym.clone());
                } else {
                    insert_sym(&mut ut.scope.formas, None, rf.name.clone(), sym.clone(), "forma", rf.span, false)?;
                    insert_sym(&mut ut.scope.fns, None, rf.name.clone(), sym.clone(), "function", rf.span, false)?;
                }
                if rf.is_pub {
                    insert_sym(&mut ut.exports.formas, None, rf.name.clone(), sym.clone(), "forma", rf.span, false)?;
                    insert_sym(&mut ut.exports.fns, None, rf.name.clone(), sym, "function", rf.span, false)?;
                }
            }

            for rf in &u.functions {
                if rf.exported {
                    if let Some((prev_file, prev_span)) = global_exports.get(&rf.name) {
                        return Err(InfimumDiagnostic::new(
                            "E024_DUPLICATE_SYMBOL",
                            "resolve",
                            format!(
                                "Duplicate Wasm export function '{}': defined at {}:{}:{} and {}:{}:{}",
                                rf.name, prev_file, prev_span.line, prev_span.col, u.display_label, rf.span.line, rf.span.col
                            ),
                            rf.span,
                            "Wasm 'export fn' names must be globally unique across the capsule",
                        ));
                    }
                    global_exports.insert(rf.name.clone(), (u.display_label.clone(), rf.span));
                }
                let sym = SymRef {
                    owner: uid,
                    orig_name: rf.name.clone(),
                    canonical_name: unit_canonical_name(u, &rf.name, rf.exported),
                    decl_file: u.display_label.clone(),
                    decl_span: rf.span,
                };
                if u.parent.is_some() {
                    ut.scope.fns.insert(rf.name.clone(), sym.clone());
                } else {
                    insert_sym(&mut ut.scope.fns, None, rf.name.clone(), sym.clone(), "function", rf.span, false)?;
                }
                if rf.is_pub {
                    insert_sym(&mut ut.exports.fns, None, rf.name.clone(), sym, "function", rf.span, false)?;
                }
            }
        }

        // 2. Process all `use` / `pub use` directives (targets already built in topo_order)
        for (raw_use, dep_uid) in &u.uses {
            let dep_exports = tables[*dep_uid].exports.clone();
            let dep_scope = tables[*dep_uid].scope.clone();
            let dep_label = graph.units[*dep_uid].display_label.clone();
            let ut = &mut tables[uid];
            match &raw_use.kind {
                RawUseKind::All => {
                    merge_table_all(&mut ut.scope, &dep_exports, raw_use.span)?;
                    if raw_use.is_pub {
                        merge_table_all(&mut ut.exports, &dep_exports, raw_use.span)?;
                    }
                }
                RawUseKind::Alias(alias, alias_span) => {
                    let ns = NamespaceRef {
                        target_uid: *dep_uid,
                        decl_file: u.display_label.clone(),
                        decl_span: *alias_span,
                    };
                    insert_namespace(&mut ut.scope.namespaces, alias.clone(), ns.clone(), *alias_span)?;
                    if raw_use.is_pub {
                        insert_namespace(&mut ut.exports.namespaces, alias.clone(), ns, *alias_span)?;
                    }
                }
                RawUseKind::Selective(items) => {
                    for item in items {
                        let local_name = item.alias.as_deref().unwrap_or(&item.name).to_string();
                        let is_al = item.alias.is_some();
                        let mut matched_any = false;

                        if let Some(s) = dep_exports.fns.get(&item.name) {
                            insert_sym(&mut ut.scope.fns, None, local_name.clone(), s.clone(), "function", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.fns, None, local_name.clone(), s.clone(), "function", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.consts.get(&item.name) {
                            insert_sym(&mut ut.scope.consts, Some(&mut ut.scope.consts_order), local_name.clone(), s.clone(), "const", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.consts, Some(&mut ut.exports.consts_order), local_name.clone(), s.clone(), "const", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.enums.get(&item.name) {
                            insert_sym(&mut ut.scope.enums, Some(&mut ut.scope.enums_order), local_name.clone(), s.clone(), "enum", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.enums, Some(&mut ut.exports.enums_order), local_name.clone(), s.clone(), "enum", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.structs.get(&item.name) {
                            insert_sym(&mut ut.scope.structs, None, local_name.clone(), s.clone(), "struct", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.structs, None, local_name.clone(), s.clone(), "struct", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.layouts.get(&item.name) {
                            insert_sym(&mut ut.scope.layouts, Some(&mut ut.scope.layouts_order), local_name.clone(), s.clone(), "layout", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.layouts, Some(&mut ut.exports.layouts_order), local_name.clone(), s.clone(), "layout", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.formas.get(&item.name) {
                            insert_sym(&mut ut.scope.formas, None, local_name.clone(), s.clone(), "forma", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.formas, None, local_name.clone(), s.clone(), "forma", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(ns) = dep_exports.namespaces.get(&item.name) {
                            insert_namespace(&mut ut.scope.namespaces, local_name.clone(), ns.clone(), item.span)?;
                            if raw_use.is_pub {
                                insert_namespace(&mut ut.exports.namespaces, local_name.clone(), ns.clone(), item.span)?;
                            }
                            matched_any = true;
                        }

                        if !matched_any {
                            let is_private_in_dep = dep_scope.fns.contains_key(&item.name)
                                || dep_scope.consts.contains_key(&item.name)
                                || dep_scope.enums.contains_key(&item.name)
                                || dep_scope.structs.contains_key(&item.name)
                                || dep_scope.layouts.contains_key(&item.name)
                                || dep_scope.formas.contains_key(&item.name)
                                || dep_scope.namespaces.contains_key(&item.name);
                            if is_private_in_dep {
                                return Err(InfimumDiagnostic::new(
                                    "E068_PRIVATE_ITEM",
                                    "resolve",
                                    format!("Symbol '{}' in '{}' is private; mark it 'pub' to export", item.name, dep_label),
                                    item.span,
                                    format!("Add 'pub' to '{}' in '{}'", item.name, dep_label),
                                ));
                            } else {
                                return Err(InfimumDiagnostic::new(
                                    "E067_UNKNOWN_IMPORT",
                                    "resolve",
                                    format!("Symbol '{}' not found in '{}'", item.name, dep_label),
                                    item.span,
                                    format!("Check the exported 'pub' declarations in '{}'", dep_label),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 3. If this is an inline `mod`, bind its namespace into parent's scope (and exports if `pub mod`)
        if let (Some(parent_uid), Some((mname, is_pub, mspan))) = (u.parent, &u.inline_mod) {
            let ns = NamespaceRef {
                target_uid: uid,
                decl_file: u.display_label.clone(),
                decl_span: *mspan,
            };
            insert_namespace(&mut tables[parent_uid].scope.namespaces, mname.clone(), ns.clone(), *mspan)?;
            if *is_pub {
                insert_namespace(&mut tables[parent_uid].exports.namespaces, mname.clone(), ns, *mspan)?;
            }
        }
    }

    Ok(tables)
}

struct ModuleResolver {
    tables: Vec<UnitTables>,
    unit_labels: Vec<String>,
    cur_uid: UnitId,
    has_std_gfx: bool,
    std_text_uid: Option<UnitId>,
    consts: BTreeMap<String, ConstDef>,
    enums: BTreeMap<String, EnumDef>,
    structs: BTreeMap<String, StructDef>,
    layouts: BTreeMap<String, LayoutDef>,
    layout_field_meta: BTreeMap<(String, String), LayoutFieldMeta>,
    layout_spans: Vec<(i64, i64, String, Span)>,
    auto_layout_cursor: i64,
    match_counter: usize,
    addr_counter: usize,
    loop_counter: usize,
    loop_stack: Vec<LoopCtrlCtx>,
    struct_vars: Vec<BTreeMap<String, String>>,
    fn_ret_struct: BTreeMap<String, String>,
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
    fn new(
        tables: Vec<UnitTables>,
        unit_labels: Vec<String>,
        entry_uid: UnitId,
        has_std_gfx: bool,
        std_text_uid: Option<UnitId>,
    ) -> Self {
        Self {
            tables,
            unit_labels,
            cur_uid: entry_uid,
            has_std_gfx,
            std_text_uid,
            consts: BTreeMap::new(),
            enums: BTreeMap::new(),
            structs: BTreeMap::new(),
            layouts: BTreeMap::new(),
            layout_field_meta: BTreeMap::new(),
            layout_spans: Vec::new(),
            auto_layout_cursor: 705_536,
            match_counter: 0,
            addr_counter: 0,
            loop_counter: 0,
            loop_stack: Vec::new(),
            struct_vars: Vec::new(),
            fn_ret_struct: BTreeMap::new(),
        }
    }

    fn lookup_namespace(&self, alias: &str) -> Option<UnitId> {
        self.tables[self.cur_uid]
            .scope
            .namespaces
            .get(alias)
            .map(|n| n.target_uid)
    }

    fn private_err(&self, target_uid: UnitId, name: &str, span: Span) -> InfimumDiagnostic {
        let lbl = &self.unit_labels[target_uid];
        InfimumDiagnostic::new(
            "E068_PRIVATE_ITEM",
            "resolve",
            format!("Symbol '{name}' in '{lbl}' is private; mark it 'pub' to export"),
            span,
            format!("Add 'pub' to '{name}' in '{lbl}'"),
        )
    }

    fn lookup_const_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.consts.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.consts.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.consts.get(name))
        }
    }

    fn lookup_enum_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.enums.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.enums.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.enums.get(name))
        }
    }

    fn lookup_struct_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.structs.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.structs.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.structs.get(name))
        }
    }

    fn lookup_layout_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.layouts.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.layouts.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.layouts.get(name))
        }
    }

    fn lookup_fn_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.fns.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.fns.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.fns.get(name))
        }
    }

    fn lookup_forma_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.formas.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.formas.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.formas.get(name))
        }
    }

    fn resolve_layout_canon(
        &self,
        module: Option<&str>,
        layout: &str,
        span: Span,
    ) -> Result<String, InfimumDiagnostic> {
        if let Some(sym) = self.lookup_layout_sym(module, layout, span)? {
            return Ok(sym.canonical_name.clone());
        }
        let disp = match module {
            Some(m) => format!("{m}::{layout}"),
            None => layout.to_string(),
        };
        Err(InfimumDiagnostic::new(
            "E061_UNKNOWN_LAYOUT_FIELD",
            "resolve",
            format!("Unknown layout '{disp}'"),
            span,
            format!("Declare 'layout {layout}' (or import it with 'use') before accessing it"),
        ))
    }

    fn resolve_struct_canon(
        &self,
        module: Option<&str>,
        name: &str,
        span: Span,
    ) -> Result<String, InfimumDiagnostic> {
        if let Some(sym) = self.lookup_struct_sym(module, name, span)? {
            return Ok(sym.canonical_name.clone());
        }
        let disp = match module {
            Some(m) => format!("{m}::{name}"),
            None => name.to_string(),
        };
        Err(InfimumDiagnostic::new(
            "E014_UNKNOWN_TYPE",
            "resolve",
            format!("Unknown struct type '{disp}'"),
            span,
            format!("Declare 'pub struct {name} {{ ... }}' or import it with 'use'"),
        ))
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
            RawType::Named { module, name, span } => {
                if let Some(sym) = self.lookup_struct_sym(module.as_deref(), name, *span)? {
                    if let Some(sdef) = self.structs.get(&sym.canonical_name) {
                        return Ok(self.struct_tuple_type(sdef));
                    }
                }
                let disp = match module {
                    Some(m) => format!("{m}::{name}"),
                    None => name.clone(),
                };
                Err(InfimumDiagnostic::new(
                    "E014_UNKNOWN_TYPE",
                    "parse",
                    format!("Unknown type '{disp}'"),
                    *span,
                    "Use a built-in type (i64, fixed, bool, str, unit, Secret<T>, (T1, T2)) or a visible 'struct' name",
                ))
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
            RawExpr::StructLit(sl) => self
                .lookup_struct_sym(sl.module.as_deref(), &sl.name, sl.span)
                .ok()
                .flatten()
                .map(|s| s.canonical_name.clone()),
            RawExpr::Var(v) => self.lookup_struct_var(v),
            RawExpr::Call {
                target: TargetRef::Named(fname),
                ..
            } => {
                let canon = self
                    .lookup_fn_sym(None, fname, Span::zero())
                    .ok()
                    .flatten()
                    .map(|s| s.canonical_name.as_str())
                    .unwrap_or(fname.as_str());
                self.fn_ret_struct.get(canon).cloned()
            }
            RawExpr::ModCall(mc) => {
                let sym = self
                    .lookup_fn_sym(Some(&mc.module), &mc.func, mc.span)
                    .ok()
                    .flatten()?;
                self.fn_ret_struct.get(&sym.canonical_name).cloned()
            }
            RawExpr::LayoutLoad(ll) if ll.subfield.is_none() => {
                let lcanon = self
                    .lookup_layout_sym(ll.module.as_deref(), &ll.layout, ll.span)
                    .ok()
                    .flatten()?
                    .canonical_name
                    .clone();
                let meta = self.layout_field_meta.get(&(lcanon, ll.field.clone()))?;
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
                if let Some(sym) = self.lookup_const_sym(None, name, span)? {
                    if let Some(c) = self.consts.get(&sym.canonical_name) {
                        return Ok(c.value);
                    }
                }
                Err(InfimumDiagnostic::new(
                    "E064_CONST_EVAL",
                    "const-eval",
                    format!("Unknown constant '{name}' in constant expression"),
                    span,
                    "Declare the constant with 'const NAME: i64 = ...;' (or import a 'pub const' with 'use') before referencing it",
                ))
            }
            RawExpr::Path { segs, span: pspan } => self.resolve_path_const(segs, *pspan),
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

    fn resolve_layout_prop_by_canon(
        &self,
        lcanon: &str,
        disp_layout: &str,
        member: &str,
        prop: &str,
        span: Span,
    ) -> Result<i64, InfimumDiagnostic> {
        let ldef = self.layouts.get(lcanon).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout '{disp_layout}' in '{disp_layout}::{member}::{prop}'"),
                span,
                format!("Declare 'layout {disp_layout}' before referencing its field properties"),
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
                    format!("Layout '{disp_layout}' has no field '{member}'"),
                    span,
                    format!("Check the fields declared in 'layout {disp_layout}'"),
                )
            })?;
        let meta = self
            .layout_field_meta
            .get(&(lcanon.to_string(), member.to_string()));
        match prop {
            "len" => Ok(field.len),
            "end" => Ok(field.offset + field.len),
            "stride" => Ok(meta.map(|m| m.stride).unwrap_or(1)),
            "count" => Ok(meta.map(|m| m.count).unwrap_or(field.len)),
            other => Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout field property '{other}' on '{disp_layout}::{member}'"),
                span,
                "Supported layout field properties are '::len', '::end', '::stride', and '::count'",
            )),
        }
    }

    fn resolve_member_in_scope(
        &self,
        module: Option<&str>,
        head: &str,
        member: &str,
        span: Span,
    ) -> Result<Option<ConstValue>, InfimumDiagnostic> {
        let disp = match module {
            Some(m) => format!("{m}::{head}"),
            None => head.to_string(),
        };
        if let Some(sym) = self.lookup_enum_sym(module, head, span)? {
            if let Some(edef) = self.enums.get(&sym.canonical_name) {
                if let Some(v) = edef.variants.iter().find(|v| v.name == member) {
                    return Ok(Some(ConstValue::Int(v.value)));
                }
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_ENUM_VARIANT",
                    "resolve",
                    format!("Enum '{disp}' has no variant '{member}'"),
                    span,
                    format!("Check the variants declared in 'enum {head}'"),
                ));
            }
        }
        if let Some(sym) = self.lookup_layout_sym(module, head, span)? {
            if let Some(ldef) = self.layouts.get(&sym.canonical_name) {
                if let Some(f) = ldef.fields.iter().find(|f| f.name == member) {
                    return Ok(Some(ConstValue::Int(f.offset)));
                }
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Layout '{disp}' has no field '{member}'"),
                    span,
                    format!("Check the fields declared in 'layout {head}'"),
                ));
            }
        }
        if let Some(sym) = self.lookup_struct_sym(module, head, span)? {
            if let Some(sdef) = self.structs.get(&sym.canonical_name) {
                if member == "SIZE" || member == "size" {
                    return Ok(Some(ConstValue::Int(sdef.size)));
                }
                if let Some(&off) = sdef.fields.get(member) {
                    return Ok(Some(ConstValue::Int(off)));
                }
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Struct '{disp}' has no field '{member}'"),
                    span,
                    format!(
                        "Available fields in 'struct {head}': {} (plus '::SIZE')",
                        sdef.field_names.join(", ")
                    ),
                ));
            }
        }
        Ok(None)
    }

    fn resolve_path_const(&self, segs: &[String], span: Span) -> Result<ConstValue, InfimumDiagnostic> {
        let full = segs.join("::");
        match segs {
            [head, member] => {
                // 1. Module constant: `m::CONST`
                if let Some(sym) = self.lookup_const_sym(Some(head), member, span)? {
                    if let Some(c) = self.consts.get(&sym.canonical_name) {
                        return Ok(c.value);
                    }
                }
                // 2. Local/imported Enum::Variant, Layout::field, Struct::SIZE/field
                if let Some(val) = self.resolve_member_in_scope(None, head, member, span)? {
                    return Ok(val);
                }
                Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_ENUM_VARIANT",
                    "resolve",
                    format!("Unknown module, enum, struct, or layout '{head}' in '{full}'"),
                    span,
                    format!("Check that '{head}' is declared or imported with 'use' and '{member}' is marked 'pub'"),
                ))
            }
            [a, b, c] => {
                // 1. Qualified module member: `m::Enum::Variant`, `m::Layout::field`, `m::Struct::SIZE`
                if self.lookup_namespace(a).is_some() {
                    if let Some(val) = self.resolve_member_in_scope(Some(a), b, c, span)? {
                        return Ok(val);
                    }
                    return Err(InfimumDiagnostic::new(
                        "E061_UNKNOWN_ENUM_VARIANT",
                        "resolve",
                        format!("Unknown exported enum, struct, or layout '{b}' in module '{a}' for '{full}'"),
                        span,
                        format!("Ensure '{b}' is marked 'pub' in module '{a}'"),
                    ));
                }
                // 2. Local/imported layout field property: `Layout::field::len/end/stride/count`
                let lcanon = self.resolve_layout_canon(None, a, span)?;
                Ok(ConstValue::Int(
                    self.resolve_layout_prop_by_canon(&lcanon, a, b, c, span)?,
                ))
            }
            [m, layout, field, prop] => {
                let lcanon = self.resolve_layout_canon(Some(m), layout, span)?;
                let disp = format!("{m}::{layout}");
                Ok(ConstValue::Int(
                    self.resolve_layout_prop_by_canon(&lcanon, &disp, field, prop, span)?,
                ))
            }
            _ => Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_ENUM_VARIANT",
                "resolve",
                format!("Unsupported path '{full}'"),
                span,
                "Use up to 4 path segments (e.g. m::CONST, m::Enum::Var, m::Layout::field::count)",
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

    fn resolve_std_text_fn(&self, name: &str, span: Span) -> Result<String, InfimumDiagnostic> {
        if let Some(tid) = self.std_text_uid {
            if let Some(sym) = self.tables[tid].exports.fns.get(name) {
                return Ok(sym.canonical_name.clone());
            }
        }
        if let Some(sym) = self.lookup_fn_sym(None, name, span)? {
            return Ok(sym.canonical_name.clone());
        }
        Err(InfimumDiagnostic::new(
            "E064_CONST_EVAL",
            "resolve",
            format!("ui_fmt requires '{name}' from std/text.ifm"),
            span,
            "Import 'use \"std/text.ifm\";' or 'use \"std/prelude.ifm\";'",
        ))
    }

    fn resolve_clamp_i64_name(&self) -> String {
        if let Some(sym) = self.tables[self.cur_uid].scope.fns.get("clamp_i64") {
            return sym.canonical_name.clone();
        }
        for t in &self.tables {
            if let Some(sym) = t.exports.fns.get("clamp_i64") {
                return sym.canonical_name.clone();
            }
        }
        "clamp_i64".to_string()
    }

    fn register_item(&mut self, item: RawItem, owner_u: &RawUnit) -> Result<(), InfimumDiagnostic> {
        match item {
            RawItem::Const(rc) => {
                let cname = unit_canonical_name(owner_u, &rc.name, false);
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
                self.consts.insert(cname, def);
            }
            RawItem::Enum(re) => {
                let cname = unit_canonical_name(owner_u, &re.name, false);
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
                self.enums.insert(cname, def);
            }
            RawItem::Struct(rs) => {
                let cname = unit_canonical_name(owner_u, &rs.name, false);
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
                    cname,
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
                let cname = unit_canonical_name(owner_u, &rl.name, false);
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
                    let (resolved_struct_type, stride) = match &rf.struct_type {
                        Some(sname) => {
                            let scanon = self.resolve_struct_canon(
                                rf.struct_module.as_deref(),
                                sname,
                                rf.span,
                            )?;
                            let sdef = self.structs.get(&scanon).ok_or_else(|| {
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
                            (Some(scanon), sdef.size)
                        }
                        None => (None, 1),
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
                        (cname.clone(), rf.name.clone()),
                        LayoutFieldMeta {
                            offset,
                            is_array: rf.is_array,
                            is_fixed: rf.is_fixed,
                            struct_type: resolved_struct_type,
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
                self.layouts.insert(cname, def);
            }
            RawItem::Mod(_) => {}
        }
        Ok(())
    }

    fn verify_no_layout_overlaps(&self) -> Result<(), InfimumDiagnostic> {
        // Protect std/gfx reserved runtime words (11 = cursor_kind, 12 = prim_count) in GPU apps
        if self.has_std_gfx {
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
        let mut params = Vec::with_capacity(raw.params.len());
        let mut init_struct_vars = BTreeMap::new();
        for (pname, raw_ty) in &raw.params {
            let resolved_ty = self.resolve_raw_type(raw_ty)?;
            if let RawType::Named { module, name, span } = raw_ty {
                let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
                init_struct_vars.insert(pname.clone(), scanon);
            }
            params.push(Param {
                name: pname.clone(),
                ty: resolved_ty,
            });
        }
        let ret_type = self.resolve_raw_type(&raw.ret_type)?;
        if let RawType::Named { module, name, span } = &raw.ret_type {
            let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
            init_struct_vars.insert("result".to_string(), scanon);
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
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_text", Span::zero())?),
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
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_fixed", Span::zero())?),
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
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_int", Span::zero())?),
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
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_text_width", Span::zero())?),
                    args: vec![Expr::StrLit(s), Expr::Var(vsz.clone()), Expr::Var(vst.clone())],
                },
                Seg::Expr(e, Some(prec)) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_fixed_width", Span::zero())?),
                    args: vec![
                        self.lower_expr(&e, locals)?,
                        Expr::IntLit(prec),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                    ],
                },
                Seg::Expr(e, None) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_int_width", Span::zero())?),
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
                        Some(RawType::Named { module, name, span }) => {
                            Some(self.resolve_struct_canon(module.as_deref(), name, *span)?)
                        }
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
                    module,
                    layout,
                    field,
                    index,
                    subfield,
                    op,
                    value,
                    span,
                } => {
                    // Case A: Local value-struct field mutation (`v.x = e;` / `v.x += e;`)
                    if module.is_none() && index.is_none() && subfield.is_none() {
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
                    let lcanon = self.resolve_layout_canon(module.as_deref(), layout, *span)?;
                    // Case B: Whole-struct store into layout (`World.hero = v;` or `World.flock[i] = s;`)
                    if subfield.is_none() && op.is_none() {
                        if let Some(meta) = self
                            .layout_field_meta
                            .get(&(lcanon.clone(), field.clone()))
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
                                        target: TargetRef::Named(self.resolve_clamp_i64_name()),
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
                        &lcanon,
                        field,
                        index.as_ref(),
                        subfield.as_deref(),
                        locals,
                        *span,
                    )?;
                    let field_inv = self.lookup_layout_inv(&lcanon, field, subfield.as_deref());
                    let field_fixed = self.lookup_layout_fixed(&lcanon, field, subfield.as_deref());
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
                    if let Some(sym) = self.lookup_const_sym(None, name, Span::zero())? {
                        if let Some(cdef) = self.consts.get(&sym.canonical_name) {
                            return Ok(match cdef.value {
                                ConstValue::Int(v) => Expr::IntLit(v),
                                ConstValue::Fixed(v) => Expr::FixedLit(v),
                                ConstValue::Bool(b) => Expr::BoolLit(b),
                            });
                        }
                    }
                }
                Ok(Expr::Var(name.clone()))
            }
            RawExpr::Path { segs, span } => {
                let v = self.resolve_path_const(segs, *span)?;
                Ok(match v {
                    ConstValue::Int(n) => Expr::IntLit(n),
                    ConstValue::Fixed(n) => Expr::FixedLit(n),
                    ConstValue::Bool(b) => Expr::BoolLit(b),
                })
            }
            RawExpr::LayoutLoad(ll) => {
                let RawLayoutLoad {
                    module,
                    layout,
                    field,
                    index,
                    subfield,
                    span,
                } = ll.as_ref();
                // Case A: Local value-struct field read (`v.x`)
                if module.is_none() && index.is_none() && subfield.is_none() {
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
                let lcanon = self.resolve_layout_canon(module.as_deref(), layout, *span)?;
                // Case B: Whole-struct load from layout (`let h = World.hero;` or `let s = World.flock[i];`)
                if subfield.is_none() {
                    if let Some(meta) = self
                        .layout_field_meta
                        .get(&(lcanon.clone(), field.clone()))
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
                                    target: TargetRef::Named(self.resolve_clamp_i64_name()),
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
                    &lcanon,
                    field,
                    index.as_deref(),
                    subfield.as_deref(),
                    locals,
                    *span,
                )?;
                let field_inv = self.lookup_layout_inv(&lcanon, field, subfield.as_deref());
                let field_fixed = self.lookup_layout_fixed(&lcanon, field, subfield.as_deref());
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
            } => {
                // Flatten `if ... else if ... else ...` iteratively so long chains never overflow the stack
                let mut branches: Vec<(Expr, Expr)> = Vec::new();
                let mut cur_cond = cond.as_ref();
                let mut cur_then = then_branch.as_ref();
                let mut cur_else = else_branch.as_ref();
                loop {
                    let lc = self.lower_expr(cur_cond, locals)?;
                    let lt = self.lower_expr(cur_then, locals)?;
                    branches.push((lc, lt));
                    if let RawExpr::If {
                        cond: nc,
                        then_branch: nt,
                        else_branch: ne,
                    } = cur_else
                    {
                        cur_cond = nc.as_ref();
                        cur_then = nt.as_ref();
                        cur_else = ne.as_ref();
                    } else {
                        break;
                    }
                }
                let mut acc = self.lower_expr(cur_else, locals)?;
                for (lc, lt) in branches.into_iter().rev() {
                    acc = Expr::If {
                        cond: Box::new(lc),
                        then_branch: Box::new(lt),
                        else_branch: Box::new(acc),
                    };
                }
                Ok(acc)
            }
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
                        if let Some(sym) = self.lookup_fn_sym(None, fname, Span::zero())? {
                            TargetRef::Named(sym.canonical_name.clone())
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
            RawExpr::ModCall(mc) => {
                let RawModCall {
                    module,
                    func,
                    args,
                    span,
                } = mc.as_ref();
                let Some(sym) = self.lookup_fn_sym(Some(module), func, *span)? else {
                    if self.lookup_namespace(module).is_none() {
                        return Err(InfimumDiagnostic::new(
                            "E065_UNKNOWN_MOD_FN",
                            "resolve",
                            format!("Unknown module namespace '{module}' in '{module}::{func}'"),
                            *span,
                            format!("Import the module with 'use \"file.ifm\" as {module};'"),
                        ));
                    }
                    return Err(InfimumDiagnostic::new(
                        "E065_UNKNOWN_MOD_FN",
                        "resolve",
                        format!("Unknown function '{module}::{func}'"),
                        *span,
                        format!("Declare 'pub fn {func}(...)' in module '{module}'"),
                    ));
                };
                let target_name = sym.canonical_name.clone();
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                Ok(Expr::Call {
                    target: TargetRef::Named(target_name),
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
            RawExpr::StructLit(sl) => {
                let RawStructLit {
                    module,
                    name,
                    fields,
                    span,
                } = sl.as_ref();
                let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
                let sdef = self.structs.get(&scanon).cloned().ok_or_else(|| {
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
                        let sym = self.lookup_const_sym(None, name, *pspan)?;
                        let cdef = sym.and_then(|s| self.consts.get(&s.canonical_name)).ok_or_else(|| {
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
                    RawMatchPattern::Path(segs, pspan) => {
                        let enum_hit: Option<(String, String, i64)> = match segs.as_slice() {
                            [head, member] => {
                                if let Some(sym) = self.lookup_enum_sym(None, head, *pspan)? {
                                    if let Some(edef) = self.enums.get(&sym.canonical_name) {
                                        let v = edef.variants.iter().find(|v| v.name == *member).ok_or_else(|| {
                                            InfimumDiagnostic::new(
                                                "E061_UNKNOWN_ENUM_VARIANT",
                                                "resolve",
                                                format!("Enum '{head}' has no variant '{member}'"),
                                                *pspan,
                                                format!("Check the variants declared in 'enum {head}'"),
                                            )
                                        })?;
                                        Some((sym.canonical_name.clone(), member.clone(), v.value))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            [m, ename, member] if self.lookup_namespace(m).is_some() => {
                                if let Some(sym) = self.lookup_enum_sym(Some(m), ename, *pspan)? {
                                    if let Some(edef) = self.enums.get(&sym.canonical_name) {
                                        let v = edef.variants.iter().find(|v| v.name == *member).ok_or_else(|| {
                                            InfimumDiagnostic::new(
                                                "E061_UNKNOWN_ENUM_VARIANT",
                                                "resolve",
                                                format!("Enum '{m}::{ename}' has no variant '{member}'"),
                                                *pspan,
                                                format!("Check the variants declared in 'enum {ename}'"),
                                            )
                                        })?;
                                        Some((sym.canonical_name.clone(), member.clone(), v.value))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some((enum_name, variant, value)) = enum_hit {
                            ResolvedPat::Enum { enum_name, variant, value }
                        } else {
                            match self.resolve_path_const(segs, *pspan)? {
                                ConstValue::Int(v) | ConstValue::Fixed(v) => ResolvedPat::Int(v),
                                ConstValue::Bool(b) => ResolvedPat::Bool(b),
                            }
                        }
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

fn rewrite_forma_slot_callbacks<E>(
    node: &mut crate::forma::FormaNode<E>,
    resolver: &ModuleResolver,
) -> Result<(), InfimumDiagnostic> {
    let resolve_cb = |raw: &str, span: Span| -> Result<String, InfimumDiagnostic> {
        if let Some((m, f)) = raw.split_once("::") {
            let Some(sym) = resolver.lookup_fn_sym(Some(m), f, span)? else {
                return Err(InfimumDiagnostic::new(
                    "E065_UNKNOWN_MOD_FN",
                    "resolve",
                    format!("Unknown callback function '{raw}' in Forma Slot"),
                    span,
                    format!("Ensure module '{m}' declares 'pub fn {f}'"),
                ));
            };
            Ok(sym.canonical_name.clone())
        } else if let Some(sym) = resolver.lookup_fn_sym(None, raw, span)? {
            Ok(sym.canonical_name.clone())
        } else {
            Ok(raw.to_string())
        }
    };
    if let Some(ref mut rfn) = node.props.render {
        *rfn = resolve_cb(rfn, node.span)?;
    }
    if let Some(ref mut wfn) = node.props.on_wheel {
        *wfn = resolve_cb(wfn, node.span)?;
    }
    for ch in &mut node.children {
        rewrite_forma_slot_callbacks(ch, resolver)?;
    }
    if let Some((_, ref mut else_nodes)) = node.forma_if {
        for ch in else_nodes {
            rewrite_forma_slot_callbacks(ch, resolver)?;
        }
    }
    Ok(())
}

fn resolve_and_lower(mut graph: UnitGraph) -> Result<Module, InfimumDiagnostic> {
    let tables = build_unit_tables(&graph)?;
    let unit_labels: Vec<String> = graph.units.iter().map(|u| u.display_label.clone()).collect();
    let entry_uid = graph.entry_uid;
    let has_std_gfx = graph.find_unit("__std__/gfx.ifm").is_some()
        || tables[entry_uid].scope.consts.contains_key("GPU_CMD_BYTE_OFFSET");
    let std_text_uid = graph.find_unit("__std__/text.ifm");
    let std_mesh3d_uid = graph.find_unit("__std__/mesh3d.ifm");

    let mut resolver = ModuleResolver::new(tables, unit_labels, entry_uid, has_std_gfx, std_text_uid);

    // 1. Register all consts, enums, structs, and layouts in topological order
    let topo = graph.topo_order.clone();
    for &uid in &topo {
        resolver.cur_uid = uid;
        let items = std::mem::take(&mut graph.units[uid].items);
        let owner_u = &graph.units[uid];
        for item in items {
            resolver.register_item(item, owner_u)?;
        }
    }
    resolver.verify_no_layout_overlaps()?;

    // 2. Collect all forma definitions keyed by canonical name (paired with their owning UnitId)
    let mut all_raw_formas: Vec<(UnitId, String, crate::forma::FormaDef<RawExpr>)> = Vec::new();
    let mut forma_defs_by_canon: BTreeMap<String, (UnitId, crate::forma::FormaDef<RawExpr>)> =
        BTreeMap::new();
    for &uid in &topo {
        let u = &mut graph.units[uid];
        let formas_u = std::mem::take(&mut u.formas);
        for mut rf in formas_u {
            let cname = unit_canonical_name(u, &rf.name, rf.exported);
            rf.name = cname.clone();
            forma_defs_by_canon.insert(cname.clone(), (uid, rf.clone()));
            all_raw_formas.push((uid, cname, rf));
        }
    }

    // 3. Expand and lower all forma definitions in their own UnitId scope
    let mut formas = Vec::with_capacity(all_raw_formas.len());
    for (uid, cname, mut rf) in all_raw_formas {
        resolver.cur_uid = uid;
        let mut visiting = vec![cname];
        let expanded_roots =
            expand_forma_node(rf.root, &forma_defs_by_canon, &mut resolver, &mut visiting)?;
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
        resolver.cur_uid = uid;
        rewrite_forma_slot_callbacks(&mut rf.root, &resolver)?;
        let param_set: BTreeSet<String> = rf.params.iter().map(|p| p.name.clone()).collect();
        let mut param_locals = vec![param_set];
        let resolved = rf.try_map(&mut |e| resolver.lower_expr(&e, &mut param_locals))?;
        formas.push(resolved);
    }

    // 4. Pre-register function return structs by canonical name
    let mut all_raw_fns: Vec<(UnitId, RawFnDef)> = Vec::new();
    for &uid in &topo {
        let u = &mut graph.units[uid];
        let fns_u = std::mem::take(&mut u.functions);
        resolver.cur_uid = uid;
        for mut rf in fns_u {
            let cname = unit_canonical_name(u, &rf.name, rf.exported);
            rf.name = cname.clone();
            if let RawType::Named { module, name, span } = &rf.ret_type {
                let scanon = resolver.resolve_struct_canon(module.as_deref(), name, *span)?;
                resolver.fn_ret_struct.insert(cname, scanon);
            }
            all_raw_fns.push((uid, rf));
        }
    }

    let mut functions = Vec::with_capacity(
        all_raw_fns.len() + formas.len() + if formas.is_empty() { 0 } else { 5 },
    );
    if !formas.is_empty() {
        let cp_atlas_fn = std_text_uid
            .and_then(|tid| resolver.tables[tid].exports.fns.get("codepoint_to_atlas"))
            .map(|s| s.canonical_name.clone())
            .unwrap_or_else(|| "codepoint_to_atlas".to_string());
        let glyph_px_fn = std_text_uid
            .and_then(|tid| resolver.tables[tid].exports.fns.get("glyph_px"))
            .map(|s| s.canonical_name.clone())
            .unwrap_or_else(|| "glyph_px".to_string());
        let existing_names: HashSet<&str> =
            all_raw_fns.iter().map(|(_, rf)| rf.name.as_str()).collect();
        for h in crate::forma::forma_runtime_helpers_with_text(&cp_atlas_fn, &glyph_px_fn) {
            if !existing_names.contains(h.name.as_str()) {
                functions.push(h);
            }
        }
    }

    let mut tests = Vec::new();
    for (uid, rf) in all_raw_fns {
        resolver.cur_uid = uid;
        if let Some(label) = rf.test_label.clone() {
            tests.push((rf.name.clone(), label));
        }
        functions.push(resolver.lower_fn(rf)?);
    }
    for forma in &formas {
        functions.push(crate::forma::lower_forma_def(forma));
    }

    // 5. Auto-generate standard .ifw GPU runtime exports when std/gfx is loaded and step_ui is exported
    if has_std_gfx && functions.iter().any(|f| f.exported && f.name == "step_ui") {
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
            let m3d_canon = std_mesh3d_uid
                .and_then(|uid| resolver.tables[uid].exports.layouts.get("Mesh3dMem"))
                .map(|s| s.canonical_name.as_str());
            if let Some(m3d) = m3d_canon.and_then(|c| resolver.layouts.get(c)) {
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

    // 6. Export clean ABI surface from entry unit's public interface (or entry scope if no `pub` items)
    let entry_exports = &resolver.tables[entry_uid].exports;
    let entry_scope = &resolver.tables[entry_uid].scope;
    let use_exports = !entry_exports.consts_order.is_empty()
        || !entry_exports.enums_order.is_empty()
        || !entry_exports.layouts_order.is_empty();
    let abi_src = if use_exports { entry_exports } else { entry_scope };

    let mut consts = Vec::new();
    for k in &abi_src.consts_order {
        if let Some(sym) = abi_src.consts.get(k) {
            if let Some(mut cdef) = resolver.consts.get(&sym.canonical_name).cloned() {
                cdef.name = k.clone();
                consts.push(cdef);
            }
        }
    }
    let mut enums = Vec::new();
    for k in &abi_src.enums_order {
        if let Some(sym) = abi_src.enums.get(k) {
            if let Some(mut edef) = resolver.enums.get(&sym.canonical_name).cloned() {
                edef.name = k.clone();
                enums.push(edef);
            }
        }
    }
    let mut layouts = Vec::new();
    for k in &abi_src.layouts_order {
        if let Some(sym) = abi_src.layouts.get(k) {
            if let Some(mut ldef) = resolver.layouts.get(&sym.canonical_name).cloned() {
                ldef.name = k.clone();
                layouts.push(ldef);
            }
        }
    }

    Ok(Module {
        consts,
        enums,
        layouts,
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
            if let TargetRef::Named(n) = target {
                if (n == "ui_int" || n.ends_with("__ui_int"))
                    && args.len() == 7
                    && arg_types.get(2) == Some(&Type::Fixed)
                {
                    let rewritten = format!("{}ui_fixed", &n[..n.len() - "ui_int".len()]);
                    *target = TargetRef::Named(rewritten);
                    args.insert(3, Expr::IntLit(2));
                } else if (n == "ui_int_width" || n.ends_with("__ui_int_width"))
                    && args.len() == 3
                    && arg_types.first() == Some(&Type::Fixed)
                {
                    let rewritten = format!("{}ui_fixed_width", &n[..n.len() - "ui_int_width".len()]);
                    *target = TargetRef::Named(rewritten);
                    args.insert(1, Expr::IntLit(2));
                }
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
        RawExpr::ModCall(mc) => RawExpr::ModCall(Box::new(RawModCall {
            module: mc.module,
            func: mc.func,
            args: mc.args.into_iter().map(|a| subst_raw_expr(a, env)).collect(),
            span: mc.span,
        })),
        RawExpr::LayoutLoad(ll) => RawExpr::LayoutLoad(Box::new(RawLayoutLoad {
            module: ll.module,
            layout: ll.layout,
            field: ll.field,
            index: ll.index.map(|i| Box::new(subst_raw_expr(*i, env))),
            subfield: ll.subfield,
            span: ll.span,
        })),
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
        RawExpr::StructLit(sl) => RawExpr::StructLit(Box::new(RawStructLit {
            module: sl.module,
            name: sl.name,
            fields: sl
                .fields
                .into_iter()
                .map(|(k, v, sp)| (k, subst_raw_expr(v, env), sp))
                .collect(),
            span: sl.span,
        })),
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
    defs: &BTreeMap<String, (UnitId, crate::forma::FormaDef<RawExpr>)>,
    resolver: &mut ModuleResolver,
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

    // 3. Nested custom `forma` component call: `ScoreCard("Вы", score)` or `m::ScoreCard(...)`
    if let Some((comp_name, args)) = node.component_call.take() {
        let resolved_sym = if let Some((m, fname)) = comp_name.split_once("::") {
            resolver.lookup_forma_sym(Some(m), fname, node.span)?
        } else {
            resolver.lookup_forma_sym(None, &comp_name, node.span)?
        };
        let target_entry = resolved_sym
            .and_then(|s| defs.get(&s.canonical_name).map(|e| (s.canonical_name.clone(), e)));
        let Some((target_canon, (target_uid, target_def))) = target_entry else {
            return Err(InfimumDiagnostic::new(
                "E091_FORMA_SYNTAX",
                "forma-expand",
                format!("Unknown Forma node or component '{comp_name}'"),
                node.span,
                "Use a built-in node (Box, Col, Row, Stack, Scroll, Overlay, Button, Dropdown, Spacer, Divider, Rect, Slot, Text) or declare 'forma Name(...)'",
            ));
        };
        if visiting.iter().any(|v| v == &target_canon) {
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
        let instantiated = subst_forma_node(target_def.root.clone(), &env);
        let prev_uid = resolver.cur_uid;
        resolver.cur_uid = *target_uid;
        visiting.push(target_canon);
        let expanded = expand_forma_node(instantiated, defs, resolver, visiting)?;
        visiting.pop();
        resolver.cur_uid = prev_uid;
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

        // Calling private function externally must fail with E068_PRIVATE_ITEM
        let bad_fn = r#"
mod m {
  fn hidden_fn() -> i64 { 42 }
}
fn main() -> i64 { m::hidden_fn() }
"#;
        let err = parse_module(bad_fn).expect_err("calling private mod fn must fail");
        assert_eq!(err.code, "E068_PRIVATE_ITEM");

        // Reading private constant externally must fail with E068_PRIVATE_ITEM
        let bad_const = r#"
mod m {
  const SECRET: i64 = 42;
}
fn main() -> i64 { m::SECRET }
"#;
        let err2 = parse_module(bad_const).expect_err("reading private mod const must fail");
        assert_eq!(err2.code, "E068_PRIVATE_ITEM");

        // File-based `use "ai.ifm" as ai;` in-memory (Online Studio)
        let mut proj = BTreeMap::new();
        proj.insert(
            "ai.ifm".to_string(),
            "const BONUS: i64 = 7;\nfn hidden(x: i64) -> i64 { x + BONUS }\npub fn pick(x: i64) -> i64 { hidden(x) * 2 }\n".to_string(),
        );
        proj.insert(
            "main.ifm".to_string(),
            "use \"ai.ifm\" as ai;\nexport fn play(x: i64) -> i64 { ai::pick(x) }\n".to_string(),
        );
        let (wasm2, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &proj)
            .expect("file-based use ... as should compile in memory");
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
