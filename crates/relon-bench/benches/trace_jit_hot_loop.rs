//! v6-ε bench rewrite (2026-05-19): honest per-iter cost of a hot
//! integer-accumulation loop running INSIDE a single trace, plus
//! diagnostic rows.
//!
//! ## Background — why the v6-γ / v6-δ shape was misleading
//!
//! The previous version of this bench (v6-γ M5 → v6-δ M2-C → v6-ε-0-C
//! → v6-ε-0-A) used the following per-iter shape:
//!
//! ```text
//! Rust caller loop:
//!     for i in 0..N {
//!         args[0] = acc; args[1] = i;
//!         trace_fn(&mut ctx, args.as_ptr())   // <-- one extern-C call PER ITER
//!         acc = ctx.result_slot as i64;
//!     }
//! ```
//!
//! That shape pinned a stable 9.5 ns/iter floor across **every**
//! variant we threw at it: trampoline call, `CallConv::Tail`,
//! `CallConv::SystemV`, IC-slot dispatch, at-call-site IR inlining.
//! v6-δ M2-C, v6-ε-0-C, and v6-ε-0-A together form a three-attempt
//! falsification chain proving the 9.5 ns is **not** any of:
//!
//! - the inner `call trace_fn` (ε-0-A inlined the body — Δ = 0.00 ns)
//! - the entry-fn prologue / epilogue (ε-0-C swapped CallConv::Tail
//!   vs SystemV — Δ = 0.01 ns)
//! - the IC dispatch layer (M2-C cached pointer — Δ = 0.01 ns)
//!
//! The 9.5 ns floor IS the **Rust → cranelift-JIT extern-C call
//! boundary** the bench harness pays every iter: args repack, `call
//! rax`, return-path `cmp eax,0`, `load ctx.result_slot`, loop
//! increment. None of those costs are intrinsic to "what a trace JIT
//! does"; they only exist because the bench harness drove the JIT
//! one iter at a time.
//!
//! ## What a real Relon hot loop looks like
//!
//! A real Relon hot loop runs the entire `for i in 0..N { acc += i }`
//! INSIDE the JIT-compiled trace body. The Rust caller invokes the
//! trace **once**, the trace runs all N iters under cranelift's
//! regalloc / scheduling, then returns. Per-iter cost is whatever
//! cranelift compiled for `acc += i` plus loop control — typically
//! 1-3 ns on LuaJIT trace-tier hardware.
//!
//! ## Row anatomy in this rewrite
//!
//! New rows ("loop-INSIDE" methodology — the honest hot-loop cost):
//!
//! - `tree_walk_loop` — Tree-walker runs `#main(Int n): sum(range(n))`.
//!   One Rust→tree-walker call total; per-iter cost = total_time /
//!   `HOT_LOOP_N`. Captures the AST interpreter dispatch tax per
//!   per-element on a real loop primitive (`_list_reduce`).
//! - `cranelift_aot_loop` — Cranelift-AOT compiles a Relon IR
//!   `Op::Loop` that sums `1..=n`. One Rust→cranelift call total;
//!   per-iter cost = total / `HOT_LOOP_N`. This is the realistic
//!   "ahead-of-time compiled function with a loop body" baseline.
//! - `trace_jit_loop` — **The real test**. A hand-built cranelift
//!   JIT function whose body IS the full N-iter
//!   `for i in 0..n { acc += i }` with overflow guard, packaged
//!   behind [`relon_trace_abi::TRACE_ENTRY_SIG`]. One Rust→JIT call
//!   total; per-iter cost = total / `HOT_LOOP_N`. This is what a
//!   trace JIT would produce after compiling a hot Relon loop into a
//!   single trace — bypassing the trace **recorder** (which doesn't
//!   yet record backward branches end-to-end) but exercising the JIT
//!   path the recorder would feed into.
//! - `rust_native_loop` — Pure Rust `for i in 0..n` accumulator with
//!   `checked_add`. Theoretical floor; the compiler can constant-fold
//!   when the input is constant so it's wrapped in `black_box` to
//!   keep it honest.
//!
//! Legacy rows kept for regression context, **relabelled as
//! "dispatch-boundary" rows**: these measure the Rust→JIT call
//! boundary cost per dispatch, not hot-loop per-iter cost.
//!
//! - `dispatch_trampoline` — historical `trace_jit_warm`; v6-γ M5
//!   shape, recorder-driven install, default ABI.
//! - `dispatch_ic` — historical `trace_jit_warm_ic`; v6-δ M2-C IC-slot
//!   cached pointer.
//! - `dispatch_tail` — historical `trace_jit_warm_tail`; v6-ε-0-C
//!   `CallConv::Tail` install path.
//! - `dispatch_sysv` — historical `trace_jit_warm_sysv`; v6-ε-0-C
//!   `CallConv::SystemV` install path.
//! - `dispatch_inline` — historical `trace_jit_warm_inline`; v6-ε-0-A
//!   at-call-site IR-inline install path.
//!
//! All five dispatch rows are expected to land in the same 9-10 ns
//! band; the row spread is the boundary cost noise floor, not any
//! optimisation's value-add. They exist to keep the M2-C / ε-0-C /
//! ε-0-A falsification chain audit-able in one bench output.
//!
//! ## Methodology
//!
//! - `HOT_LOOP_N = 1_000_000` for every row.
//! - For the four loop-INSIDE rows: one criterion iteration drives ONE
//!   invocation of the loop fn (which itself runs `HOT_LOOP_N` iters).
//!   `Throughput::Elements(HOT_LOOP_N)` makes criterion print per-iter
//!   numbers directly.
//! - For the five dispatch-boundary rows: one criterion iteration drives
//!   a Rust-side `for` loop that calls the trace fn `HOT_LOOP_N` times.
//!   Each call's body is the 4-op `LocalGet+LocalGet+Add+Return` shape
//!   ε-0-A pinned.
//!
//! ## Recorder gap (option (a) per task brief)
//!
//! The trace recorder today emits straight-line `LocalGet+Add+Return`
//! traces; it does **not** yet capture an `Op::Loop` body with a
//! backward branch end-to-end. The emitter side has `MarkLoopHead` /
//! `MarkLoopBack` lowering that compiles correctly, but the recorder
//! never inserts those markers. We choose **option (a)** from the
//! brief: bypass the recorder and hand-build the trace JIT function
//! that includes the loop. The `trace_jit_loop` row's machine-code
//! shape is the same shape a fully-extended recorder would produce
//! once it knows how to record a loop trace; the JIT path is exercised
//! identically. The recorder gap is documented as a follow-up in the
//! bench-rewrite report.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, AbiParam, BlockArg, InstBuilder, MemFlags, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_codegen_native::{
    clear_recording, compile_inline_host_fn, global_trace_jit_state, register_recording,
    register_trace_runtime_symbols, trace_install::TraceJitState, CraneliftAotEvaluator,
    RecordingRegistration, SandboxConfig, TraceIcSlot, MAX_FN_ID,
};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::{parse_document, TokenRange};
use relon_trace_abi::{TraceContext, TraceEntryStatus};

