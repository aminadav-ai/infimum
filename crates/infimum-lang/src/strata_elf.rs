//! Direct Linux `x86_64` ELF64 Machine-Code Emitter for `Infimum.Strata`.
//!
//! Compiles verified Infimum graphs directly into standalone static Linux `x86_64`
//! machine-code executables (~2-5 KB) in <1ms with ZERO `rustc`, `gcc`, `ld`, or `libc`.

use crate::ast::{BinOp, Expr, FnDef, Param, Stmt, TargetRef, Type, UnaryOp};
use crate::canon::CanonicalNode;
use crate::diag::{InfimumDiagnostic, Span};
use crate::store::GraphStore;
use crate::strata_wasm::collect_multi_root_subgraph;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

const ELF_BASE_VADDR: u64 = 0x0040_0000;
const BSS_MEM_VADDR: u64 = 0x0080_0000;
const BSS_MEM_BYTES: u64 = 0x0080_0000; // 8 MiB zero-initialized linear memory
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const HEADERS_TOTAL: usize = EHDR_SIZE + 2 * PHDR_SIZE; // 176 bytes (0xB0)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrataElfArtifact {
    pub entry_ref: String,
    pub short_hash: String,
    pub compiled_nodes: usize,
    pub elf_path: String,
    pub elf_size_bytes: u64,
    pub build_us: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrataRunOutcome {
    pub artifact: StrataElfArtifact,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub run_us: u128,
}

pub fn build_strata_elf_target(
    store: &GraphStore,
    target_ref: &str,
    out_path: &Path,
) -> Result<StrataElfArtifact, InfimumDiagnostic> {
    let t0 = Instant::now();
    let root_hash = store.resolve_ref(target_ref)?;
    let root_node = store.load_node(&root_hash)?;
    let nodes = collect_multi_root_subgraph(store, std::slice::from_ref(&root_hash))?;

    let elf_bytes = emit_elf64_binary(&nodes, &root_node)?;

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    fs::write(out_path, &elf_bytes).map_err(|e| {
        InfimumDiagnostic::new(
            "E070_BUILD_IO",
            "strata-elf",
            format!("Failed to write ELF64 binary '{}': {e}", out_path.display()),
            Span::zero(),
            "Check write permissions on output path",
        )
    })?;
    #[cfg(unix)]
    {
        if let Ok(meta) = fs::metadata(out_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(out_path, perms);
        }
    }

    Ok(StrataElfArtifact {
        entry_ref: target_ref.to_string(),
        short_hash: root_node.short_hash,
        compiled_nodes: nodes.len(),
        elf_path: out_path.display().to_string(),
        elf_size_bytes: elf_bytes.len() as u64,
        build_us: t0.elapsed().as_micros(),
    })
}

pub fn run_strata_elf_target(
    store: &GraphStore,
    workspace_root: &Path,
    target_ref: &str,
    cli_args: &[&str],
) -> Result<StrataRunOutcome, InfimumDiagnostic> {
    let bin_dir = workspace_root.join(".infimum").join("strata_bin");
    let _ = fs::create_dir_all(&bin_dir);
    let root_hash = store.resolve_ref(target_ref)?;
    let elf_path = bin_dir.join(&root_hash);

    let artifact = build_strata_elf_target(store, target_ref, &elf_path)?;
    let trun = Instant::now();
    let out = Command::new(&elf_path).args(cli_args).output().map_err(|e| {
        InfimumDiagnostic::new(
            "E073_RUN_EXEC_FAILED",
            "strata-elf",
            format!("Failed to execute Strata ELF binary '{}': {e}", elf_path.display()),
            Span::zero(),
            "Ensure host OS is x86_64 Linux",
        )
    })?;

    Ok(StrataRunOutcome {
        artifact,
        exit_code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        run_us: trun.elapsed().as_micros(),
    })
}

#[derive(Clone, Copy)]
enum SymTarget {
    NodeFn(usize),
    HelperParseArg,
    HelperPrintI64,
    HelperPrintBool,
}

struct CallReloc {
    disp32_offset: usize,
    target: SymTarget,
}

