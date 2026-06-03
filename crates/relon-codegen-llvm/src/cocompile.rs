//! Stage 1.B — LTO co-compile backbone (closed-world `CallNative`).
//!
//! GraalVM-style closed-world native dispatch: when the full host-fn
//! set is known at emit time (the `build.rs` / `emit_object` path,
//! *not* the open-world MCJIT / `from_source` path), the host Rust is
//! compiled to LLVM bitcode, linked into the *same* LLVM module as the
//! emitted Relon code, and run through LTO / inline so every
//! `Op::CallNative` collapses from a dynamic
//! `relon_llvm_call_native` helper hop into an inlined unit-internal
//! call — exactly what `relon-codegen-cranelift`'s *static*
//! `cap_lookup -> fn_ptr` arm does, but resolved fully at link time.
//!
//! ## Toolchain spike (the highest risk, validated first)
//!
//! The host bitcode is produced by **rustc's bundled LLVM**, while the
//! Relon module is built by the **system LLVM 18.1.3** (`inkwell`'s
//! `llvm18-1` feature). On this host rustc ships LLVM 22 — a 4-major
//! skew. Raw `rustc --emit=llvm-bc` embeds a ThinLTO module-summary
//! whose version (12) the LLVM-18 bitcode reader rejects
//! (`Invalid summary version 12`), so `link_in_module` cannot consume
//! it directly.
//!
//! The bridge that works: emit **textual** IR (`rustc --emit=llvm-ir`)
//! and re-assemble it with the system `llvm-as-18`. LLVM's textual IR
//! is forward-compatible enough across this skew that the 18.1.3
//! assembler accepts rustc-22's `.ll`, yielding LLVM-18 bitcode the
//! inkwell module links cleanly. The host fn is then marked
//! `alwaysinline` so the O3 pipeline fully inlines it (the rustc
//! default attribute set — `probe-stack` / `target-cpu` — otherwise
//! makes the cost-model decline even a trivial single-use call).
//!
//! Everything here is gated behind explicit calls; the open-world
//! MCJIT path (`evaluator.rs`) is untouched and remains the default.

use std::path::Path;
use std::process::Command;

use inkwell::attributes::AttributeLoc;
use inkwell::context::Context;
use inkwell::execution_engine::ExecutionEngine;
use inkwell::module::Module as LlvmModule;
use inkwell::targets::{
    CodeModel, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;

use crate::codegen::{emit_module_funcs_closed_world, ConstPool, ENTRY_SYMBOL};
use crate::error::LlvmError;

/// `extern "C" fn(i64, i64, ...) -> i64` raw entry signature for a
/// closed-world legacy-i64 entry that JIT-runs without the buffer
/// arena handshake. Arity is fixed at the call site (`run_i64`).
type EntryArity1 = unsafe extern "C" fn(i64) -> i64;

/// Result of a closed-world co-compile: the post-O3 module IR text
/// (for inline-count assertions) plus a JIT execution engine kept
/// alive alongside its leaked `Context` so callers can run the entry.
pub struct CocompiledModule {
    /// The post-O3 module IR text. Callers assert against this:
    /// zero `call @relon_llvm_call_native` (open-world helper never
    /// emitted) and zero residual `call @<host_symbol>` (the linked
    /// host fn was inlined).
    pub ir_after_opt: String,
    /// The pre-link / pre-opt module IR text — useful when a test
    /// wants to confirm the direct `call @<host_symbol>` was the shape
    /// emitted before inlining erased it.
    pub ir_before_link: String,
    // The engine borrows the module which borrows the leaked Context.
    // Kept last so it drops first; the Context leak means the
    // `'static` lifetime is sound for the engine's lifetime.
    engine: ExecutionEngine<'static>,
}

impl CocompiledModule {
    /// Run the closed-world legacy-i64 entry with a single i64 arg.
    ///
    /// # Safety
    /// The JIT'd entry is a raw `extern "C" fn(i64) -> i64`; the engine
    /// owns the code. The caller must have built a single-arg legacy
    /// entry (the spike fixture does).
    pub fn run_i64(&self, arg: i64) -> Result<i64, LlvmError> {
        let f: inkwell::execution_engine::JitFunction<'_, EntryArity1> = unsafe {
            self.engine
                .get_function(ENTRY_SYMBOL)
                .map_err(|e| LlvmError::Codegen(format!("cocompile: entry lookup: {e}")))?
        };
        Ok(unsafe { f.call(arg) })
    }
}

