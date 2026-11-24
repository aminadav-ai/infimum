use crate::ast::{EffectCap, Expr, FnDef, Module, Stmt, TargetRef, Type};
use crate::canon::{canonicalize_fn, CanonicalNode};
use crate::diag::{InfimumDiagnostic, Span};
use crate::typecheck::{typecheck_node, DirectSecretSink};
use crate::verify::{
    compute_forma_proof_key, compute_proof_key, verify_node, z3_version_id, VerificationReport,
    PROOF_CACHE_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestEntry {
    pub alias: String,
    pub hash: String,
    pub short_hash: String,
    pub deduplicated: bool,
    pub obligations_proved: usize,
    pub verify_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub node: CanonicalNode,
    pub aliases: Vec<String>,
    pub neighbor_signatures: Vec<NeighborSig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborSig {
    pub hash: String,
    pub short_hash: String,
    pub aliases: Vec<String>,
    pub params: Vec<String>,
    pub effects: Vec<String>,
    pub ret_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityGrant {
    pub effect: String,
    pub witness_chain: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretPipe {
    pub secret_type: String,
    pub destination_domain: String,
    pub operation: String,
    pub witness_chain: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityPassport {
    pub root_hash: String,
    pub short_hash: String,
    pub aliases: Vec<String>,
    pub total_reachable_nodes: usize,
    pub is_pure: bool,
    pub uses_secret_types: bool,
    pub capabilities: Vec<CapabilityGrant>,
    pub secret_pipes: Vec<SecretPipe>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSummaryNode {
    pub hash: String,
    pub short_hash: String,
    pub aliases: Vec<String>,
    pub direct_deps: Vec<String>,
    pub direct_effects: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProofCacheEntry {
    obligations_checked: usize,
    node_hash: String,
    z3_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProofCacheFile {
    schema_version: String,
    entries: BTreeMap<String, ProofCacheEntry>,
}

impl Default for ProofCacheFile {
    fn default() -> Self {
        Self {
            schema_version: PROOF_CACHE_SCHEMA_VERSION.to_string(),
            entries: BTreeMap::new(),
        }
    }
}

pub struct GraphStore {
    nodes_dir: PathBuf,
    aliases_file: PathBuf,
    proof_cache_file: PathBuf,
    aliases: BTreeMap<String, String>,
    proof_cache: Mutex<ProofCacheFile>,
}

impl GraphStore {
    pub fn open(workspace_root: &Path) -> Result<Self, InfimumDiagnostic> {
        let infimum_dir = workspace_root.join(".infimum");
        let nodes_dir = infimum_dir.join("nodes");
        let aliases_file = infimum_dir.join("aliases.json");
        let proof_cache_file = infimum_dir.join("proof_cache.json");

        fs::create_dir_all(&nodes_dir).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to initialize .infimum/nodes directory: {e}"),
                Span::zero(),
                "Check write permissions on workspace directory",
            )
        })?;

        let aliases = if aliases_file.is_file() {
            let content = fs::read_to_string(&aliases_file).map_err(|e| {
                InfimumDiagnostic::new(
                    "E030_STORE_IO",
                    "store",
                    format!("Failed to read aliases.json: {e}"),
                    Span::zero(),
                    "Ensure .infimum/aliases.json is readable UTF-8 JSON",
                )
            })?;
            serde_json::from_str(&content).map_err(|e| {
                InfimumDiagnostic::new(
                    "E031_STORE_CORRUPT",
                    "store",
                    format!("Corrupted aliases.json: {e}"),
                    Span::zero(),
                    "Repair or recreate .infimum/aliases.json",
                )
            })?
        } else {
            BTreeMap::new()
        };

        let proof_cache = if proof_cache_file.is_file() {
            fs::read_to_string(&proof_cache_file)
                .ok()
                .and_then(|raw| serde_json::from_str::<ProofCacheFile>(&raw).ok())
                .filter(|c| c.schema_version == PROOF_CACHE_SCHEMA_VERSION)
                .unwrap_or_default()
        } else {
            ProofCacheFile::default()
        };

        Ok(Self {
            nodes_dir,
            aliases_file,
            proof_cache_file,
            aliases,
            proof_cache: Mutex::new(proof_cache),
        })
    }

    fn save_proof_cache(&self) -> Result<(), InfimumDiagnostic> {
        let snapshot = self
            .proof_cache
            .lock()
            .map_err(|_| {
                InfimumDiagnostic::new(
                    "E030_STORE_IO",
                    "store",
                    "Proof cache mutex poisoned",
                    Span::zero(),
                    "Restart the verification process",
                )
            })?
            .clone();

        let json = serde_json::to_string_pretty(&snapshot).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to serialize proof_cache.json: {e}"),
                Span::zero(),
                "Internal serialization error",
            )
        })?;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_name = format!("proof_cache.json.tmp.{}.{nanos}", std::process::id());
        let tmp_path = self
            .proof_cache_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(tmp_name);

        fs::write(&tmp_path, json).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to write temporary proof cache file: {e}"),
                Span::zero(),
                "Check disk space and permissions in .infimum/",
            )
        })?;

        fs::rename(&tmp_path, &self.proof_cache_file).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to atomically commit proof_cache.json: {e}"),
                Span::zero(),
                "Check filesystem permissions in .infimum/",
            )
        })
    }

    fn save_aliases(&self) -> Result<(), InfimumDiagnostic> {
        let json = serde_json::to_string_pretty(&self.aliases).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to serialize aliases: {e}"),
                Span::zero(),
                "Internal serialization error",
            )
        })?;
        fs::write(&self.aliases_file, json).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to write aliases.json: {e}"),
                Span::zero(),
                "Check disk space and permissions",
            )
        })
    }

    pub fn load_node(&self, full_hash: &str) -> Result<CanonicalNode, InfimumDiagnostic> {
        let path = self.nodes_dir.join(format!("{full_hash}.json"));
        let raw = fs::read_to_string(&path).map_err(|_| {
            InfimumDiagnostic::new(
                "E032_NODE_NOT_FOUND",
                "store",
                format!("Node #{full_hash} does not exist in store"),
                Span::zero(),
                "Ingest the missing node first or check hash spelling",
            )
        })?;
        serde_json::from_str(&raw).map_err(|e| {
            InfimumDiagnostic::new(
                "E031_STORE_CORRUPT",
                "store",
                format!("Corrupted node file #{full_hash}: {e}"),
                Span::zero(),
                "Re-ingest source definitions into .infimum/nodes",
            )
        })
    }

    pub fn resolve_hash_prefix(&self, prefix: &str) -> Result<String, InfimumDiagnostic> {
        let clean = prefix.trim_start_matches('#').to_ascii_lowercase();
        if clean.len() == 64 && self.nodes_dir.join(format!("{clean}.json")).is_file() {
            return Ok(clean);
        }

        let mut matches = Vec::new();
        let entries = fs::read_dir(&self.nodes_dir).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Cannot read .infimum/nodes: {e}"),
                Span::zero(),
                "Ensure .infimum/nodes directory exists",
            )
        })?;

        for entry in entries.flatten() {
            let fname = entry.file_name();
            let s = fname.to_string_lossy();
            if let Some(stem) = s.strip_suffix(".json") {
                if stem.starts_with(&clean) {
                    matches.push(stem.to_string());
                }
            }
        }

        match matches.len() {
            0 => Err(InfimumDiagnostic::new(
                "E032_NODE_NOT_FOUND",
                "resolve",
                format!("No node hash matching '#{clean}' found in store"),
                Span::zero(),
                "Run 'infimum graph' to list available node hashes",
            )),
            1 => Ok(matches.remove(0)),
            _ => Err(InfimumDiagnostic::new(
                "E033_AMBIGUOUS_HASH_PREFIX",
                "resolve",
                format!("Hash prefix '#{clean}' matches multiple nodes: {matches:?}"),
                Span::zero(),
                "Provide a longer hash prefix (e.g. 12+ hex chars)",
            )),
        }
    }

    pub fn resolve_ref(&self, name_or_hash: &str) -> Result<String, InfimumDiagnostic> {
        if let Some(stripped) = name_or_hash.strip_prefix('#') {
            return self.resolve_hash_prefix(stripped);
        }
        if let Some(full) = self.aliases.get(name_or_hash) {
            return Ok(full.clone());
        }
        // Also allow bare hash prefix if valid hex >= 6 chars
        if name_or_hash.len() >= 6 && name_or_hash.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(full) = self.resolve_hash_prefix(name_or_hash) {
                return Ok(full);
            }
        }
        Err(InfimumDiagnostic::new(
            "E022_UNKNOWN_FUNCTION",
            "resolve",
            format!("Unknown function alias or hash '{name_or_hash}'"),
            Span::zero(),
            "Define the target function or ingest it into the .infimum store first",
        ))
    }

    pub fn aliases_for_hash(&self, full_hash: &str) -> Vec<String> {
        self.aliases
            .iter()
            .filter_map(|(alias, h)| {
                if h == full_hash {
                    Some(alias.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn rename_alias(&mut self, old_name: &str, new_name: &str) -> Result<String, InfimumDiagnostic> {
        let hash = self.aliases.remove(old_name).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E022_UNKNOWN_FUNCTION",
                "store",
                format!("Alias '{old_name}' does not exist"),
                Span::zero(),
                "Run 'infimum graph' to inspect existing aliases",
            )
        })?;
        self.aliases.insert(new_name.to_string(), hash.clone());
        self.save_aliases()?;
        Ok(hash)
    }

    pub fn verify_module(&self, module: &Module) -> Result<Vec<VerificationReport>, InfimumDiagnostic> {
        self.verify_module_with_cache(module, true)
    }

    fn verify_module_formas_with_cache(
        &self,
        module: &Module,
        use_cache: bool,
        z3_version: &str,
        cache_dirty: &mut bool,
    ) -> Result<(), InfimumDiagnostic> {
        if module.formas.is_empty() {
            return Ok(());
        }
        let mut misses: Vec<(usize, String)> = Vec::new();
        for (i, forma) in module.formas.iter().enumerate() {
            let key = compute_forma_proof_key(forma, z3_version);
            let hit = if use_cache {
                self.proof_cache
                    .lock()
                    .ok()
                    .map(|g| g.entries.contains_key(&key))
                    .unwrap_or(false)
            } else {
                false
            };
            if !hit {
                misses.push((i, key));
            }
        }
        if misses.is_empty() {
            return Ok(());
        }

        type VerifyBatchItem = (usize, String, Result<usize, InfimumDiagnostic>);
        let results: Mutex<Vec<VerifyBatchItem>> =
            Mutex::new(Vec::with_capacity(misses.len()));
        let next_idx = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(misses.len())
            .max(1);

        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let pos = next_idx.fetch_add(1, Ordering::Relaxed);
                    if pos >= misses.len() {
                        break;
                    }
                    let (fi, ref key) = misses[pos];
                    let res = crate::forma::verify_ui::verify_forma(&module.formas[fi])
                        .map(|r| r.obligations_proved);
                    if let Ok(mut g) = results.lock() {
                        g.push((fi, key.clone(), res));
                    }
                });
            }
        });

        let mut drained = results.into_inner().unwrap_or_default();
        drained.sort_by_key(|(fi, _, _)| *fi);
        let mut first_err: Option<InfimumDiagnostic> = None;
        for (fi, key, res) in drained {
            match res {
                Ok(obligations) => {
                    if let Ok(mut guard) = self.proof_cache.lock() {
                        guard.entries.insert(
                            key,
                            ProofCacheEntry {
                                obligations_checked: obligations,
                                node_hash: format!("forma:{}", module.formas[fi].name),
                                z3_version: z3_version.to_string(),
                            },
                        );
                        *cache_dirty = true;
                    }
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(())
    }

    /// Two-phase incremental + parallel verification for all functions in `module`.
    /// Phase A (sequential, ~5ms): canonicalize, typecheck, compute contract-invariant proof keys, lookup cache.
    /// Phase B (parallel across CPU cores): verify only cache misses via Z3 simultaneously.
    fn stage_and_verify_module(
        &self,
        module: &Module,
        use_cache: bool,
    ) -> Result<Vec<(String, CanonicalNode, VerificationReport)>, InfimumDiagnostic> {
        let z3_version = z3_version_id()?;
        let mut cache_dirty = false;

        if let Err(e) = self.verify_module_formas_with_cache(module, use_cache, &z3_version, &mut cache_dirty) {
            if cache_dirty {
                let _ = self.save_proof_cache();
            }
            return Err(e);
        }

        let order = topo_sort_functions(&module.functions)?;
        let mut staged_aliases = self.aliases.clone();
        let mut staged_nodes: BTreeMap<String, CanonicalNode> = BTreeMap::new();
        let mut items: Vec<(usize, String, CanonicalNode, String, Option<VerificationReport>)> =
            Vec::with_capacity(order.len());

        // Phase A: Canonicalize + typecheck + contract-invariant proof key lookup
        for &idx in &order {
            let raw_fn = &module.functions[idx];
            let mut node = canonicalize_fn(
                raw_fn,
                &|callee| {
                    staged_aliases.get(callee).cloned().ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E022_UNKNOWN_FUNCTION",
                            "resolve",
                            format!(
                                "Function '{}' calls unknown function '{callee}'",
                                raw_fn.name
                            ),
                            Span::zero(),
                            "Define the callee in the same input or ingest it into .infimum first",
                        )
                    })
                },
                &|prefix| self.resolve_hash_prefix(prefix),
            )?;

            let loader = |dep_hash: &str| -> Result<CanonicalNode, InfimumDiagnostic> {
                if let Some(mem_node) = staged_nodes.get(dep_hash) {
                    Ok(mem_node.clone())
                } else {
                    self.load_node(dep_hash)
                }
            };

            node.direct_secret_sinks = typecheck_node(&raw_fn.name, &node, &loader)?;
            let proof_key = compute_proof_key(&node, &loader, &z3_version)?;
            let cached_report = if use_cache {
                self.proof_cache
                    .lock()
                    .ok()
                    .and_then(|guard| guard.entries.get(&proof_key).cloned())
                    .map(|entry| VerificationReport {
                        alias: raw_fn.name.clone(),
                        short_hash: node.short_hash.clone(),
                        obligations_checked: entry.obligations_checked,
                        elapsed_ms: 0,
                        status: "PROVED (cached)".to_string(),
                        cached: true,
                    })
            } else {
                None
            };

            staged_aliases.insert(raw_fn.name.clone(), node.hash.clone());
            staged_nodes.insert(node.hash.clone(), node.clone());
            items.push((idx, raw_fn.name.clone(), node, proof_key, cached_report));
        }

        // Phase B: Verify all cache misses in parallel across CPU cores!
        let miss_positions: Vec<usize> = items
            .iter()
            .enumerate()
            .filter_map(|(pos, item)| if item.4.is_none() { Some(pos) } else { None })
            .collect();

        if !miss_positions.is_empty() {
            let results: Mutex<Vec<(usize, Result<VerificationReport, InfimumDiagnostic>)>> =
                Mutex::new(Vec::with_capacity(miss_positions.len()));
            let next_m = AtomicUsize::new(0);
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(miss_positions.len())
                .max(1);

            let staged_nodes_ref = &staged_nodes;
            let items_ref = &items;

            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| {
                        let loader = |dep_hash: &str| -> Result<CanonicalNode, InfimumDiagnostic> {
                            if let Some(mem_node) = staged_nodes_ref.get(dep_hash) {
                                Ok(mem_node.clone())
                            } else {
                                self.load_node(dep_hash)
                            }
                        };
                        loop {
                            let mi = next_m.fetch_add(1, Ordering::Relaxed);
                            if mi >= miss_positions.len() {
                                break;
                            }
                            let pos = miss_positions[mi];
                            let (fn_idx, _, ref node, _, _) = items_ref[pos];
                            let raw_fn = &module.functions[fn_idx];
                            let res = verify_node(raw_fn, node, &loader);
                            if let Ok(mut g) = results.lock() {
                                g.push((pos, res));
                            }
                        }
                    });
                }
            });

            let mut drained = results.into_inner().unwrap_or_default();
            drained.sort_by_key(|(pos, _)| *pos);
            let mut first_err: Option<InfimumDiagnostic> = None;
            for (pos, res) in drained {
                match res {
                    Ok(report) => {
                        let proof_key = items[pos].3.clone();
                        let node_hash = items[pos].2.hash.clone();
                        if let Ok(mut guard) = self.proof_cache.lock() {
                            guard.entries.insert(
                                proof_key,
                                ProofCacheEntry {
                                    obligations_checked: report.obligations_checked,
                                    node_hash,
                                    z3_version: z3_version.clone(),
                                },
                            );
                            cache_dirty = true;
                        }
                        items[pos].4 = Some(report);
                    }
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            if let Some(e) = first_err {
                if cache_dirty {
                    let _ = self.save_proof_cache();
                }
                return Err(e);
            }
        }

        if cache_dirty {
            self.save_proof_cache()?;
        }

        let mut out = Vec::with_capacity(items.len());
        for (_, name, node, _, rep_opt) in items {
            if let Some(rep) = rep_opt {
                out.push((name, node, rep));
            }
        }
        Ok(out)
    }

    pub fn verify_module_with_cache(
        &self,
        module: &Module,
        use_cache: bool,
    ) -> Result<Vec<VerificationReport>, InfimumDiagnostic> {
        let staged = self.stage_and_verify_module(module, use_cache)?;
        Ok(staged.into_iter().map(|(_, _, r)| r).collect())
    }

    pub fn ingest_module(&mut self, module: &Module) -> Result<Vec<IngestEntry>, InfimumDiagnostic> {
        self.ingest_module_with_cache(module, true)
    }

    pub fn ingest_module_with_cache(
        &mut self,
        module: &Module,
        use_cache: bool,
    ) -> Result<Vec<IngestEntry>, InfimumDiagnostic> {
        let staged_entries = self.stage_and_verify_module(module, use_cache)?;

        // Phase 2: Commit verified nodes atomically into .infimum/nodes/
        let mut results = Vec::with_capacity(staged_entries.len());
        for (alias, node, vreport) in staged_entries {
            let node_file = self.nodes_dir.join(format!("{}.json", node.hash));
            let deduplicated = node_file.is_file();
            if !deduplicated {
                let json = serde_json::to_string_pretty(&node).map_err(|e| {
                    InfimumDiagnostic::new(
                        "E030_STORE_IO",
                        "store",
                        format!("Failed to serialize node: {e}"),
                        Span::zero(),
                        "Internal JSON serialization error",
                    )
                })?;
                fs::write(&node_file, json).map_err(|e| {
                    InfimumDiagnostic::new(
                        "E030_STORE_IO",
                        "store",
                        format!("Failed to write node file: {e}"),
                        Span::zero(),
                        "Check write permissions in .infimum/nodes",
                    )
                })?;
            }

            self.aliases.insert(alias.clone(), node.hash.clone());
            results.push(IngestEntry {
                alias,
                hash: node.hash,
                short_hash: node.short_hash,
                deduplicated,
                obligations_proved: vreport.obligations_checked,
                verify_ms: vreport.elapsed_ms,
            });
        }

        self.save_aliases()?;
        Ok(results)
    }

    pub fn show_node(&self, name_or_hash: &str) -> Result<NodeView, InfimumDiagnostic> {
        let full_hash = self.resolve_ref(name_or_hash)?;
        let node = self.load_node(&full_hash)?;
        let aliases = self.aliases_for_hash(&full_hash);

        let mut neighbor_signatures = Vec::with_capacity(node.direct_deps.len());
        for dep_hash in &node.direct_deps {
            let dep = self.load_node(dep_hash)?;
            neighbor_signatures.push(NeighborSig {
                hash: dep.hash.clone(),
                short_hash: dep.short_hash.clone(),
                aliases: self.aliases_for_hash(&dep.hash),
                params: dep
                    .ast
                    .params
                    .iter()
                    .map(|p| format!("{}:{}", p.name, p.ty))
                    .collect(),
                effects: dep.direct_effects.iter().map(|e| e.to_string()).collect(),
                ret_type: dep.ast.ret_type.to_string(),
            });
        }

        Ok(NodeView {
            node,
            aliases,
            neighbor_signatures,
        })
    }

    pub fn list_graph(&self) -> Result<Vec<GraphSummaryNode>, InfimumDiagnostic> {
        let mut nodes = Vec::new();
        let entries = fs::read_dir(&self.nodes_dir).map_err(|e| {
            InfimumDiagnostic::new(
                "E030_STORE_IO",
                "store",
                format!("Failed to read .infimum/nodes: {e}"),
                Span::zero(),
                "Check directory permissions",
            )
        })?;

        for entry in entries.flatten() {
            let fname = entry.file_name();
            let s = fname.to_string_lossy();
            if let Some(hash) = s.strip_suffix(".json") {
                let node = self.load_node(hash)?;
                nodes.push(GraphSummaryNode {
                    hash: node.hash.clone(),
                    short_hash: node.short_hash,
                    aliases: self.aliases_for_hash(&node.hash),
                    direct_deps: node.direct_deps,
                    direct_effects: node.direct_effects.iter().map(|e| e.to_string()).collect(),
                });
            }
        }
        nodes.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(nodes)
    }

    pub fn compute_passport(&self, name_or_hash: &str) -> Result<SecurityPassport, InfimumDiagnostic> {
        let root_hash = self.resolve_ref(name_or_hash)?;
        let root_node = self.load_node(&root_hash)?;
        let root_aliases = self.aliases_for_hash(&root_hash);

        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
        let mut effect_witness: BTreeMap<EffectCap, Vec<String>> = BTreeMap::new();
        let mut sink_witness: BTreeMap<DirectSecretSink, Vec<String>> = BTreeMap::new();
        let mut uses_secret = fn_uses_secret(&root_node.ast);

        let root_label = format_node_label(&root_node, &root_aliases);
        visited.insert(root_hash.clone());
        queue.push_back((root_hash.clone(), vec![root_label]));

        while let Some((curr_hash, path)) = queue.pop_front() {
            let curr = self.load_node(&curr_hash)?;
            if fn_uses_secret(&curr.ast) {
                uses_secret = true;
            }
            for eff in &curr.direct_effects {
                effect_witness
                    .entry(eff.clone())
                    .or_insert_with(|| path.clone());
            }
            for sink in &curr.direct_secret_sinks {
                sink_witness
                    .entry(sink.clone())
                    .or_insert_with(|| path.clone());
            }
            for dep_hash in &curr.direct_deps {
                if visited.insert(dep_hash.clone()) {
                    let dep_node = self.load_node(dep_hash)?;
                    let dep_aliases = self.aliases_for_hash(dep_hash);
                    let mut next_path = path.clone();
                    next_path.push(format_node_label(&dep_node, &dep_aliases));
                    queue.push_back((dep_hash.clone(), next_path));
                }
            }
        }

        let capabilities: Vec<CapabilityGrant> = effect_witness
            .into_iter()
            .map(|(eff, chain)| CapabilityGrant {
                effect: eff.to_string(),
                witness_chain: chain,
            })
            .collect();

        let secret_pipes: Vec<SecretPipe> = sink_witness
            .into_iter()
            .map(|(sink, chain)| SecretPipe {
                secret_type: sink.secret_type,
                destination_domain: sink.domain,
                operation: sink.operation,
                witness_chain: chain,
            })
            .collect();

        Ok(SecurityPassport {
            root_hash,
            short_hash: root_node.short_hash,
            aliases: root_aliases,
            total_reachable_nodes: visited.len(),
            is_pure: capabilities.is_empty(),
            uses_secret_types: uses_secret,
            capabilities,
            secret_pipes,
        })
    }
}