fn emit_elf64_binary(
    nodes: &[CanonicalNode],
    root_node: &CanonicalNode,
) -> Result<Vec<u8>, InfimumDiagnostic> {
    let mut hash_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        hash_to_idx.insert(n.hash.clone(), i);
    }
    let root_idx = *hash_to_idx.get(&root_node.hash).ok_or_else(|| {
        InfimumDiagnostic::new(
            "E032_NODE_NOT_FOUND",
            "strata-elf",
            "Root node missing in compiled subgraph",
            Span::zero(),
            "Ingest root node first",
        )
    })?;

    let mut code = Vec::with_capacity(2048);
    let mut relocs: Vec<CallReloc> = Vec::new();
    let mut fn_offsets = vec![0usize; nodes.len()];

    // 1. Emit each compiled Infimum.Strata node function
    for (idx, node) in nodes.iter().enumerate() {
        fn_offsets[idx] = code.len();
        compile_x86_node_fn(node, idx, &hash_to_idx, &mut code, &mut relocs)?;
    }

    // 2. Emit minimal built-in runtime helpers (`parse_arg`, `print_i64`, `print_bool`)
    let helper_parse_off = code.len();
    emit_helper_parse_i64(&mut code);

    let helper_print_i64_off = code.len();
    emit_helper_print_i64(&mut code);

    let helper_print_bool_off = code.len();
    emit_helper_print_bool(&mut code);

    // 3. Emit `_start` entrypoint (checks arity, parses CLI args, checks root `req` contracts, calls root, prints result, exits)
    let start_off = code.len();
    emit_x86_start(root_node, root_idx, &hash_to_idx, &mut code, &mut relocs)?;

    // 4. Patch all relative `call rel32` displacements
    for rel in &relocs {
        let target_off = match rel.target {
            SymTarget::NodeFn(i) => fn_offsets[i],
            SymTarget::HelperParseArg => helper_parse_off,
            SymTarget::HelperPrintI64 => helper_print_i64_off,
            SymTarget::HelperPrintBool => helper_print_bool_off,
        };
        let next_rip = rel.disp32_offset + 4;
        let disp = (target_off as i64) - (next_rip as i64);
        let disp32 = disp as i32;
        code[rel.disp32_offset..rel.disp32_offset + 4].copy_from_slice(&disp32.to_le_bytes());
    }

    let total_file_size = HEADERS_TOTAL + code.len();
    let entry_vaddr = ELF_BASE_VADDR + (HEADERS_TOTAL + start_off) as u64;

    let mut elf = Vec::with_capacity(total_file_size);
    write_elf64_headers(&mut elf, entry_vaddr, total_file_size as u64);
    elf.extend_from_slice(&code);
    Ok(elf)
}

struct X86FnCtx<'a> {
    self_idx: usize,
    hash_to_idx: &'a BTreeMap<String, usize>,
    // Maps canonical var name -> signed RBP displacement (`[rbp + disp32]`)
    var_disp: BTreeMap<String, i32>,
    next_local_disp: i32,
}

impl<'a> X86FnCtx<'a> {
    fn alloc_slot(&mut self, name: Option<&str>) -> i32 {
        self.next_local_disp -= 8;
        let disp = self.next_local_disp;
        if let Some(n) = name {
            self.var_disp.insert(n.to_string(), disp);
        }
        disp
    }
}

fn compile_x86_node_fn(
    node: &CanonicalNode,
    self_idx: usize,
    hash_to_idx: &BTreeMap<String, usize>,
    code: &mut Vec<u8>,
    relocs: &mut Vec<CallReloc>,
) -> Result<(), InfimumDiagnostic> {
    let f: &FnDef = &node.ast;
    let arity = f.params.len();
    let mut var_disp = BTreeMap::new();

    // Caller pushes p0, p1, ..., p_{arity-1} left-to-right before `call`:
    // `[rbp + 16]` is `p_{arity-1}`, `[rbp + 16 + (arity - 1 - i)*8]` is `p_i`.
    for (i, p) in f.params.iter().enumerate() {
        let disp = 16 + ((arity - 1 - i) as i32) * 8;
        var_disp.insert(p.name.clone(), disp);
    }

    let mut ctx = X86FnCtx {
        self_idx,
        hash_to_idx,
        var_disp,
        next_local_disp: 0,
    };
    prealloc_x86_expr_slots(&f.body, &mut ctx);

    // Emit into temporary body buffer so we know exact frame size (including loop fuel slots)
    let mut body_bytes = Vec::with_capacity(256);
    let mut body_relocs: Vec<CallReloc> = Vec::new();
    emit_x86_expr(&f.body, &mut ctx, &mut body_bytes, &mut body_relocs)?;

    let raw_frame = (-ctx.next_local_disp) as u32;
    let aligned_frame = (raw_frame + 15) & !15;

    // Function prologue: push rbp; mov rbp, rsp; sub rsp, aligned_frame
    code.push(0x55);
    code.extend_from_slice(&[0x48, 0x89, 0xE5]);
    if aligned_frame > 0 {
        code.extend_from_slice(&[0x48, 0x81, 0xEC]);
        code.extend_from_slice(&aligned_frame.to_le_bytes());
    }

    let body_base = code.len();
    for r in body_relocs {
        relocs.push(CallReloc {
            disp32_offset: body_base + r.disp32_offset,
            target: r.target,
        });
    }
    code.extend_from_slice(&body_bytes);

    // Function epilogue: mov rsp, rbp; pop rbp; ret
    code.extend_from_slice(&[0x48, 0x89, 0xEC, 0x5D, 0xC3]);
    Ok(())
}

fn prealloc_x86_stmt_slots(stmts: &[Stmt], ctx: &mut X86FnCtx<'_>) {
    for s in stmts {
        match s {
            Stmt::Let { name, value, .. } => {
                prealloc_x86_expr_slots(value, ctx);
                if !ctx.var_disp.contains_key(name) {
                    ctx.alloc_slot(Some(name));
                }
            }
            Stmt::LetTuple { names, value } => {
                prealloc_x86_expr_slots(value, ctx);
                for (n, _) in names {
                    if !ctx.var_disp.contains_key(n) {
                        ctx.alloc_slot(Some(n));
                    }
                }
            }
            Stmt::Assign { value, .. } => prealloc_x86_expr_slots(value, ctx),
            Stmt::While { cond, body, .. } => {
                prealloc_x86_expr_slots(cond, ctx);
                prealloc_x86_stmt_slots(body, ctx);
            }
            Stmt::Expr(e) => prealloc_x86_expr_slots(e, ctx),
        }
    }
}