/// Iteration count for every row's hot loop. Criterion's
/// `Throughput::Elements(HOT_LOOP_N)` makes the per-iter cost
/// surface directly in the report.
const HOT_LOOP_N: u64 = 1_000_000;

/// Tree-walker hot loops are µs/iter class; running them at
/// `HOT_LOOP_N = 1M` would blow up the bench wall-clock (single
/// invocation ≈ seconds), so the `tree_walk_loop` row drops to a
/// smaller N and reports per-iter cost via a per-row
/// `Throughput::Elements(TREE_WALK_LOOP_N)` adjustment.
const TREE_WALK_LOOP_N: u64 = 10_000;

// =====================================================================
// =====  loop-INSIDE rows  ============================================
// =====================================================================

/// Build an IR body that computes `sum(1..=n)` via an `Op::Loop` with
/// an explicit `BrIf` exit, mirroring the working pattern from
/// `relon-codegen-native/tests/control_flow_extended.rs`. The loop
/// runs entirely on the wasm operand-stack carrier; the cranelift-AOT
/// backend lowers the back-edge into a normal cranelift loop.
///
/// The body intentionally **does not** include an overflow check on
/// every iter: Relon's `Add(I64)` is wrapping at the IR level, and the
/// AOT backend matches that. Comparable to the `trace_jit_loop` row
/// below (whose hand-built cranelift fn includes `sadd_overflow` for
/// guard-exposure parity with what a real trace would emit).
fn sum_loop_ir() -> IrModule {
    fn t(op: Op) -> TaggedOp {
        TaggedOp {
            op,
            range: TokenRange::default(),
        }
    }
    const I: u32 = 0;
    const ACC: u32 = 1;
    let body = vec![
        // i = 1
        t(Op::ConstI64(1)),
        t(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        // seed yield = 0
        t(Op::ConstI64(0)),
        t(Op::Block {
            result_ty: Some(IrType::I64),
            body: vec![t(Op::Loop {
                result_ty: Some(IrType::I64),
                body: vec![
                    // acc = block_param
                    t(Op::LetSet {
                        idx: ACC,
                        ty: IrType::I64,
                    }),
                    // if i > n -> exit
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::LocalGet(0)),
                    t(Op::Gt(IrType::I64)),
                    t(Op::If {
                        result_ty: IrType::I64,
                        then_body: vec![
                            t(Op::LetGet {
                                idx: ACC,
                                ty: IrType::I64,
                            }),
                            t(Op::Br { label_depth: 2 }),
                            t(Op::ConstI64(0)),
                        ],
                        else_body: vec![t(Op::ConstI64(0))],
                    }),
                    // drop If-yield placeholder
                    t(Op::LetSet {
                        idx: 2,
                        ty: IrType::I64,
                    }),
                    // acc' = acc + i
                    t(Op::LetGet {
                        idx: ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::Add(IrType::I64)),
                    // i = i + 1
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    // top-of-stack = acc'; back-edge.
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        t(Op::Return),
    ];
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

/// Pre-warmed cranelift-AOT evaluator for the `sum 1..=n` loop body
/// built by [`sum_loop_ir`]. One Rust→JIT invoke runs all N iters
/// inside cranelift's compiled loop.
fn build_cranelift_aot_loop() -> CraneliftAotEvaluator {
    CraneliftAotEvaluator::from_ir_direct(
        sum_loop_ir(),
        SandboxConfig::default(),
        vec!["n".to_string()],
    )
    .expect("cranelift sum-loop compile")
}

/// Owning wrapper around a hand-built cranelift JIT module whose only
/// exported function runs the full `for i in 1..=n { acc += i }` loop
/// internally and returns through [`relon_trace_abi::TRACE_ENTRY_SIG`].
///
/// This is the **honest** trace-JIT hot-loop measurement: bypass the
/// trace recorder (which can't yet record a backward branch in a
/// trace), but emit cranelift IR with the exact shape a fully-recorded
/// trace would produce — `LocalGet` for `n`, an init block, a header
/// block with the exit `brif`, a body block with `sadd_overflow` + a
/// guard, and a back-edge.
///
/// The JIT module owns its memory mapping; drop order ensures the
/// entry pointer stays valid as long as the wrapper is alive.
pub struct TraceJitLoopFn {
    entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32,
    _module: JITModule,
}

// SAFETY: same contract as JITedTraceFn — entry pointer's lifetime is
// tied to `_module`; cross-thread share is safe so long as callers
// respect TRACE_ENTRY_SIG.
unsafe impl Send for TraceJitLoopFn {}
unsafe impl Sync for TraceJitLoopFn {}

impl TraceJitLoopFn {
    /// # Safety
    ///
    /// Caller must keep `self` alive for the duration of the call;
    /// `ctx` must be exclusive; `args` must point to a slot[0] = `n`
    /// u64 (the loop bound, treated as i64 inside the trace).
    pub unsafe fn invoke(&self, ctx: *mut TraceContext, args: *const u64) -> i32 {
        (self.entry)(ctx, args)
    }
}

/// Build a cranelift JIT module containing one exported function:
///
/// ```text
/// fn loop_step(ctx: *mut TraceContext, args: *const u64) -> i32 {
///     let n: i64 = *args.add(0);
///     let mut acc: i64 = 0;
///     let mut i: i64 = 1;
///     loop {
///         if i > n { break }
///         let (sum, of) = sadd_overflow(acc, i);
///         if of { goto deopt }
///         acc = sum;
///         i = i + 1;
///     }
///     ctx.result_slot = acc;
///     return 0   // Success
/// deopt:
///     // call ctx.host_hooks.save_deopt(ctx, 0, 0)
///     return 1   // GuardFailed
/// }
/// ```
///
/// Block layout mirrors what a trace-JIT-compiled hot loop would look
/// like once the recorder learns to record loops. We deliberately
/// include `sadd_overflow` + guard so the per-iter cycle count is
/// realistic — a real Relon `Add(I64)` body trace would carry the same
/// `ArithOverflow` guard the v6-δ M1 emitter already lowers.
fn build_trace_jit_loop_fn() -> TraceJitLoopFn {
    // ---- ISA + flag setup mirrors the trampoline path
    // (build_trace_jit_module in trace_install.rs) so the codegen
    // tunings — opt_level=speed, probestack off, frame_pointer off —
    // are identical between the dispatch-boundary rows and this row.
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "false")
        .expect("flag is_pic must accept false");
    flag_builder
        .set("opt_level", "speed")
        .expect("flag opt_level must accept speed");
    flag_builder
        .set("enable_verifier", "true")
        .expect("flag enable_verifier must accept true");
    let _ = flag_builder.set("enable_probestack", "false");
    let _ = flag_builder.set("preserve_frame_pointers", "false");
    let flags = settings::Flags::new(flag_builder);
    let isa_builder = cranelift_native::builder().expect("cranelift-native builder");
    let isa = isa_builder.finish(flags).expect("isa finish");

    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    register_trace_runtime_symbols(&mut builder);
    let mut module = JITModule::new(builder);
    let pointer_ty = module.target_config().pointer_type();

    // Pre-declare save_deopt so the deopt arm has a callable extern.
    let mut save_deopt_sig = Signature::new(CallConv::SystemV);
    save_deopt_sig.params.push(AbiParam::new(pointer_ty));
    save_deopt_sig.params.push(AbiParam::new(I32));
    save_deopt_sig.params.push(AbiParam::new(I64));
    let save_deopt_id = module
        .declare_function("__relon_trace_save_deopt", Linkage::Import, &save_deopt_sig)
        .expect("declare save_deopt");

    // Entry signature: same as TRACE_ENTRY_SIG.
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I32));

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, save_deopt_id.as_u32() + 1),
        sig.clone(),
    );

    // Import save_deopt as a callable FuncRef inside the fn body.
    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig.clone());
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id.as_u32()));
    let save_deopt_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(save_deopt_name),
        signature: save_deopt_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);

    // Entry block: load `n` from args, seed acc=0, i=1, jump to header.
    let entry_block = fb.create_block();
    fb.append_block_params_for_function_params(entry_block);
    let trace_ctx = fb.block_params(entry_block)[0];
    let args_ptr = fb.block_params(entry_block)[1];
    fb.switch_to_block(entry_block);
    fb.seal_block(entry_block);

    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let acc_seed = fb.ins().iconst(I64, 0);
    let i_seed = fb.ins().iconst(I64, 1);

    // Header block carries (acc, i) as block params.
    let header_block = fb.create_block();
    fb.append_block_param(header_block, I64);
    fb.append_block_param(header_block, I64);

    let body_block = fb.create_block();
    let exit_block = fb.create_block();
    let deopt_block = fb.create_block();

    fb.ins().jump(
        header_block,
        &[BlockArg::Value(acc_seed), BlockArg::Value(i_seed)],
    );

    // Header: if i > n -> exit; else -> body.
    fb.switch_to_block(header_block);
    let acc_p = fb.block_params(header_block)[0];
    let i_p = fb.block_params(header_block)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThanOrEqual, i_p, n_val);
    let empty: [BlockArg; 0] = [];
    fb.ins()
        .brif(cont, body_block, empty.iter(), exit_block, empty.iter());

    // Body: (sum, of) = sadd_overflow(acc, i); if of -> deopt; else
    // jump header(sum, i+1).
    fb.switch_to_block(body_block);
    fb.seal_block(body_block);
    let (sum, of) = fb.ins().sadd_overflow(acc_p, i_p);
    let one = fb.ins().iconst(I64, 1);
    let next_i = fb.ins().iadd(i_p, one);
    let no_of = {
        let zero = fb.ins().iconst(ir::types::I8, 0);
        fb.ins().icmp(IntCC::Equal, of, zero)
    };
    fb.ins().brif(
        no_of,
        header_block,
        &[BlockArg::Value(sum), BlockArg::Value(next_i)],
        deopt_block,
        empty.iter(),
    );
    fb.seal_block(header_block);

    // Exit: store acc into ctx.result_slot, return Success (0).
    fb.switch_to_block(exit_block);
    fb.seal_block(exit_block);
    fb.ins().store(
        MemFlags::trusted(),
        acc_p,
        trace_ctx,
        relon_trace_emitter::result_slot_offset(),
    );
    let zero_i32 = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
    fb.ins().return_(&[zero_i32]);

    // Deopt: call save_deopt(ctx, 0, 0), return GuardFailed (1).
    fb.switch_to_block(deopt_block);
    fb.seal_block(deopt_block);
    let guard_pc = fb.ins().iconst(I32, 0);
    let ext_pc = fb.ins().iconst(I64, 0);
    fb.ins()
        .call(save_deopt_ref, &[trace_ctx, guard_pc, ext_pc]);
    let failed_i32 = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    fb.ins().return_(&[failed_i32]);

    fb.finalize();

    let func_id = module
        .declare_function(
            "relon_trace_jit_loop_fn",
            Linkage::Local,
            &ctx.func.signature,
        )
        .expect("declare loop fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define loop fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    // SAFETY: signature matches TRACE_ENTRY_SIG.
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    TraceJitLoopFn {
        entry,
        _module: module,
    }
}