fn format_node_label(node: &CanonicalNode, aliases: &[String]) -> String {
    if let Some(first) = aliases.first() {
        format!("{first} (#{})", node.short_hash)
    } else {
        format!("#{}", node.short_hash)
    }
}

fn type_has_secret(ty: &Type) -> bool {
    match ty {
        Type::Secret(_) => true,
        Type::Tuple(ts) => ts.iter().any(type_has_secret),
        Type::I64 | Type::Fixed | Type::Bool | Type::Str | Type::Unit => false,
    }
}

fn fn_uses_secret(f: &FnDef) -> bool {
    f.params.iter().any(|p| type_has_secret(&p.ty))
        || type_has_secret(&f.ret_type)
        || expr_uses_secret(&f.body)
}

fn stmt_uses_secret(s: &Stmt) -> bool {
    match s {
        Stmt::Let { ty, value, .. } => {
            ty.as_ref().is_some_and(type_has_secret) || expr_uses_secret(value)
        }
        Stmt::Assign { value, .. } | Stmt::LetTuple { value, .. } => expr_uses_secret(value),
        Stmt::While { cond, body, .. } => {
            expr_uses_secret(cond) || body.iter().any(stmt_uses_secret)
        }
        Stmt::Expr(inner) => expr_uses_secret(inner),
    }
}