fn prealloc_x86_expr_slots(expr: &Expr, ctx: &mut X86FnCtx<'_>) {
    match expr {
        Expr::Block { stmts, tail } => {
            prealloc_x86_stmt_slots(stmts, ctx);
            prealloc_x86_expr_slots(tail, ctx);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            prealloc_x86_expr_slots(cond, ctx);
            prealloc_x86_expr_slots(then_branch, ctx);
            prealloc_x86_expr_slots(else_branch, ctx);
        }
        Expr::Unary { expr: inner, .. }
        | Expr::MemLoad { index: inner, .. }
        | Expr::SecretWrap(inner)
        | Expr::Reveal { expr: inner, .. } => prealloc_x86_expr_slots(inner, ctx),
        Expr::MemStore { index, value, .. } => {
            prealloc_x86_expr_slots(index, ctx);
            prealloc_x86_expr_slots(value, ctx);
        }
        Expr::Binary { lhs, rhs, .. } => {
            prealloc_x86_expr_slots(lhs, ctx);
            prealloc_x86_expr_slots(rhs, ctx);
        }
        Expr::Call { args, .. } | Expr::Perform { args, .. } | Expr::TupleLit(args) => {
            for a in args {
                prealloc_x86_expr_slots(a, ctx);
            }
        }
        Expr::TupleField { expr: inner, .. }
        | Expr::SlicePtr(inner)
        | Expr::StrPtr(inner)
        | Expr::SliceLen(inner) => prealloc_x86_expr_slots(inner, ctx),
        Expr::SliceLit { ptr, len, .. }
        | Expr::BoundsCheck {
            index: ptr,
            len,
            ..
        } => {
            prealloc_x86_expr_slots(ptr, ctx);
            prealloc_x86_expr_slots(len, ctx);
        }
        Expr::RangeCheck { lo, hi, len } => {
            prealloc_x86_expr_slots(lo, ctx);
            prealloc_x86_expr_slots(hi, ctx);
            prealloc_x86_expr_slots(len, ctx);
        }
        Expr::IntLit(_) | Expr::FixedLit(_) | Expr::BoolLit(_) | Expr::StrLit(_) | Expr::UnitLit | Expr::Var(_) => {}
    }
}

fn emit_x86_stmts(
    stmts: &[Stmt],
    ctx: &mut X86FnCtx<'_>,
    out: &mut Vec<u8>,
    relocs: &mut Vec<CallReloc>,
) -> Result<(), InfimumDiagnostic> {
    for s in stmts {
        match s {
            Stmt::Let { name, value, .. } | Stmt::Assign { name, value } => {
                emit_x86_expr(value, ctx, out, relocs)?;
                let disp = *ctx.var_disp.get(name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E020_UNKNOWN_VAR",
                        "strata-elf",
                        format!("Unknown stack slot for '{name}'"),
                        Span::zero(),
                        "Internal slot error",
                    )
                })?;
                // mov [rbp + disp32], rax
                out.extend_from_slice(&[0x48, 0x89, 0x85]);
                out.extend_from_slice(&disp.to_le_bytes());
            }
            Stmt::LetTuple { .. } => {
                return Err(InfimumDiagnostic::new(
                    "E085_STRATA_UNSUPPORTED_TYPE",
                    "strata-elf",
                    "Tuples are supported in Strata Wasm (.ifw), interpreter, and Rust codegen (not yet in raw x86_64 ELF)",
                    Span::zero(),
                    "Use 'infimum compile-wasm' or 'infimum build' for tuple-returning functions",
                ));
            }
            Stmt::While {
                cond,
                max_iters,
                body,
            } => {
                let fuel_disp = ctx.alloc_slot(None);
                // mov rax, max_iters; mov [rbp + fuel_disp], rax
                emit_mov_rax_imm64(out, *max_iters as i64);
                out.extend_from_slice(&[0x48, 0x89, 0x85]);
                out.extend_from_slice(&fuel_disp.to_le_bytes());

                let loop_top = out.len();
                // Check fuel > 0: mov rax, [rbp + fuel_disp]; cmp rax, 0; jle exit
                out.extend_from_slice(&[0x48, 0x8B, 0x85]);
                out.extend_from_slice(&fuel_disp.to_le_bytes());
                out.extend_from_slice(&[0x48, 0x83, 0xF8, 0x00]); // cmp rax, 0
                out.extend_from_slice(&[0x0F, 0x8E]); // jle rel32
                let jle_fuel_patch = out.len();
                out.extend_from_slice(&0i32.to_le_bytes());

                // Check cond != 0
                emit_x86_expr(cond, ctx, out, relocs)?;
                out.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                out.extend_from_slice(&[0x0F, 0x84]); // je rel32
                let je_cond_patch = out.len();
                out.extend_from_slice(&0i32.to_le_bytes());

                // Decrement fuel: dec qword ptr [rbp + fuel_disp]
                out.extend_from_slice(&[0x48, 0xFF, 0x8D]);
                out.extend_from_slice(&fuel_disp.to_le_bytes());

                emit_x86_stmts(body, ctx, out, relocs)?;

                // jmp loop_top
                out.push(0xE9);
                let jmp_back_next = out.len() + 4;
                let back_disp = (loop_top as i32) - (jmp_back_next as i32);
                out.extend_from_slice(&back_disp.to_le_bytes());

                let loop_exit = out.len();
                patch_rel32(out, jle_fuel_patch, loop_exit);
                patch_rel32(out, je_cond_patch, loop_exit);
            }
            Stmt::Expr(e) => {
                emit_x86_expr(e, ctx, out, relocs)?;
            }
        }
    }
    Ok(())
}

