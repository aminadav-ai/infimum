// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::diag::{InfimumDiagnostic, Span};

use super::raw_ast::{RawFile, RawItem, RawMod, RawUnit, UnitGraph, UnitId};
use super::syntax::parse_raw_file;

pub(super) fn resolve_embedded_std(spec: &str) -> Option<(&'static str, &'static str)> {
    match spec {
        "std/math" | "std/math.ifm" => Some(("__std__/math.ifm", include_str!("../../std/math.ifm"))),
        "std/dec" | "std/dec.ifm" => Some(("__std__/dec.ifm", include_str!("../../std/dec.ifm"))),
        "std/color" | "std/color.ifm" => Some(("__std__/color.ifm", include_str!("../../std/color.ifm"))),
        "std/sys_mem" | "std/sys_mem.ifm" => {
            Some(("__std__/sys_mem.ifm", include_str!("../../std/sys_mem.ifm")))
        }
        "std/host_abi" | "std/host_abi.ifm" => {
            Some(("__std__/host_abi.ifm", include_str!("../../std/host_abi.ifm")))
        }
        "std/mem" | "std/mem.ifm" => Some(("__std__/mem.ifm", include_str!("../../std/mem.ifm"))),
        "std/gfx" | "std/gfx.ifm" => Some(("__std__/gfx.ifm", include_str!("../../std/gfx.ifm"))),
        "std/mesh3d" | "std/mesh3d.ifm" => Some(("__std__/mesh3d.ifm", include_str!("../../std/mesh3d.ifm"))),
        "std/text" | "std/text.ifm" => Some(("__std__/text.ifm", include_str!("../../std/text.ifm"))),
        "std/audio" | "std/audio.ifm" => Some(("__std__/audio.ifm", include_str!("../../std/audio.ifm"))),
        "std/input" | "std/input.ifm" => Some(("__std__/input.ifm", include_str!("../../std/input.ifm"))),
        "std/fmt" | "std/fmt.ifm" => Some(("__std__/fmt.ifm", include_str!("../../std/fmt.ifm"))),
        "std/theme" | "std/theme.ifm" => Some(("__std__/theme.ifm", include_str!("../../std/theme.ifm"))),
        "std/icons" | "std/icons.ifm" => Some(("__std__/icons.ifm", include_str!("../../std/icons.ifm"))),
        "std/ui" | "std/ui.ifm" => Some(("__std__/ui.ifm", include_str!("../../std/ui.ifm"))),
        "std/text_edit" | "std/text_edit.ifm" => {
            Some(("__std__/text_edit.ifm", include_str!("../../std/text_edit.ifm")))
        }
        "std/markdown" | "std/markdown.ifm" => {
            Some(("__std__/markdown.ifm", include_str!("../../std/markdown.ifm")))
        }
        "std/buf" | "std/buf.ifm" => Some(("__std__/buf.ifm", include_str!("../../std/buf.ifm"))),
        "std/slice" | "std/slice.ifm" => {
            Some(("__std__/slice.ifm", include_str!("../../std/slice.ifm")))
        }
        "std/utf8" | "std/utf8.ifm" => {
            Some(("__std__/utf8.ifm", include_str!("../../std/utf8.ifm")))
        }
        "std/json" | "std/json.ifm" => Some(("__std__/json.ifm", include_str!("../../std/json.ifm"))),
        "std/prelude" | "std/prelude.ifm" => {
            Some(("__std__/prelude.ifm", include_str!("../../std/prelude.ifm")))
        }
        _ => None,
    }
}

pub(super) fn tag_raw_items_source(items: &mut [RawItem], label: &str) {
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

pub(super) fn tag_raw_file_source(raw: &mut RawFile, label: &str) {
    for f in &mut raw.functions {
        if f.source_file.is_none() {
            f.source_file = Some(label.to_string());
        }
    }
    tag_raw_items_source(&mut raw.items, label);
}

pub(super) fn diag_with_file(diag: InfimumDiagnostic, file_label: &str) -> InfimumDiagnostic {
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


pub(super) trait FileSource {
    /// Resolve a `use "<spec>"` relative to `requester_key` into `(canonical_key, display_label, source_text)`.
    fn resolve_use(
        &self,
        requester_key: &str,
        spec: &str,
        use_span: Span,
    ) -> Result<(String, String, String), InfimumDiagnostic>;
}

pub(super) struct MemorySource;

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

pub(super) struct DiskSource {
    pub(super) entry_dir: Option<PathBuf>,
}

impl DiskSource {
    pub(super) fn display_for(&self, canon: &Path) -> String {
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

pub(super) fn normalize_virtual_rel(base_dir: &str, rel: &str) -> String {
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

pub(super) fn virtual_parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

pub(super) struct VirtualFsSource<'a> {
    pub(super) files: &'a BTreeMap<String, String>,
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

pub(super) struct UnitLoader<'a, S: FileSource> {
    pub(super) source: &'a S,
    pub(super) units: Vec<RawUnit>,
    pub(super) by_key: BTreeMap<String, UnitId>,
    pub(super) visiting: Vec<(String, String)>, // (canonical_key, display_label)
    pub(super) topo_order: Vec<UnitId>,
}

impl<'a, S: FileSource> UnitLoader<'a, S> {
    pub(super) fn new(source: &'a S) -> Self {
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
                include_str!("../forma/widgets_rt.ifm").to_string(),
            ));
        }
        if spec.starts_with("std/") {
            let Some((virtual_key, text)) = resolve_embedded_std(spec) else {
                return Err(InfimumDiagnostic::new(
                    "E040_FILE_READ",
                    "parse",
                    format!("Unknown built-in Infimum stdlib module '{spec}'"),
                    use_span,
                    "Available stdlib modules: std/prelude.ifm, std/sys_mem.ifm, std/mem.ifm, std/math.ifm, std/color.ifm, std/gfx.ifm, std/mesh3d.ifm, std/input.ifm, std/text.ifm, std/audio.ifm, std/fmt.ifm, std/theme.ifm, std/icons.ifm, std/ui.ifm, std/text_edit.ifm, std/markdown.ifm, std/buf.ifm, std/slice.ifm, std/json.ifm",
                ));
            };
            let label = format!("std/{}", virtual_key.trim_start_matches("__std__/"));
            return Ok((virtual_key.to_string(), label, text.to_string()));
        }
        self.source.resolve_use(requester_key, spec, use_span)
    }

    pub(super) fn load_unit(
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

    pub(super) fn finish(mut self, entry_uid: UnitId) -> Result<UnitGraph, InfimumDiagnostic> {
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