/// Co-compile a closed-world legacy-i64 IR module against a host shim
/// crate.
///
/// 1. emit the Relon module with `WorldMode::ClosedWorld` so
///    `Op::CallNative` lowers to a direct `call @<host_symbol>`;
/// 2. compile `host_shim_src` (a `#[no_mangle] extern "C"` host fn
///    crate) to LLVM-18 bitcode via the textual-IR bridge;
/// 3. `link_in_module` the host bitcode into the Relon module;
/// 4. mark every linked host fn `alwaysinline`;
/// 5. run the same `default<O3>` pipeline the MCJIT path uses, then
///    JIT the module.
///
/// `ir` must have a legacy-i64 `(i64) -> i64` entry whose body carries
/// the `Op::CallNative` and an `imports` table naming the host fn.
pub fn cocompile_legacy_i64(
    ir: &relon_ir::ir::Module,
    host_shim_src: &str,
) -> Result<CocompiledModule, LlvmError> {
    let entry_idx = ir
        .entry_func_index
        .ok_or_else(|| LlvmError::Codegen("cocompile: IR module has no entry function".into()))?;
    let entry = &ir.funcs[entry_idx];

    // Leak the Context so the engine can hold a `'static` borrow (same
    // pattern as `LlvmAotEvaluator`).
    let ctx_box: Box<Context> = Box::new(Context::create());
    // SAFETY: `ctx_box` lives on the heap and is never freed before the
    // returned engine; we intentionally leak it.
    let ctx: &'static Context = unsafe { &*(Box::into_raw(ctx_box) as *const Context) };

    let module = ctx.create_module("relon_llvm_cocompile");

    let const_pool = ConstPool::from_module(ir)?;
    let helpers: Vec<&relon_ir::ir::Func> = ir
        .funcs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != entry_idx)
        .map(|(_, f)| f)
        .collect();
    let helper_ir_indices: Vec<u32> = ir
        .funcs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != entry_idx)
        .map(|(i, _)| i as u32)
        .collect();

    // Emit with the closed-world flag: `Op::CallNative` -> direct
    // `call @<host_symbol>`, host fns pre-declared as `extern`.
    emit_module_funcs_closed_world(
        ctx,
        &module,
        entry,
        /*buffer_return_size=*/ 0,
        &const_pool,
        &helpers,
        Some(&helper_ir_indices),
        /*lambdas=*/ &[],
        /*closure_table=*/ &[],
        &ir.imports,
    )?;

    let ir_before_link = module.print_to_string().to_string();

    // Compile + link the host bitcode for every imported host fn. One
    // shim crate carries them all; we link it once.
    let host_bc = compile_host_shim_to_bitcode(host_shim_src)?;
    let host_module = LlvmModule::parse_bitcode_from_path(&host_bc, ctx)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: parse host bitcode: {e}")))?;
    module
        .link_in_module(host_module)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: link_in_module: {e}")))?;

    // Force-inline every host fn the import table named. rustc's
    // default attribute set makes the O3 inliner decline a trivial
    // single-use call, so we stamp `alwaysinline` explicitly — this is
    // the trusted-host-fn inline the LTO co-compile relies on.
    let always_inline = ctx.create_enum_attribute(
        inkwell::attributes::Attribute::get_named_enum_kind_id("alwaysinline"),
        0,
    );
    for import in &ir.imports {
        if let Some(host_fn) = module.get_function(&import.name) {
            // Only a fn with a body can be inlined; an unresolved decl
            // would mean the shim didn't define it (surface as an error
            // later when the JIT can't resolve the symbol).
            if host_fn.get_first_basic_block().is_some() {
                host_fn.add_attribute(AttributeLoc::Function, always_inline);
            }
        }
    }

    run_default_o3_pipeline(&module)?;

    let ir_after_opt = module.print_to_string().to_string();

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::Aggressive)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: create JIT engine: {e}")))?;

    Ok(CocompiledModule {
        ir_after_opt,
        ir_before_link,
        engine,
    })
}