fn expr_uses_secret(e: &Expr) -> bool {
    match e {
        Expr::SecretWrap(_) | Expr::Reveal { .. } => true,
        Expr::Unary { expr: inner, .. } | Expr::MemLoad { index: inner, .. } => expr_uses_secret(inner),
        Expr::MemStore { index, value, .. } => expr_uses_secret(index) || expr_uses_secret(value),
        Expr::Binary { lhs, rhs, .. } => expr_uses_secret(lhs) || expr_uses_secret(rhs),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => expr_uses_secret(cond) || expr_uses_secret(then_branch) || expr_uses_secret(else_branch),
        Expr::Call { args, .. } | Expr::Perform { args, .. } => args.iter().any(expr_uses_secret),
        Expr::Block { stmts, tail } => stmts.iter().any(stmt_uses_secret) || expr_uses_secret(tail),
        Expr::TupleLit(elems) => elems.iter().any(expr_uses_secret),
        Expr::TupleField { expr, .. } => expr_uses_secret(expr),
        Expr::IntLit(_) | Expr::FixedLit(_) | Expr::BoolLit(_) | Expr::StrLit(_) | Expr::UnitLit | Expr::Var(_) => {
            false
        }
    }
}

fn collect_stmt_calls(s: &Stmt, self_name: &str, out: &mut BTreeSet<String>) {
    match s {
        Stmt::Let { value, .. } | Stmt::LetTuple { value, .. } | Stmt::Assign { value, .. } => {
            collect_named_calls(value, self_name, out)
        }
        Stmt::While { cond, body, .. } => {
            collect_named_calls(cond, self_name, out);
            for inner in body {
                collect_stmt_calls(inner, self_name, out);
            }
        }
        Stmt::Expr(e) => collect_named_calls(e, self_name, out),
    }
}