fn emit_x86_expr(
    expr: &Expr,
    ctx: &mut X86FnCtx<'_>,
    out: &mut Vec<u8>,
    relocs: &mut Vec<CallReloc>,
) -> Result<(), InfimumDiagnostic> {
    match expr {
        Expr::IntLit(v) | Expr::FixedLit(v) => emit_mov_rax_imm64(out, *v),
        Expr::BoolLit(b) => emit_mov_rax_imm64(out, if *b { 1 } else { 0 }),
        Expr::UnitLit => {
            out.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
        }
        Expr::StrLit(_) | Expr::SecretWrap(_) | Expr::Reveal { .. } => {
            return Err(InfimumDiagnostic::new(
                "E085_STRATA_UNSUPPORTED_EXPR",
                "strata-elf",
                "Direct x86_64 Strata ELF emitter supports unboxed i64, bool, unit, and Mem",
                Span::zero(),
                "Use numeric/memory types in Strata kernels",
            ));
        }
        Expr::Var(name) => {
            let disp = *ctx.var_disp.get(name).ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E020_UNKNOWN_VAR",
                    "strata-elf",
                    format!("Unknown variable '{name}'"),
                    Span::zero(),
                    "Ensure variable is bound",
                )
            })?;
            // mov rax, [rbp + disp32]
            out.extend_from_slice(&[0x48, 0x8B, 0x85]);
            out.extend_from_slice(&disp.to_le_bytes());
        }
        Expr::Unary { op, expr: inner } => {
            emit_x86_expr(inner, ctx, out, relocs)?;
            match op {
                UnaryOp::Neg => out.extend_from_slice(&[0x48, 0xF7, 0xD8]), // neg rax
                UnaryOp::Not => {
                    out.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                    out.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                    out.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                }
                UnaryOp::IntToFixed => {
                    out.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x10]); // shl rax, 16
                }
                UnaryOp::FixedToInt => {
                    out.extend_from_slice(&[0x48, 0xC1, 0xF8, 0x10]); // sar rax, 16
                }
                UnaryOp::FixedToRaw | UnaryOp::RawToFixed => {}
            }
        }
        Expr::Binary { op, lhs, rhs } => match op {
            BinOp::And => {
                emit_x86_expr(lhs, ctx, out, relocs)?;
                out.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                out.extend_from_slice(&[0x0F, 0x84]); // je false_lbl
                let je_patch = out.len();
                out.extend_from_slice(&0i32.to_le_bytes());

                emit_x86_expr(rhs, ctx, out, relocs)?;
                out.push(0xE9); // jmp end_lbl
                let jmp_patch = out.len();
                out.extend_from_slice(&0i32.to_le_bytes());

                let false_lbl = out.len();
                patch_rel32(out, je_patch, false_lbl);
                out.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax

                let end_lbl = out.len();
                patch_rel32(out, jmp_patch, end_lbl);
            }
            BinOp::Or => {
                emit_x86_expr(lhs, ctx, out, relocs)?;
                out.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                out.extend_from_slice(&[0x0F, 0x85]); // jne true_lbl
                let jne_patch = out.len();
                out.extend_from_slice(&0i32.to_le_bytes());

                emit_x86_expr(rhs, ctx, out, relocs)?;
                out.push(0xE9); // jmp end_lbl
                let jmp_patch = out.len();
                out.extend_from_slice(&0i32.to_le_bytes());

                let true_lbl = out.len();
                patch_rel32(out, jne_patch, true_lbl);
                emit_mov_rax_imm64(out, 1);

                let end_lbl = out.len();
                patch_rel32(out, jmp_patch, end_lbl);
            }
            _ => {
                emit_x86_expr(lhs, ctx, out, relocs)?;
                out.push(0x50); // push rax (lhs)
                emit_x86_expr(rhs, ctx, out, relocs)?;
                // mov rcx, rax (rhs); pop rax (lhs)
                out.extend_from_slice(&[0x48, 0x89, 0xC1, 0x58]);
                match op {
                    BinOp::Add => out.extend_from_slice(&[0x48, 0x01, 0xC8]),
                    BinOp::Sub => out.extend_from_slice(&[0x48, 0x29, 0xC8]),
                    BinOp::Mul => out.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]),
                    BinOp::Div => out.extend_from_slice(&[0x48, 0x99, 0x48, 0xF7, 0xF9]),
                    BinOp::FixedMul => {
                        out.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
                        out.extend_from_slice(&[0x48, 0xC1, 0xF8, 0x10]); // sar rax, 16
                    }
                    BinOp::FixedDiv => {
                        out.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x10]); // shl rax, 16
                        out.extend_from_slice(&[0x48, 0x99, 0x48, 0xF7, 0xF9]); // cqo; idiv rcx
                    }
                    BinOp::Mod => {
                        out.extend_from_slice(&[0x48, 0x99, 0x48, 0xF7, 0xF9, 0x48, 0x89, 0xD0])
                    }
                    BinOp::BitAnd => out.extend_from_slice(&[0x48, 0x21, 0xC8]),
                    BinOp::BitOr => out.extend_from_slice(&[0x48, 0x09, 0xC8]),
                    BinOp::BitXor => out.extend_from_slice(&[0x48, 0x31, 0xC8]),
                    BinOp::Shl => out.extend_from_slice(&[0x48, 0xD3, 0xE0]),
                    BinOp::Shr => out.extend_from_slice(&[0x48, 0xD3, 0xF8]),
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        out.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
                        let setcc = match op {
                            BinOp::Eq => 0x94,
                            BinOp::Ne => 0x95,
                            BinOp::Lt => 0x9C,
                            BinOp::Le => 0x9E,
                            BinOp::Gt => 0x9F,
                            BinOp::Ge => 0x9D,
                            _ => unreachable!(),
                        };
                        out.extend_from_slice(&[0x0F, setcc, 0xC0]);
                        out.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                    }
                    BinOp::And | BinOp::Or => unreachable!(),
                }
            }
        },
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            emit_x86_expr(cond, ctx, out, relocs)?;
            out.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            out.extend_from_slice(&[0x0F, 0x84]); // je else_lbl
            let je_patch = out.len();
            out.extend_from_slice(&0i32.to_le_bytes());

            emit_x86_expr(then_branch, ctx, out, relocs)?;
            out.push(0xE9); // jmp end_lbl
            let jmp_patch = out.len();
            out.extend_from_slice(&0i32.to_le_bytes());

            let else_lbl = out.len();
            patch_rel32(out, je_patch, else_lbl);
            emit_x86_expr(else_branch, ctx, out, relocs)?;

            let end_lbl = out.len();
            patch_rel32(out, jmp_patch, end_lbl);
        }
        Expr::MemLoad { index, .. } => {
            emit_x86_expr(index, ctx, out, relocs)?;
            // and eax, 0x1FFFFF
            out.extend_from_slice(&[0x25, 0xFF, 0xFF, 0x1F, 0x00]);
            // mov rdx, BSS_MEM_VADDR
            out.extend_from_slice(&[0x48, 0xBA]);
            out.extend_from_slice(&BSS_MEM_VADDR.to_le_bytes());
            // movsxd rax, dword ptr [rdx + rax*4]
            out.extend_from_slice(&[0x48, 0x63, 0x04, 0x82]);
        }
        Expr::MemStore { index, value, .. } => {
            emit_x86_expr(index, ctx, out, relocs)?;
            out.extend_from_slice(&[0x25, 0xFF, 0xFF, 0x1F, 0x00]); // and eax, 0x1FFFFF
            out.push(0x50); // push rax (masked idx)
            emit_x86_expr(value, ctx, out, relocs)?;
            out.push(0x59); // pop rcx (masked idx)
            out.extend_from_slice(&[0x48, 0xBA]); // mov rdx, BSS_MEM_VADDR
            out.extend_from_slice(&BSS_MEM_VADDR.to_le_bytes());
            // mov dword ptr [rdx + rcx*4], eax
            out.extend_from_slice(&[0x89, 0x04, 0x8A]);
            out.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
        }
        Expr::Perform { args, .. } => {
            for a in args {
                emit_x86_expr(a, ctx, out, relocs)?;
            }
            out.extend_from_slice(&[0x31, 0xC0]);
        }
        Expr::Call { target, args } => {
            for a in args {
                emit_x86_expr(a, ctx, out, relocs)?;
                out.push(0x50); // push arg
            }
            let callee_idx = match target {
                TargetRef::SelfRec => ctx.self_idx,
                TargetRef::Hash(h) => *ctx.hash_to_idx.get(h).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E032_NODE_NOT_FOUND",
                        "strata-elf",
                        format!("Missing callee #{h}"),
                        Span::zero(),
                        "Ingest callee first",
                    )
                })?,
                TargetRef::Named(n) => {
                    return Err(InfimumDiagnostic::new(
                        "E055_UNRESOLVED_CALL",
                        "strata-elf",
                        format!("Unresolved call '{n}'"),
                        Span::zero(),
                        "Canonicalize before ELF emission",
                    ))
                }
            };
            emit_call_sym(out, relocs, SymTarget::NodeFn(callee_idx));
            if !args.is_empty() {
                let pop_bytes = (args.len() * 8) as u32;
                out.extend_from_slice(&[0x48, 0x81, 0xC4]); // add rsp, pop_bytes
                out.extend_from_slice(&pop_bytes.to_le_bytes());
            }
        }
        Expr::Block { stmts, tail } => {
            emit_x86_stmts(stmts, ctx, out, relocs)?;
            emit_x86_expr(tail, ctx, out, relocs)?;
        }
        Expr::TupleLit(_)
        | Expr::TupleField { .. }
        | Expr::SliceLit { .. }
        | Expr::SlicePtr(_)
        | Expr::StrPtr(_)
        | Expr::SliceLen(_) => {
            return Err(InfimumDiagnostic::new(
                "E085_STRATA_UNSUPPORTED_TYPE",
                "strata-elf",
                "Tuples and slices are supported in Strata Wasm (.ifw), interpreter, and Rust codegen (not yet in raw x86_64 ELF)",
                Span::zero(),
                "Use 'infimum compile-wasm' or 'infimum build' for tuple/slice expressions",
            ));
        }
        Expr::BoundsCheck { index, .. } => {
            emit_x86_expr(index, ctx, out, relocs)?;
        }
        Expr::RangeCheck { lo, .. } => {
            emit_x86_expr(lo, ctx, out, relocs)?;
        }
    }
    Ok(())
}