/// Compile a host shim Rust source to LLVM-18-compatible bitcode.
///
/// The skew bridge (see module docs): emit textual IR with rustc, then
/// re-assemble it with the system `llvm-as-18`. The returned path is a
/// `.bc` the inkwell (LLVM-18) module can `parse_bitcode_from_path`.
fn compile_host_shim_to_bitcode(host_shim_src: &str) -> Result<std::path::PathBuf, LlvmError> {
    let dir = std::env::temp_dir().join(format!("relon_cocompile_{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: mkdir tmp: {e}")))?;
    let rs_path = dir.join("host_shim.rs");
    let ll_path = dir.join("host_shim.ll");
    let bc_path = dir.join("host_shim.bc");
    std::fs::write(&rs_path, host_shim_src)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: write shim: {e}")))?;

    // 1. rustc --emit=llvm-ir (textual): decouples from rustc's bitcode
    //    binary format / ThinLTO summary version.
    let rustc = Command::new("rustc")
        .args([
            "--emit=llvm-ir",
            "--crate-type=cdylib",
            "-O",
            rs_path.to_str().unwrap(),
            "-o",
            ll_path.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| LlvmError::Codegen(format!("cocompile: spawn rustc: {e}")))?;
    if !rustc.status.success() {
        return Err(LlvmError::Codegen(format!(
            "cocompile: rustc --emit=llvm-ir failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        )));
    }

    // 2. llvm-as-18 (system LLVM): assemble the textual IR into
    //    LLVM-18 bitcode. Probe a couple of common binary names.
    assemble_with_system_llvm(&ll_path, &bc_path)?;

    Ok(bc_path)
}

/// Assemble textual IR into bitcode with the system LLVM-18 assembler.
/// Tries the versioned `llvm-as-18` first (matches the inkwell
/// `llvm18-1` feature), then the unversioned `llvm-as`.
fn assemble_with_system_llvm(ll: &Path, bc: &Path) -> Result<(), LlvmError> {
    let candidates = ["llvm-as-18", "llvm-as"];
    let mut last_err = String::new();
    for tool in candidates {
        match Command::new(tool)
            .arg(ll.to_str().unwrap())
            .arg("-o")
            .arg(bc.to_str().unwrap())
            .output()
        {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                last_err = format!("{tool}: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Err(e) => last_err = format!("{tool}: spawn failed: {e}"),
        }
    }
    Err(LlvmError::Codegen(format!(
        "cocompile: no working system llvm-as (need LLVM-18 to match the inkwell feature); \
         last error: {last_err}"
    )))
}

/// Run the same `default<O3>` middle-end pipeline the MCJIT path uses
/// (`evaluator.rs::run_default_o3_pipeline`). Re-implemented here
/// because that one is private to `evaluator.rs`; the knobs are
/// identical so the optimized shape matches.
fn run_default_o3_pipeline(module: &LlvmModule<'_>) -> Result<(), LlvmError> {
    Target::initialize_native(&InitializationConfig::default())
        .map_err(|e| LlvmError::Codegen(format!("cocompile: initialize_native: {e}")))?;
    let triple_str = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple_str)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: target from_triple: {e}")))?;
    let cpu = TargetMachine::get_host_cpu_name();
    let features = TargetMachine::get_host_cpu_features();
    let triple = TargetTriple::create(
        triple_str
            .as_str()
            .to_str()
            .map_err(|e| LlvmError::Codegen(format!("cocompile: triple utf8: {e}")))?,
    );
    let machine = target
        .create_target_machine(
            &triple,
            cpu.to_str().unwrap_or(""),
            features.to_str().unwrap_or(""),
            OptimizationLevel::Aggressive,
            RelocMode::Default,
            CodeModel::JITDefault,
        )
        .ok_or_else(|| LlvmError::Codegen("cocompile: create_target_machine null".into()))?;
    let opts = inkwell::passes::PassBuilderOptions::create();
    module
        .run_passes("default<O3>", &machine, opts)
        .map_err(|e| LlvmError::Codegen(format!("cocompile: run_passes O3: {e}")))?;
    Ok(())
}