fn collect_named_calls(expr: &Expr, self_name: &str, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Call { target, args } => {
            if let TargetRef::Named(name) = target {
                if name != self_name {
                    out.insert(name.clone());
                }
            }
            for a in args {
                collect_named_calls(a, self_name, out);
            }
        }
        Expr::Perform { args, .. } => {
            for a in args {
                collect_named_calls(a, self_name, out);
            }
        }
        Expr::Unary { expr: inner, .. }
        | Expr::MemLoad { index: inner, .. }
        | Expr::SecretWrap(inner)
        | Expr::Reveal { expr: inner, .. }
        | Expr::TupleField { expr: inner, .. } => collect_named_calls(inner, self_name, out),
        Expr::TupleLit(elems) => {
            for e in elems {
                collect_named_calls(e, self_name, out);
            }
        }
        Expr::MemStore { index, value, .. } => {
            collect_named_calls(index, self_name, out);
            collect_named_calls(value, self_name, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_named_calls(lhs, self_name, out);
            collect_named_calls(rhs, self_name, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_named_calls(cond, self_name, out);
            collect_named_calls(then_branch, self_name, out);
            collect_named_calls(else_branch, self_name, out);
        }
        Expr::Block { stmts, tail } => {
            for s in stmts {
                collect_stmt_calls(s, self_name, out);
            }
            collect_named_calls(tail, self_name, out);
        }
        Expr::IntLit(_) | Expr::FixedLit(_) | Expr::BoolLit(_) | Expr::StrLit(_) | Expr::UnitLit | Expr::Var(_) => {}
    }
}

pub(crate) fn topo_sort_functions(fns: &[FnDef]) -> Result<Vec<usize>, InfimumDiagnostic> {
    let mut name_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, f) in fns.iter().enumerate() {
        if name_to_idx.insert(f.name.clone(), i).is_some() {
            return Err(InfimumDiagnostic::new(
                "E024_DUPLICATE_FN_NAME",
                "resolve",
                format!("Duplicate function name '{}' in the same module", f.name),
                Span::zero(),
                "Give each function in a batch a distinct alias or ingest versions separately",
            ));
        }
    }

    let n = fns.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indegree: Vec<usize> = vec![0; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, f) in fns.iter().enumerate() {
        let mut called = BTreeSet::new();
        for r in &f.requires {
            collect_named_calls(r, &f.name, &mut called);
        }
        for e in &f.ensures {
            collect_named_calls(e, &f.name, &mut called);
        }
        collect_named_calls(&f.body, &f.name, &mut called);

        for cname in called {
            if let Some(&dep_idx) = name_to_idx.get(&cname) {
                deps[i].push(dep_idx);
                dependents[dep_idx].push(i);
                indegree[i] += 1;
            }
        }
    }

    let mut queue: VecDeque<usize> = VecDeque::new();
    for (i, &deg) in indegree.iter().enumerate() {
        if deg == 0 {
            queue.push_back(i);
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(u) = queue.pop_front() {
        order.push(u);
        for &v in &dependents[u] {
            indegree[v] -= 1;
            if indegree[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    if order.len() != n {
        return Err(InfimumDiagnostic::new(
            "E023_MUTUAL_RECURSION",
            "resolve",
            "Mutual recursion cycle detected across functions in module",
            Span::zero(),
            "Refactor mutually recursive functions into a single recursive node using self-calls",
        ));
    }

    Ok(order)
}