fn emit_x86_start(
    root: &CanonicalNode,
    root_idx: usize,
    hash_to_idx: &BTreeMap<String, usize>,
    code: &mut Vec<u8>,
    relocs: &mut Vec<CallReloc>,
) -> Result<(), InfimumDiagnostic> {
    let f = &root.ast;
    let arity = f.params.len();

    // Save initial stack pointer in rbx (`[rbx] = argc`, `[rbx + 16 + i*8] = argv[i+1]`)
    // mov rbx, rsp; mov rbp, rsp; sub rsp, aligned_slots
    code.extend_from_slice(&[0x48, 0x89, 0xE3]); // mov rbx, rsp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp

    let mut var_disp = BTreeMap::new();
    for (i, p) in f.params.iter().enumerate() {
        let disp = -(((i + 1) * 8) as i32);
        var_disp.insert(p.name.clone(), disp);
    }
    let slots_bytes = (((arity + 2) * 8 + 15) & !15) as u32;
    code.extend_from_slice(&[0x48, 0x81, 0xEC]); // sub rsp, slots_bytes
    code.extend_from_slice(&slots_bytes.to_le_bytes());

    // Check argc == arity + 1: mov rax, [rbx]; cmp rax, arity + 1; jne exit_2
    code.extend_from_slice(&[0x48, 0x8B, 0x03]); // mov rax, [rbx]
    code.extend_from_slice(&[0x48, 0x81, 0xF8]); // cmp rax, imm32
    code.extend_from_slice(&((arity + 1) as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x85]); // jne exit_err
    let jne_argc_patch = code.len();
    code.extend_from_slice(&0i32.to_le_bytes());

    // Parse each CLI argument `argv[i+1]` into `[rbp + disp]`
    for (i, p) in f.params.iter().enumerate() {
        let argv_disp = (16 + i * 8) as u32;
        // mov rdi, [rbx + argv_disp]
        code.extend_from_slice(&[0x48, 0x8B, 0xBB]);
        code.extend_from_slice(&argv_disp.to_le_bytes());
        emit_call_sym(code, relocs, SymTarget::HelperParseArg);
        let slot_disp = *var_disp.get(&p.name).unwrap();
        code.extend_from_slice(&[0x48, 0x89, 0x85]);
        code.extend_from_slice(&slot_disp.to_le_bytes());
    }

    // Evaluate root `req` preconditions on parsed CLI arguments!
    let mut req_ctx = X86FnCtx {
        self_idx: root_idx,
        hash_to_idx,
        var_disp: var_disp.clone(),
        next_local_disp: -((arity * 8) as i32),
    };
    let mut req_fail_patches = Vec::new();
    for req_expr in &f.requires {
        emit_x86_expr(req_expr, &mut req_ctx, code, relocs)?;
        code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
        code.extend_from_slice(&[0x0F, 0x84]); // je exit_err
        req_fail_patches.push(code.len());
        code.extend_from_slice(&0i32.to_le_bytes());
    }

    // Push arguments left-to-right and call root node function
    for p in &f.params {
        let slot_disp = *var_disp.get(&p.name).unwrap();
        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
        code.extend_from_slice(&slot_disp.to_le_bytes());
        code.push(0x50); // push rax
    }
    emit_call_sym(code, relocs, SymTarget::NodeFn(root_idx));

    // Print return value according to root return type
    match f.ret_type {
        Type::I64 | Type::Fixed => emit_call_sym(code, relocs, SymTarget::HelperPrintI64),
        Type::Bool => emit_call_sym(code, relocs, SymTarget::HelperPrintBool),
        Type::Unit | Type::Str | Type::Secret(_) | Type::Tuple(_) | Type::Slice(_) => {}
    }

    // sys_exit(0): mov eax, 60; xor edi, edi; syscall
    code.extend_from_slice(&[0xB8, 60, 0, 0, 0, 0x31, 0xFF, 0x0F, 0x05]);

    // exit_err label: sys_exit(2)
    let exit_err_off = code.len();
    patch_rel32(code, jne_argc_patch, exit_err_off);
    for p_off in req_fail_patches {
        patch_rel32(code, p_off, exit_err_off);
    }
    let _ = Param {
        name: String::new(),
        ty: Type::Unit,
    };
    code.extend_from_slice(&[0xB8, 60, 0, 0, 0, 0xBF, 2, 0, 0, 0, 0x0F, 0x05]);
    Ok(())
}