// =====================================================================
// =====  dispatch-boundary rows  ======================================
// =====================================================================

/// Step body for the dispatch-boundary rows: `acc + i` where both args
/// are sourced via `Op::LocalGet`. Used by the cranelift-AOT entry-fn
/// build path that pairs with the per-iter caller loop.
fn step_body() -> Vec<TaggedOp> {
    vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Add(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ]
}

/// Real hot-loop trace body: `LocalGet(0) + LocalGet(1); Return`.
fn step_body_trace_real() -> Vec<TaggedOp> {
    step_body()
}

fn step_ir() -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: step_body(),
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

/// Pre-warmed cranelift-AOT evaluator for the single-step body — used
/// by the legacy "Rust-side loop drives N invokes" baseline, not by
/// the new `cranelift_aot_loop` row.
fn build_cranelift_step_evaluator() -> CraneliftAotEvaluator {
    CraneliftAotEvaluator::from_ir_direct(
        step_ir(),
        SandboxConfig::default(),
        vec!["arg0".to_string(), "arg1".to_string()],
    )
    .expect("cranelift step compile")
}

/// ε-M0: install a SUM-1..=N loop trace through the actual recorder
/// pipeline (`register_recording` + `__relon_jump_to_recorder`).
///
/// The recorder's IR walker recurses into `Op::Loop`, emits
/// `MarkLoopHead { phis: [(acc_init, phi_acc), (i_init, phi_i)] }`,
/// records the body once, then emits the matching
/// `MarkLoopBack { next_values: [acc_next, i_next] }`. The emitter
/// lowers this to the same cranelift loop shape `build_trace_jit_loop_fn`
/// hand-builds.
///
/// Returns the synthetic fn_id the installed trace lives under.
fn install_recorded_loop_trace() -> u32 {
    // Pick a fn_id outside the ranges used by the dispatch-boundary
    // rows (`MAX_FN_ID - 2`) and the three-way / smoke ranges.
    let fn_id = (MAX_FN_ID - 4) as u32;
    let _ = clear_recording(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            // The bench's `sum_loop_ir` builds a Block { Loop { ... } }
            // shape where the loop yields its accumulator via the
            // wasm-style block-param. The recorder doesn't yet
            // record operand-stack-based loop carries; we use the
            // let-slot variant from the e2e test harness instead.
            body: sum_loop_let_slot_body(),
            param_tys: vec![IrType::I32],
        },
    );
    let state = global_trace_jit_state();
    state.invalidate_trace(fn_id);
    let warm: [u64; 1] = [3];
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, warm.as_ptr());
    }
    assert!(
        state.lookup_trace(fn_id).is_some(),
        "ε-M0 recorder loop trace must install"
    );
    fn_id
}