/// Parses C-string in `rdi` -> signed `i64` in `rax` (also accepts `"true"` -> 1, `"false"` -> 0).
fn emit_helper_parse_i64(code: &mut Vec<u8>) {
    // Check if first char is 't' (0x74) -> return 1; 'f' (0x66) -> return 0
    code.extend_from_slice(&[
        0x0F, 0xB6, 0x0F, // movzx ecx, byte ptr [rdi]
        0x83, 0xF9, b't', // cmp ecx, 't'
        0x75, 0x06, // jne +6
        0xB8, 1, 0, 0, 0, // mov eax, 1
        0xC3, // ret
        0x83, 0xF9, b'f', // cmp ecx, 'f'
        0x75, 0x03, // jne +3
        0x31, 0xC0, // xor eax, eax
        0xC3, // ret
        // Standard signed decimal parser:
        0x31, 0xC0, // xor eax, eax (acc = 0)
        0x41, 0xB8, 1, 0, 0, 0, // mov r8d, 1 (sign = +1)
        0x83, 0xF9, b'-', // cmp ecx, '-'
        0x75, 0x0A, // jne loop_start (+10)
        0x49, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, // mov r8, -1
        0x48, 0xFF, 0xC7, // inc rdi
        // loop_start:
        0x0F, 0xB6, 0x0F, // movzx ecx, byte ptr [rdi]
        0x85, 0xC9, // test ecx, ecx
        0x74, 0x14, // jz done (+20)
        0x83, 0xE9, b'0', // sub ecx, '0'
        0x83, 0xF9, 9, // cmp ecx, 9
        0x77, 0x0C, // ja done (+12)
        0x48, 0x6B, 0xC0, 10, // imul rax, rax, 10
        0x48, 0x01, 0xC8, // add rax, rcx
        0x48, 0xFF, 0xC7, // inc rdi
        0xEB, 0xE5, // jmp loop_start (-27)
        // done:
        0x49, 0x0F, 0xAF, 0xC0, // imul rax, r8
        0xC3, // ret
    ]);
}

/// Prints signed `i64` in `rax` followed by `\n` to stdout via Linux `sys_write`.
fn emit_helper_print_i64(code: &mut Vec<u8>) {
    code.extend_from_slice(&[
        0x55, // push rbp
        0x48, 0x89, 0xE5, // mov rbp, rsp
        0x48, 0x83, 0xEC, 32, // sub rsp, 32
        0x48, 0x8D, 0x7D, 0xFF, // lea rdi, [rbp - 1]
        0xC6, 0x07, b'\n', // mov byte ptr [rdi], '\n'
        0x41, 0xB9, 0, 0, 0, 0, // mov r9d, 0 (is_neg = 0)
        0x48, 0x85, 0xC0, // test rax, rax
        0x7D, 0x09, // jge conv_loop (+9)
        0x41, 0xB9, 1, 0, 0, 0, // mov r9d, 1
        0x48, 0xF7, 0xD8, // neg rax
        // conv_loop:
        0xB9, 10, 0, 0, 0, // mov ecx, 10
        0x31, 0xD2, // xor edx, edx
        0x48, 0xF7, 0xF1, // div rcx (unsigned rax / 10, rem in rdx)
        0x80, 0xC2, b'0', // add dl, '0'
        0x48, 0xFF, 0xCF, // dec rdi
        0x88, 0x17, // mov [rdi], dl
        0x48, 0x85, 0xC0, // test rax, rax
        0x75, 0xE9, // jnz conv_loop (-23)
        0x45, 0x85, 0xC9, // test r9d, r9d
        0x74, 0x06, // jz write_out (+6)
        0x48, 0xFF, 0xCF, // dec rdi
        0xC6, 0x07, b'-', // mov byte ptr [rdi], '-'
        // write_out:
        0x48, 0x89, 0xEE, // mov rsi, rbp
        0x48, 0x29, 0xFE, // sub rsi, rdi (len = rbp - rdi)
        0x48, 0x89, 0xF2, // mov rdx, rsi (count)
        0x48, 0x89, 0xFE, // mov rsi, rdi (buf)
        0xBF, 1, 0, 0, 0, // mov edi, 1 (stdout)
        0xB8, 1, 0, 0, 0, // mov eax, 1 (sys_write)
        0x0F, 0x05, // syscall
        0x48, 0x89, 0xEC, // mov rsp, rbp
        0x5D, // pop rbp
        0xC3, // ret
    ]);
}