/// IR body for the recorder-driven sum-loop. Uses let-slot carries
/// rather than wasm-style operand-stack yield, since the ε-M0
/// recorder only handles let-slot carries. The IR matches the
/// `build_sum_loop_body` shape in `recorded_loop_e2e.rs`.
fn sum_loop_let_slot_body() -> Vec<TaggedOp> {
    const I: u32 = 0;
    const ACC: u32 = 1;
    let t = |op: Op| TaggedOp {
        op,
        range: TokenRange::default(),
    };
    vec![
        t(Op::ConstI64(1)),
        t(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: ACC,
            ty: IrType::I64,
        }),
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::LocalGet(0)),
                    t(Op::Gt(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    t(Op::LetGet {
                        idx: ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        t(Op::LetGet {
            idx: ACC,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ]
}

/// Install a `LocalGet+LocalGet+Add+Return` trace through the
/// recorder-driven default install path. Returns the synthetic fn_id
/// the trace lives under in the global registry.
fn install_trace_for_step() -> u32 {
    let fn_id = (MAX_FN_ID - 2) as u32;
    let _ = clear_recording(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: step_body_trace_real(),
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );
    let state = global_trace_jit_state();
    state.invalidate_trace(fn_id);
    let warm: [u64; 2] = [1, 2];
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, warm.as_ptr());
    }
    assert!(
        state.lookup_trace(fn_id).is_some(),
        "trace must install for the dispatch-boundary bench step"
    );
    fn_id
}

/// Install the same 4-op trace via an isolated [`TraceJitState`] with
/// the explicit `call_conv` pinned on the trace entry signature.
fn install_explicit_conv_trace(call_conv: CallConv) -> (TraceJitState, u32) {
    use relon_trace_jit::{TraceBuffer, TraceOp};
    let fn_id = 0u32;
    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let sum = buffer.fresh_ssa();
    buffer.append(TraceOp::LocalGet(a, 0));
    buffer.append(TraceOp::LocalGet(b, 1));
    buffer.append(TraceOp::Add(sum, a, b));
    buffer.append(TraceOp::Return(sum));

    let state = TraceJitState::new();
    let trace_fn = state
        .jit_compile_buffer_for_fn_with_call_conv(fn_id, buffer, call_conv)
        .expect("explicit-conv install must succeed");
    state.install_trace(fn_id, trace_fn);
    (state, fn_id)
}

/// Build the same 4-op trace through the at-call-site inline path
/// (v6-ε-0-A). Used by the `dispatch_inline` row.
fn build_inline_step_host_fn() -> relon_codegen_native::InlineHostFn {
    use relon_trace_jit::{TraceBuffer, TraceOp};
    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let sum = buffer.fresh_ssa();
    buffer.append(TraceOp::LocalGet(a, 0));
    buffer.append(TraceOp::LocalGet(b, 1));
    buffer.append(TraceOp::Add(sum, a, b));
    buffer.append(TraceOp::Return(sum));
    let trace = Arc::new(buffer.into_optimized());
    compile_inline_host_fn(trace).expect("inline host-fn compile must succeed")
}

// =====================================================================
// =====  tree-walker fixture  =========================================
// =====================================================================

/// Tree-walker fixture for the `tree_walk_loop` row: a Relon `#main`
/// that delegates to `list.sum(range(n))`. `range(n)` builds
/// `[0, 1, ..., n-1]` via the top-level builtin; `list.sum` reaches
/// into the stdlib `std/list` module which is `_list_reduce`-backed.
/// The per-iter cost reported by criterion is total_runtime /
/// `TREE_WALK_LOOP_N`.
///
/// We deliberately don't try to hand-roll a tree-walker `while` loop
/// — Relon's source surface composes the loop primitive via
/// `_list_reduce`, and that's the canonical "Relon hot loop" shape on
/// the tree-walker path.
fn build_tree_walker() -> TreeWalkEvaluator {
    let src = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    TreeWalkEvaluator::new(Arc::new(ctx))
}

/// Argument helpers for the cranelift-AOT and tree-walker invocation
/// shapes. The cranelift-AOT row's evaluator is built with the
/// synthetic param name `n` (see [`build_cranelift_aot_loop`]).
fn args_n_for_cranelift(n: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

fn args_n_for_tree_walk(n: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

fn args_acc_i_step_eval(acc: i64, i: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("arg0".to_string(), Value::Int(acc));
    m.insert("arg1".to_string(), Value::Int(i));
    m
}

// =====================================================================
// =====  bench entry  =================================================
// =====================================================================

fn bench_hot_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("v6_epsilon_hot_loop");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(6));
    // Throughput::Elements(HOT_LOOP_N) makes criterion print per-iter
    // cost; this works for both the "single invoke runs N iters
    // internally" rows and the "Rust loop drives N invokes" rows.
    group.throughput(Throughput::Elements(HOT_LOOP_N));

    // ---------------- loop-INSIDE rows ----------------

    // --- tree_walk_loop: single invoke; tree-walker runs the full
    //     loop internally via `list.sum(range(n))`. N drops to
    //     `TREE_WALK_LOOP_N = 10_000` so the bench wall-clock stays
    //     manageable (tree-walker is µs/iter class on a 1M-element
    //     list); throughput adjusts to keep the per-element-cost
    //     surface honest.
    let walker = build_tree_walker();
    let scope = Arc::new(Scope::default());
    let tw_n = TREE_WALK_LOOP_N as i64;
    group.throughput(Throughput::Elements(TREE_WALK_LOOP_N));
    group.bench_function(BenchmarkId::new("backend", "tree_walk_loop"), |b| {
        b.iter(|| {
            let v = walker
                .run_main(&scope, args_n_for_tree_walk(black_box(tw_n)))
                .expect("tree-walk run_main");
            black_box(v)
        });
    });
    // Reset throughput back to HOT_LOOP_N for the remaining rows.
    group.throughput(Throughput::Elements(HOT_LOOP_N));

    // --- cranelift_aot_loop: single invoke; the cranelift-AOT fn body
    //     IS the `sum 1..=n` loop. We pass `n = HOT_LOOP_N` so the loop
    //     runs exactly `HOT_LOOP_N` body iters (i runs 1..=N with exit
    //     when i > N). The bench compares per-iter cost; the absolute
    //     result is the analytic `N*(N+1)/2` which `black_box` keeps.
    let cranelift_loop = build_cranelift_aot_loop();
    let n_full = HOT_LOOP_N as i64;
    group.bench_function(BenchmarkId::new("backend", "cranelift_aot_loop"), |b| {
        b.iter(|| {
            let v = cranelift_loop
                .run_main(args_n_for_cranelift(black_box(n_full)))
                .expect("cranelift sum-loop run_main");
            black_box(v)
        });
    });

    // --- trace_jit_loop: the real hot-loop measurement. One Rust→JIT
    //     invoke; the JIT fn body runs `n_full` iters (1..=n) with
    //     overflow guard inside cranelift's compiled loop. No per-iter
    //     extern-C boundary; the entire hot loop is inside the trace.
    let trace_loop_fn = build_trace_jit_loop_fn();
    group.bench_function(BenchmarkId::new("backend", "trace_jit_loop"), |b| {
        b.iter(|| {
            let mut ctx = TraceContext::with_capacity(64);
            let args: [u64; 1] = [black_box(n_full) as u64];
            let raw = unsafe { trace_loop_fn.invoke(&mut ctx as *mut _, args.as_ptr()) };
            // Expect Success; a guard fire would surface here so we
            // can fail loudly without polluting the measurement.
            assert_eq!(raw, 0, "trace_jit_loop must finish on the Success path");
            black_box(ctx.result_slot as i64)
        });
    });
    drop(trace_loop_fn);

    // --- trace_jit_loop_recorded (ε-M0): same shape as
    //     `trace_jit_loop` above, but the trace is installed through
    //     the actual recorder pipeline (`register_recording` +
    //     `__relon_jump_to_recorder`) rather than hand-built. The
    //     recorder's IR walker hits `Op::Loop`, emits MarkLoopHead /
    //     MarkLoopBack with the matching φ pair, and the trace
    //     emitter lowers to the same cranelift IR shape the
    //     hand-built path produces. The ε-M0 brief's per-iter perf
    //     bar: ≤ 2× the hand-built `trace_jit_loop` number.
    let recorded_fn_id = install_recorded_loop_trace();
    let recorded_state = global_trace_jit_state();
    group.bench_function(
        BenchmarkId::new("backend", "trace_jit_loop_recorded"),
        |b| {
            b.iter(|| {
                let n = black_box(n_full);
                let args: [u64; 1] = [n as u64];
                let v = unsafe {
                    recorded_state.invoke_with_fallback(
                        recorded_fn_id,
                        args.as_ptr(),
                        64,
                        |_args| {
                            // Fallback on guard fire: the analytic
                            // `n*(n+1)/2`. We avoid a tree-walker
                            // invocation here because criterion's
                            // per-iter measurement would otherwise be
                            // dominated by the fallback cost. The
                            // recorded trace's hot path (cmp+brif+add
                            // back-edge) is what the bench is
                            // measuring; the deopt resume cost is
                            // paid once per invocation, not per iter.
                            (n * (n + 1) / 2) as u64
                        },
                    )
                };
                black_box(v)
            });
        },
    );

    // --- rust_native_loop: pure Rust theoretical floor. Same `1..=n`
    //     shape as `cranelift_aot_loop` / `trace_jit_loop` so the
    //     comparison is direct.
    group.bench_function(BenchmarkId::new("backend", "rust_native_loop"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            let n = black_box(n_full);
            for i in 1..=n {
                acc = match acc.checked_add(i) {
                    Some(v) => v,
                    None => acc.wrapping_add(i),
                };
            }
            black_box(acc)
        });
    });

    // ---------------- dispatch-boundary rows ----------------
    //
    // These rows preserve the v6-γ M5 → v6-ε-0-A measurement shape so
    // the falsification chain stays auditable in one bench output.
    // Each row's caller is a Rust `for` loop that invokes the trace
    // fn `HOT_LOOP_N` times. They measure the Rust→JIT call boundary
    // cost per dispatch, NOT hot-loop per-iter cost.

    let step_eval = build_cranelift_step_evaluator();
    group.bench_function(
        BenchmarkId::new("backend", "dispatch_cranelift_step"),
        |b| {
            b.iter(|| {
                let mut acc: i64 = 0;
                for i in 0..HOT_LOOP_N as i64 {
                    let r = step_eval
                        .run_main(args_acc_i_step_eval(black_box(acc), black_box(i)))
                        .expect("cranelift step run_main");
                    if let Value::Int(v) = r {
                        acc = v;
                    }
                }
                black_box(acc)
            });
        },
    );

    let fn_id = install_trace_for_step();
    let state = global_trace_jit_state();
    let trace_fn = state.lookup_trace(fn_id).expect("post-install");
    group.bench_function(BenchmarkId::new("backend", "dispatch_trampoline"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            let mut ctx = TraceContext::with_capacity(64);
            let mut args: [u64; 2] = [0, 0];
            for i in 0..HOT_LOOP_N as i64 {
                args[0] = black_box(acc) as u64;
                args[1] = black_box(i) as u64;
                let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, args.as_ptr()) };
                if matches!(status, TraceEntryStatus::Success) {
                    acc = ctx.result_slot as i64;
                } else {
                    acc = acc.wrapping_add(i);
                }
            }
            black_box(acc)
        });
    });
    drop(trace_fn);

    let ic_slot = TraceIcSlot::new();
    let entry = ic_slot
        .lookup_or_install(fn_id, 0)
        .expect("IC slot must resolve installed trace");
    let _trace_anchor = state.lookup_trace(fn_id).expect("post-install (ic)");
    group.bench_function(BenchmarkId::new("backend", "dispatch_ic"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            let mut ctx = TraceContext::with_capacity(64);
            let mut args: [u64; 2] = [0, 0];
            for i in 0..HOT_LOOP_N as i64 {
                args[0] = black_box(acc) as u64;
                args[1] = black_box(i) as u64;
                let raw = unsafe { entry(&mut ctx as *mut _, args.as_ptr()) };
                if raw == 0 {
                    acc = ctx.result_slot as i64;
                } else {
                    acc = acc.wrapping_add(i);
                }
            }
            black_box(acc)
        });
    });

    let (tail_state, tail_fn_id) = install_explicit_conv_trace(CallConv::Tail);
    let tail_trace_anchor = tail_state
        .lookup_trace(tail_fn_id)
        .expect("explicit-Tail install");
    let tail_entry = unsafe { tail_trace_anchor.typed_entry() };
    group.bench_function(BenchmarkId::new("backend", "dispatch_tail"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            let mut ctx = TraceContext::with_capacity(64);
            let mut args: [u64; 2] = [0, 0];
            for i in 0..HOT_LOOP_N as i64 {
                args[0] = black_box(acc) as u64;
                args[1] = black_box(i) as u64;
                let raw = unsafe { tail_entry(&mut ctx as *mut _, args.as_ptr()) };
                if raw == 0 {
                    acc = ctx.result_slot as i64;
                } else {
                    acc = acc.wrapping_add(i);
                }
            }
            black_box(acc)
        });
    });
    drop(tail_trace_anchor);
    drop(tail_state);

    let (sysv_state, sysv_fn_id) = install_explicit_conv_trace(CallConv::SystemV);
    let sysv_trace_anchor = sysv_state
        .lookup_trace(sysv_fn_id)
        .expect("explicit-SystemV install");
    let sysv_entry = unsafe { sysv_trace_anchor.typed_entry() };
    group.bench_function(BenchmarkId::new("backend", "dispatch_sysv"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            let mut ctx = TraceContext::with_capacity(64);
            let mut args: [u64; 2] = [0, 0];
            for i in 0..HOT_LOOP_N as i64 {
                args[0] = black_box(acc) as u64;
                args[1] = black_box(i) as u64;
                let raw = unsafe { sysv_entry(&mut ctx as *mut _, args.as_ptr()) };
                if raw == 0 {
                    acc = ctx.result_slot as i64;
                } else {
                    acc = acc.wrapping_add(i);
                }
            }
            black_box(acc)
        });
    });
    drop(sysv_trace_anchor);
    drop(sysv_state);

    let inline_host_fn = build_inline_step_host_fn();
    let inline_entry = unsafe { inline_host_fn.typed_entry() };
    group.bench_function(BenchmarkId::new("backend", "dispatch_inline"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            let mut ctx = TraceContext::with_capacity(64);
            let mut args: [u64; 2] = [0, 0];
            for i in 0..HOT_LOOP_N as i64 {
                args[0] = black_box(acc) as u64;
                args[1] = black_box(i) as u64;
                let raw = unsafe { inline_entry(&mut ctx as *mut _, args.as_ptr()) };
                if raw == 0 {
                    acc = ctx.result_slot as i64;
                } else {
                    acc = acc.wrapping_add(i);
                }
            }
            black_box(acc)
        });
    });
    drop(inline_host_fn);

    // --- dispatch_rust_inlined_baseline: same Rust-side per-iter
    //     dispatch shape as the trace-fn rows above, but the callee
    //     body is the pure-Rust `checked_add`. Theoretical floor for
    //     the **dispatch-shape** rows (NOT the loop-INSIDE rows).
    //     The gap between this and the dispatch rows isolates the
    //     Rust→JIT extern-C boundary cost cleanly.
    let ic_slot_owner = ic_slot;
    let _ = ic_slot_owner; // keep IC slot alloc alive symmetrically.
    group.bench_function(
        BenchmarkId::new("backend", "dispatch_rust_inlined_baseline"),
        |b| {
            b.iter(|| {
                let mut acc: i64 = 0;
                for i in 0..HOT_LOOP_N as i64 {
                    let a = black_box(acc);
                    let j = black_box(i);
                    acc = match a.checked_add(j) {
                        Some(v) => v,
                        None => a.wrapping_add(j),
                    };
                }
                black_box(acc)
            });
        },
    );

    group.finish();
}

criterion_group!(benches, bench_hot_loop);
criterion_main!(benches);