/// Prints `"true\n"` or `"false\n"` based on `rax != 0`.
fn emit_helper_print_bool(code: &mut Vec<u8>) {
    code.extend_from_slice(&[
        0x55,
        0x48, 0x89, 0xE5,
        0x48, 0x83, 0xEC, 16,
        0x48, 0x85, 0xC0, // test rax, rax
        0x74, 0x16, // jz print_false (+22)
        // "true\n" (5 bytes)
        0xC7, 0x45, 0xF8, b't', b'r', b'u', b'e',
        0xC6, 0x45, 0xFC, b'\n',
        0x48, 0x8D, 0x75, 0xF8, // lea rsi, [rbp - 8]
        0xBA, 5, 0, 0, 0, // mov edx, 5
        0xEB, 0x15, // jmp do_sys (+21)
        // print_false: "false\n" (6 bytes)
        0xC7, 0x45, 0xF8, b'f', b'a', b'l', b's',
        0x66, 0xC7, 0x45, 0xFC, b'e', b'\n',
        0x48, 0x8D, 0x75, 0xF8, // lea rsi, [rbp - 8]
        0xBA, 6, 0, 0, 0, // mov edx, 6
        // do_sys:
        0xBF, 1, 0, 0, 0, // mov edi, 1
        0xB8, 1, 0, 0, 0, // mov eax, 1
        0x0F, 0x05, // syscall
        0x48, 0x89, 0xEC,
        0x5D,
        0xC3,
    ]);
}

fn emit_mov_rax_imm64(out: &mut Vec<u8>, val: i64) {
    if val == 0 {
        out.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
    } else if (0..=i32::MAX as i64).contains(&val) {
        out.push(0xB8); // mov eax, imm32 (zero-extends to rax)
        out.extend_from_slice(&(val as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&[0x48, 0xB8]); // movabs rax, imm64
        out.extend_from_slice(&val.to_le_bytes());
    }
}

fn emit_call_sym(out: &mut Vec<u8>, relocs: &mut Vec<CallReloc>, target: SymTarget) {
    out.push(0xE8);
    let disp_off = out.len();
    out.extend_from_slice(&0i32.to_le_bytes());
    relocs.push(CallReloc {
        disp32_offset: disp_off,
        target,
    });
}

fn patch_rel32(buf: &mut [u8], disp_pos: usize, target_pos: usize) {
    let next_rip = disp_pos + 4;
    let disp = (target_pos as i32) - (next_rip as i32);
    buf[disp_pos..disp_pos + 4].copy_from_slice(&disp.to_le_bytes());
}

fn write_elf64_headers(out: &mut Vec<u8>, entry_vaddr: u64, file_size: u64) {
    // 64-byte Elf64_Ehdr
    out.extend_from_slice(&[
        0x7F, b'E', b'L', b'F', // EI_MAG
        2,    // EI_CLASS = ELFCLASS64
        1,    // EI_DATA = ELFDATA2LSB
        1,    // EI_VERSION = EV_CURRENT
        0,    // EI_OSABI = ELFOSABI_NONE
        0, 0, 0, 0, 0, 0, 0, 0, // padding
    ]);
    out.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    out.extend_from_slice(&62u16.to_le_bytes()); // e_machine = EM_X86_64
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&entry_vaddr.to_le_bytes()); // e_entry
    out.extend_from_slice(&(EHDR_SIZE as u64).to_le_bytes()); // e_phoff = 64
    out.extend_from_slice(&0u64.to_le_bytes()); // e_shoff = 0
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes()); // e_ehsize = 64
    out.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes()); // e_phentsize = 56
    out.extend_from_slice(&2u16.to_le_bytes()); // e_phnum = 2
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    // Phdr[0]: PT_LOAD RX code segment
    out.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    out.extend_from_slice(&5u32.to_le_bytes()); // p_flags = PF_R | PF_X
    out.extend_from_slice(&0u64.to_le_bytes()); // p_offset = 0
    out.extend_from_slice(&ELF_BASE_VADDR.to_le_bytes()); // p_vaddr
    out.extend_from_slice(&ELF_BASE_VADDR.to_le_bytes()); // p_paddr
    out.extend_from_slice(&file_size.to_le_bytes()); // p_filesz
    out.extend_from_slice(&file_size.to_le_bytes()); // p_memsz
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align

    // Phdr[1]: PT_LOAD RW 4 MiB .bss linear memory segment
    out.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    out.extend_from_slice(&6u32.to_le_bytes()); // p_flags = PF_R | PF_W
    out.extend_from_slice(&0u64.to_le_bytes()); // p_offset = 0
    out.extend_from_slice(&BSS_MEM_VADDR.to_le_bytes()); // p_vaddr
    out.extend_from_slice(&BSS_MEM_VADDR.to_le_bytes()); // p_paddr
    out.extend_from_slice(&0u64.to_le_bytes()); // p_filesz = 0 (zero disk footprint!)
    out.extend_from_slice(&BSS_MEM_BYTES.to_le_bytes()); // p_memsz = 4 MiB
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align
}
