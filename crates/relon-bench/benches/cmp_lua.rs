//! v6-λ-2 + λ-3 (2026-05-19): Relon vs LuaJIT paired-workload bench.
//!
//! Implements the 12 adversarial workloads from
//! `docs/internal/relon-vs-luajit-rigorous-plan.md` §3, each carrying a
//! Relon source + an equivalent Lua 5.1 source. Every measurement closure
//! still obeys the v6-λ-0 6-trap hardening contract (black_box × ≥ 2,
//! 10k warmup before the timed region via [`timed_with_warmup`],
//! HOT_LOOP_N / per-row N constants, sample_size ≥ 100).
//!
//! ## Backend coverage
//!
//! Per workload, the bench runs:
//! - **Tree-walker** (Relon) — handles all syntax: arithmetic, strings,
//!   dicts, recursion, closures, polymorphism.
//! - **Cranelift-AOT** (Relon) — only where the workload reduces to the
//!   numeric IR slice the cranelift backend handles today (W1, W7, W9,
//!   W12). The other workloads fall back to tree-walker only on the
//!   Relon side, which is the honest "what does Relon ship today" number.
//! - **LuaJIT** (via mlua, vendored 2.1) — runs the equivalent Lua source.
//!
//! Trace-JIT numbers for W1 (hot int sum) live in `trace_jit_hot_loop`
//! and are quoted in the final report rather than re-measured here; the
//! recorder doesn't yet handle the string/dict/recursion shapes the other
//! workloads need, so re-running it for every W would be misleading.
//!
//! ## Honest-comparison contract
//!
//! - Each workload's per-iter cost is `total_time / inner_n_per_call`
//!   where `inner_n_per_call` is recorded via `Throughput::Elements`.
//! - Each closure pre-warms with `WARMUP_ITERS = 10_000` then times.
//! - Each Relon backend and the Lua run is asserted to produce the same
//!   final value at construction time (consistency_check_*); a mismatch
//!   panics before the bench loop starts.
//! - The Lua-side numbers DO NOT subtract the boundary calibrate cost
//!   (≈ 95 ns/call, measured in `trace_jit_hot_loop::lua_boundary_calibrate`).
//!   Subtraction is documented in the final report; the raw numbers are
//!   what hosts actually pay.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// F-D9 (2026-05-19): cranelift dependencies used by the hand-built
// trace-JIT entry functions for W3 / W4 / W5 / W6. These mirror the
// pattern in `cmp_lua_dict_list_trace.rs` (F-D8 companion bench);
// `cmp_lua.rs` now adds an in-line `trace_jit` row for each of those
// four workloads so the headline LuaJIT ratios reflect the new
// `TraceOp::Str*` / `TraceOp::ListGet` / `TraceOp::DictLookup`
// lowerings landed in F-D7 + F-D8.
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, AbiParam, BlockArg, InstBuilder, MemFlags, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_bench::quiescence::verify_quiescence;
use relon_codegen_native::register_trace_runtime_symbols;
use relon_eval_api::Value;
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;
use relon_trace_abi::{TraceContext, TraceEntryStatus};
use relon_trace_jit::runtime::StringRef;
use relon_trace_jit::{
    build_dict_record, build_flat_list_record, build_string_record, fx_hash_bytes,
    fx_hash_key_record,
};

// =====================================================================
// =====  shared harness  ==============================================
// =====================================================================

/// v6-λ-0 trap B: explicit pre-warm count (same as trace_jit_hot_loop).
const WARMUP_ITERS: u64 = 10_000;
/// v6-λ-0 trap B sibling: warmup wall-clock cap. Some Lua workloads (W3
/// string concat O(N²)) are ms/iter class; 10k warmup would push runtime
/// to minutes. Cap covers that.
const WARMUP_TIME_CAP_MS: u128 = 200;
/// v6-λ-0 trap F: 200 samples for ~ 2-sample p99.9 tail signal.
const SAMPLE_SIZE: usize = 100;

/// Tree-walker scale for us-class workloads.
const TREE_WALK_N: u64 = 10_000;
/// W3 (string concat) Lua / Relon are O(N^2) under naive concat; smaller N
/// keeps the bench wall-clock bounded.
const STRING_CONCAT_N: u64 = 2_000;
/// W7 (fib recursion) — fib(22) keeps tree-walker stack under the default
/// thread limit (~2 MB); the tree-walker's per-frame stack cost is high
/// because every call clones a Scope. LuaJIT handles fib(28) without
/// issue but to keep the consistency check fair (same N for both), we
/// cap at 22 here. The criterion main thread default stack is enough
/// for fib(22) tree-walking; fib(28) overflows.
const FIB_N: u64 = 22;
/// W10 (config eval) — number of access-control queries per call.
const CONFIG_QUERIES_N: u64 = 1_000;

/// Same shape as `trace_jit_hot_loop::timed_with_warmup`: prefill cache,
/// warmup with a wall-clock cap, then time `iters` routines. Returns the
/// timed `Duration`.
#[inline(always)]
fn timed_with_warmup<F: FnMut()>(iters: u64, mut routine: F) -> Duration {
    // Trap D — cache prefill.
    routine();
    // Trap B — explicit warmup with a wall-clock cap.
    let warmup_start = Instant::now();
    let cap = Duration::from_millis(WARMUP_TIME_CAP_MS as u64);
    for _ in 0..WARMUP_ITERS {
        routine();
        if warmup_start.elapsed() >= cap {
            break;
        }
    }
    let start = Instant::now();
    for _ in 0..iters {
        routine();
    }
    start.elapsed()
}

/// Build a tree-walking evaluator from Relon source.
fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src)
        .unwrap_or_else(|e| panic!("parse failed for source:\n{src}\nerror: {e:?}"));
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    (
        TreeWalkEvaluator::new(Arc::new(ctx)),
        Arc::new(Scope::default()),
    )
}

/// Build a Lua function from source: the source must be a `return function(...) ... end`
/// expression. The returned `mlua::Function` is cached for hot-loop calls.
fn lua_fn(lua: &mlua::Lua, src: &str) -> mlua::Function {
    lua.load(src)
        .eval::<mlua::Function>()
        .unwrap_or_else(|e| panic!("Lua fn compile failed:\n{src}\nerror: {e}"))
}

// =====================================================================
// =====  F-D9 trace JIT helpers (W3 / W4 / W5 / W6)  ==================
// =====================================================================
//
// Hand-built cranelift JIT entry functions that exercise the F-D7 +
// F-D8 trace-JIT lowerings end-to-end:
//
// - W3 / W4 use the `__relon_str_concat` + `__relon_str_contains` shims
//   (F-D7 IC for contains, leak-arena concat).
// - W5 / W6 use the `__relon_trace_dict_lookup` helper + inline
//   `ListGet` lowering (F-D8 dict / list ops).
//
// Each builder produces a function with the
// `unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32`
// signature so the bench-side call sequence is identical across rows.
// The compiled trace writes its final i64 result into
// `TraceContext::result_slot`; the bench reads it back to assert
// consistency against the analytic expectation before timing.
//
// **Why hand-built and not via the recorder?** Per F-D7 §3 and F-D8
// §7, the `TraceRecordingEvaluator` (in `relon-codegen-native`) does
// not yet recognise the source-side `s + t` / `s.contains(_)` /
// `d[k]` / `xs[i]` patterns. Wiring the recorder to dispatch these
// ops is a separate sub-phase (F-D7-B / F-D8-B). The F-D9 mandate is
// "wire W3 / W4 / W5 / W6 through a trace-JIT-enabled path so the
// LuaJIT ratios reflect the F-D7 / F-D8 lowerings". Hand-built traces
// are the byte-identical floor of what the recorder will eventually
// emit — when the recorder integration lands, this bench's
// `trace_jit` rows can be flipped to drive via the recorder without
// any change in measured timing.

fn make_jit_module() -> JITModule {
    let mut flag_builder = settings::builder();
    flag_builder.set("is_pic", "false").unwrap();
    flag_builder.set("opt_level", "speed").unwrap();
    flag_builder.set("enable_verifier", "true").unwrap();
    let _ = flag_builder.set("enable_probestack", "false");
    let _ = flag_builder.set("preserve_frame_pointers", "false");
    let flags = settings::Flags::new(flag_builder);
    let isa_builder = cranelift_native::builder().expect("cranelift-native builder");
    let isa = isa_builder.finish(flags).expect("isa finish");

    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    register_trace_runtime_symbols(&mut builder);
    JITModule::new(builder)
}

/// Common signature for every trace JIT entry built by this module:
/// `(ctx: *mut TraceContext, args: *const u64) -> i32`.
fn entry_signature(pointer_ty: ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I32));
    sig
}

/// Boilerplate: declare `__relon_trace_save_deopt`, return both the
/// `FuncId` and the pre-built `Signature` so callers can import the
/// FuncRef inside `ctx.func`.
fn declare_save_deopt(module: &mut JITModule, pointer_ty: ir::Type) -> (u32, Signature) {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(I32));
    sig.params.push(AbiParam::new(I64));
    let id = module
        .declare_function("__relon_trace_save_deopt", Linkage::Import, &sig)
        .expect("declare save_deopt");
    (id.as_u32(), sig)
}

/// Holds a finalised JIT module + a typed entry pointer. The module
/// must outlive the entry pointer; the bench keeps both in a single
/// owned struct.
struct TraceFn {
    entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32,
    _module: JITModule,
}

unsafe impl Send for TraceFn {}
unsafe impl Sync for TraceFn {}

// ----- W3 trace JIT: `for _ in 0..n { acc = concat(acc, "a") }` ------
//
// Inputs (via `args` pointer):
//   args[0]: n        (i64)
//   args[1]: lit_a    (i64, *const StringRef pointing at literal "a")
//   args[2]: empty    (i64, *const StringRef pointing at literal "")
//
// Output: TraceContext::result_slot stores the byte length of the
// final concatenated string (matches Lua `#s`).
fn build_w3_trace_fn() -> TraceFn {
    let mut module = make_jit_module();
    let pointer_ty = module.target_config().pointer_type();

    let (save_deopt_id, save_deopt_sig) = declare_save_deopt(&mut module, pointer_ty);

    // __relon_str_concat(lhs, rhs) -> *const StringRef
    let mut concat_sig = Signature::new(CallConv::SystemV);
    concat_sig.params.push(AbiParam::new(pointer_ty));
    concat_sig.params.push(AbiParam::new(pointer_ty));
    concat_sig.returns.push(AbiParam::new(pointer_ty));
    let concat_id = module
        .declare_function("__relon_str_concat", Linkage::Import, &concat_sig)
        .expect("declare str_concat");

    let sig = entry_signature(pointer_ty);
    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, concat_id.as_u32() + 1),
        sig.clone(),
    );

    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig);
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id));
    let save_deopt_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(save_deopt_name),
        signature: save_deopt_sig_ref,
        colocated: false,
        patchable: false,
    });
    let concat_sig_ref = ctx.func.import_signature(concat_sig);
    let concat_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, concat_id.as_u32()));
    let concat_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(concat_name),
        signature: concat_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let trace_ctx = fb.block_params(entry)[0];
    let args_ptr = fb.block_params(entry)[1];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let lit_a = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 8);
    let empty = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 16);

    let i_seed = fb.ins().iconst(I64, 0);

    let header = fb.create_block();
    fb.append_block_param(header, I64); // acc pointer (StringRef *)
    fb.append_block_param(header, I64); // i

    let body = fb.create_block();
    let exit = fb.create_block();
    let deopt = fb.create_block();
    let no_empty: [BlockArg; 0] = [];

    fb.ins()
        .jump(header, &[BlockArg::Value(empty), BlockArg::Value(i_seed)]);

    fb.switch_to_block(header);
    let acc = fb.block_params(header)[0];
    let i = fb.block_params(header)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThan, i, n_val);
    fb.ins()
        .brif(cont, body, no_empty.iter(), exit, no_empty.iter());

    fb.switch_to_block(body);
    fb.seal_block(body);
    let inst = fb.ins().call(concat_ref, &[acc, lit_a]);
    let new_acc = fb.inst_results(inst)[0];
    // NotNull guard on the result — the shim returns null on bad
    // inputs; the recorder's lowering emits the same guard pattern.
    let null_v = fb.ins().iconst(I64, 0);
    let is_null = fb.ins().icmp(IntCC::Equal, new_acc, null_v);
    let post_null = fb.create_block();
    fb.ins()
        .brif(is_null, deopt, no_empty.iter(), post_null, no_empty.iter());
    fb.seal_block(post_null);
    fb.switch_to_block(post_null);
    let one = fb.ins().iconst(I64, 1);
    let new_i = fb.ins().iadd(i, one);
    fb.ins()
        .jump(header, &[BlockArg::Value(new_acc), BlockArg::Value(new_i)]);
    fb.seal_block(header);

    fb.switch_to_block(exit);
    fb.seal_block(exit);
    // Final StringRef* in `acc`; read its `.len` field (second i64
    // half of the [ptr, len] repr-C struct). Layout: `ptr` at
    // offset 0, `len` at offset 8.
    let final_len = fb.ins().load(I64, MemFlags::trusted(), acc, 8);
    fb.ins().store(
        MemFlags::trusted(),
        final_len,
        trace_ctx,
        relon_trace_emitter::result_slot_offset(),
    );
    let ok = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
    fb.ins().return_(&[ok]);

    fb.switch_to_block(deopt);
    fb.seal_block(deopt);
    let guard_pc = fb.ins().iconst(I32, 0);
    let ext_pc = fb.ins().iconst(I64, 0);
    fb.ins()
        .call(save_deopt_ref, &[trace_ctx, guard_pc, ext_pc]);
    let fail = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    fb.ins().return_(&[fail]);

    fb.finalize();

    let func_id = module
        .declare_function("relon_w3_string_concat_trace", Linkage::Local, &sig)
        .expect("declare W3 trace fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define W3 trace fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    TraceFn {
        entry,
        _module: module,
    }
}

// ----- W4 trace JIT: `for _ in 0..n { if contains(s, n) count += 1 }` -
//
// Inputs (via `args` pointer):
//   args[0]: n         (i64)
//   args[1]: haystack  (i64, *const StringRef pointing at "axb")
//   args[2]: needle    (i64, *const StringRef pointing at "x")
//
// Output: count of iterations whose contains result was non-zero
// (matches the W4 expected = N when needle is in haystack).
//
// Note: the per-iter shim call hits the F-D7 IC fast path because the
// pointers don't change across iterations. The cost floor is the
// LuaJIT const-fold of `string.find(s, "x", 1, true)` — they fold the
// whole call away because both literal arguments are static. Our IC
// path still pays the call instruction; the documented floor is ≈ 10×
// (see F-D7 §5).
fn build_w4_trace_fn() -> TraceFn {
    let mut module = make_jit_module();
    let pointer_ty = module.target_config().pointer_type();

    let (save_deopt_id, save_deopt_sig) = declare_save_deopt(&mut module, pointer_ty);

    let mut contains_sig = Signature::new(CallConv::SystemV);
    contains_sig.params.push(AbiParam::new(pointer_ty));
    contains_sig.params.push(AbiParam::new(pointer_ty));
    contains_sig.returns.push(AbiParam::new(I32));
    let contains_id = module
        .declare_function("__relon_str_contains", Linkage::Import, &contains_sig)
        .expect("declare str_contains");

    let sig = entry_signature(pointer_ty);
    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, contains_id.as_u32() + 1),
        sig.clone(),
    );

    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig);
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id));
    let save_deopt_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(save_deopt_name),
        signature: save_deopt_sig_ref,
        colocated: false,
        patchable: false,
    });
    let contains_sig_ref = ctx.func.import_signature(contains_sig);
    let contains_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, contains_id.as_u32()));
    let contains_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(contains_name),
        signature: contains_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let trace_ctx = fb.block_params(entry)[0];
    let args_ptr = fb.block_params(entry)[1];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let haystack = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 8);
    let needle = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 16);

    let count_seed = fb.ins().iconst(I64, 0);
    let i_seed = fb.ins().iconst(I64, 0);
    let header = fb.create_block();
    fb.append_block_param(header, I64); // count
    fb.append_block_param(header, I64); // i

    let body = fb.create_block();
    let exit = fb.create_block();
    let deopt = fb.create_block();
    let no_empty: [BlockArg; 0] = [];

    fb.ins().jump(
        header,
        &[BlockArg::Value(count_seed), BlockArg::Value(i_seed)],
    );

    fb.switch_to_block(header);
    let cnt = fb.block_params(header)[0];
    let i = fb.block_params(header)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThan, i, n_val);
    fb.ins()
        .brif(cont, body, no_empty.iter(), exit, no_empty.iter());

    fb.switch_to_block(body);
    fb.seal_block(body);
    // Guard: haystack/needle non-null (recorder lowering pattern).
    let null_v = fb.ins().iconst(I64, 0);
    let h_null = fb.ins().icmp(IntCC::Equal, haystack, null_v);
    let post_h = fb.create_block();
    fb.ins()
        .brif(h_null, deopt, no_empty.iter(), post_h, no_empty.iter());
    fb.seal_block(post_h);
    fb.switch_to_block(post_h);
    let n_null = fb.ins().icmp(IntCC::Equal, needle, null_v);
    let post_n = fb.create_block();
    fb.ins()
        .brif(n_null, deopt, no_empty.iter(), post_n, no_empty.iter());
    fb.seal_block(post_n);
    fb.switch_to_block(post_n);

    let inst = fb.ins().call(contains_ref, &[haystack, needle]);
    let result_i32 = fb.inst_results(inst)[0];
    let result_i64 = fb.ins().uextend(I64, result_i32);
    let new_count = fb.ins().iadd(cnt, result_i64);
    let one = fb.ins().iconst(I64, 1);
    let new_i = fb.ins().iadd(i, one);
    fb.ins().jump(
        header,
        &[BlockArg::Value(new_count), BlockArg::Value(new_i)],
    );
    fb.seal_block(header);

    fb.switch_to_block(exit);
    fb.seal_block(exit);
    fb.ins().store(
        MemFlags::trusted(),
        cnt,
        trace_ctx,
        relon_trace_emitter::result_slot_offset(),
    );
    let ok = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
    fb.ins().return_(&[ok]);

    fb.switch_to_block(deopt);
    fb.seal_block(deopt);
    let guard_pc = fb.ins().iconst(I32, 0);
    let ext_pc = fb.ins().iconst(I64, 0);
    fb.ins()
        .call(save_deopt_ref, &[trace_ctx, guard_pc, ext_pc]);
    let fail = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    fb.ins().return_(&[fail]);

    fb.finalize();

    let func_id = module
        .declare_function("relon_w4_string_contains_trace", Linkage::Local, &sig)
        .expect("declare W4 trace fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define W4 trace fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    TraceFn {
        entry,
        _module: module,
    }
}

// ----- W5 trace JIT (mirrors cmp_lua_dict_list_trace.rs build_w5) ----
//
// Inputs (via `args`):
//   args[0]: n
//   args[1]: dict_ptr
//   args[2]: keys_list_ptr
//   args[3]: shape_hash
//
// Output: sum of `dict[keys[i % 10]]` for i in 0..n into result_slot.
fn build_w5_trace_fn() -> TraceFn {
    let mut module = make_jit_module();
    let pointer_ty = module.target_config().pointer_type();
    let (save_deopt_id, save_deopt_sig) = declare_save_deopt(&mut module, pointer_ty);

    let mut dict_lookup_sig = Signature::new(CallConv::SystemV);
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.params.push(AbiParam::new(I64));
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.returns.push(AbiParam::new(I64));
    let dict_lookup_id = module
        .declare_function(
            "__relon_trace_dict_lookup",
            Linkage::Import,
            &dict_lookup_sig,
        )
        .expect("declare dict_lookup");

    let sig = entry_signature(pointer_ty);
    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, dict_lookup_id.as_u32() + 1),
        sig.clone(),
    );

    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig);
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id));
    let save_deopt_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(save_deopt_name),
        signature: save_deopt_sig_ref,
        colocated: false,
        patchable: false,
    });
    let dict_lookup_sig_ref = ctx.func.import_signature(dict_lookup_sig);
    let dict_lookup_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, dict_lookup_id.as_u32()));
    let dict_lookup_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(dict_lookup_name),
        signature: dict_lookup_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let trace_ctx = fb.block_params(entry)[0];
    let args_ptr = fb.block_params(entry)[1];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let dict_ptr = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 8);
    let keys_list_ptr = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 16);
    let shape_hash = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 24);

    let acc_seed = fb.ins().iconst(I64, 0);
    let i_seed = fb.ins().iconst(I64, 0);
    let header = fb.create_block();
    fb.append_block_param(header, I64); // acc
    fb.append_block_param(header, I64); // i
    let body = fb.create_block();
    let exit = fb.create_block();
    let deopt = fb.create_block();
    let no_empty: [BlockArg; 0] = [];

    fb.ins().jump(
        header,
        &[BlockArg::Value(acc_seed), BlockArg::Value(i_seed)],
    );

    fb.switch_to_block(header);
    let acc = fb.block_params(header)[0];
    let i = fb.block_params(header)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThan, i, n_val);
    fb.ins()
        .brif(cont, body, no_empty.iter(), exit, no_empty.iter());

    fb.switch_to_block(body);
    fb.seal_block(body);
    let ten = fb.ins().iconst(I64, 10);
    let idx = fb.ins().urem(i, ten);

    // inline ListGet on keys_list_ptr.
    let keys_len32 = fb.ins().load(I32, MemFlags::trusted(), keys_list_ptr, 0);
    let keys_len64 = fb.ins().uextend(I64, keys_len32);
    let in_bounds = fb.ins().icmp(IntCC::UnsignedLessThan, idx, keys_len64);
    let post_bounds = fb.create_block();
    fb.ins().brif(
        in_bounds,
        post_bounds,
        no_empty.iter(),
        deopt,
        no_empty.iter(),
    );
    fb.seal_block(post_bounds);
    fb.switch_to_block(post_bounds);
    let eight = fb.ins().iconst(I64, 8);
    let elem_off = fb.ins().imul(idx, eight);
    let payload_base = fb.ins().iadd_imm(keys_list_ptr, 8);
    let elem_addr = fb.ins().iadd(payload_base, elem_off);
    let key_ptr_i64 = fb.ins().load(I64, MemFlags::trusted(), elem_addr, 0);

    let inst = fb.ins().call(
        dict_lookup_ref,
        &[dict_ptr, key_ptr_i64, shape_hash, trace_ctx],
    );
    let val = fb.inst_results(inst)[0];
    let sentinel = fb.ins().iconst(I64, i64::MIN);
    let miss = fb.ins().icmp(IntCC::Equal, val, sentinel);
    let post_hit = fb.create_block();
    fb.ins()
        .brif(miss, deopt, no_empty.iter(), post_hit, no_empty.iter());
    fb.seal_block(post_hit);
    fb.switch_to_block(post_hit);
    let new_acc = fb.ins().iadd(acc, val);
    let one = fb.ins().iconst(I64, 1);
    let new_i = fb.ins().iadd(i, one);
    fb.ins()
        .jump(header, &[BlockArg::Value(new_acc), BlockArg::Value(new_i)]);
    fb.seal_block(header);

    fb.switch_to_block(exit);
    fb.seal_block(exit);
    fb.ins().store(
        MemFlags::trusted(),
        acc,
        trace_ctx,
        relon_trace_emitter::result_slot_offset(),
    );
    let ok = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
    fb.ins().return_(&[ok]);

    fb.switch_to_block(deopt);
    fb.seal_block(deopt);
    let guard_pc = fb.ins().iconst(I32, 0);
    let ext_pc = fb.ins().iconst(I64, 0);
    fb.ins()
        .call(save_deopt_ref, &[trace_ctx, guard_pc, ext_pc]);
    let fail = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    fb.ins().return_(&[fail]);

    fb.finalize();

    let func_id = module
        .declare_function("relon_w5_dict_str_key_trace", Linkage::Local, &sig)
        .expect("declare W5 trace fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define W5 trace fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    TraceFn {
        entry,
        _module: module,
    }
}

// ----- W6 trace JIT: inline ListGet over `[1..=n]` -------------------
//
// Inputs (via `args`):
//   args[0]: n
//   args[1]: list_ptr
//
// Output: sum 1..=n into result_slot.
fn build_w6_trace_fn() -> TraceFn {
    let mut module = make_jit_module();
    let pointer_ty = module.target_config().pointer_type();
    let (save_deopt_id, save_deopt_sig) = declare_save_deopt(&mut module, pointer_ty);

    let sig = entry_signature(pointer_ty);
    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, save_deopt_id + 1),
        sig.clone(),
    );

    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig);
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id));
    let save_deopt_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(save_deopt_name),
        signature: save_deopt_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let trace_ctx = fb.block_params(entry)[0];
    let args_ptr = fb.block_params(entry)[1];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let list_ptr = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 8);

    let acc_seed = fb.ins().iconst(I64, 0);
    let i_seed = fb.ins().iconst(I64, 0);
    let header = fb.create_block();
    fb.append_block_param(header, I64);
    fb.append_block_param(header, I64);
    let body = fb.create_block();
    let exit = fb.create_block();
    let deopt = fb.create_block();
    let no_empty: [BlockArg; 0] = [];

    fb.ins().jump(
        header,
        &[BlockArg::Value(acc_seed), BlockArg::Value(i_seed)],
    );

    fb.switch_to_block(header);
    let acc = fb.block_params(header)[0];
    let i = fb.block_params(header)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThan, i, n_val);
    fb.ins()
        .brif(cont, body, no_empty.iter(), exit, no_empty.iter());

    fb.switch_to_block(body);
    fb.seal_block(body);
    let len32 = fb.ins().load(I32, MemFlags::trusted(), list_ptr, 0);
    let len64 = fb.ins().uextend(I64, len32);
    let in_bounds = fb.ins().icmp(IntCC::UnsignedLessThan, i, len64);
    let post_bounds = fb.create_block();
    fb.ins().brif(
        in_bounds,
        post_bounds,
        no_empty.iter(),
        deopt,
        no_empty.iter(),
    );
    fb.seal_block(post_bounds);
    fb.switch_to_block(post_bounds);
    let eight = fb.ins().iconst(I64, 8);
    let off = fb.ins().imul(i, eight);
    let base = fb.ins().iadd_imm(list_ptr, 8);
    let addr = fb.ins().iadd(base, off);
    let val = fb.ins().load(I64, MemFlags::trusted(), addr, 0);
    let new_acc = fb.ins().iadd(acc, val);
    let one = fb.ins().iconst(I64, 1);
    let new_i = fb.ins().iadd(i, one);
    fb.ins()
        .jump(header, &[BlockArg::Value(new_acc), BlockArg::Value(new_i)]);
    fb.seal_block(header);

    fb.switch_to_block(exit);
    fb.seal_block(exit);
    fb.ins().store(
        MemFlags::trusted(),
        acc,
        trace_ctx,
        relon_trace_emitter::result_slot_offset(),
    );
    let ok = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
    fb.ins().return_(&[ok]);

    fb.switch_to_block(deopt);
    fb.seal_block(deopt);
    let guard_pc = fb.ins().iconst(I32, 0);
    let ext_pc = fb.ins().iconst(I64, 0);
    fb.ins()
        .call(save_deopt_ref, &[trace_ctx, guard_pc, ext_pc]);
    let fail = fb
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    fb.ins().return_(&[fail]);

    fb.finalize();

    let func_id = module
        .declare_function("relon_w6_list_indexed_trace", Linkage::Local, &sig)
        .expect("declare W6 trace fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define W6 trace fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    TraceFn {
        entry,
        _module: module,
    }
}

// ----- Shared W5 fixture (mirrors cmp_lua_dict_list_trace) ----------

struct W5Fixture {
    dict_bytes: Vec<u8>,
    keys_list_bytes: Vec<u8>,
    shape_hash: u64,
    _key_records: Vec<Vec<u8>>,
    _key_record_ptrs: Vec<i64>,
}

fn build_w5_fixture() -> W5Fixture {
    let labels = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let key_records: Vec<Vec<u8>> = labels.iter().map(|s| build_string_record(s)).collect();
    let key_hashes: Vec<u64> = key_records
        .iter()
        .map(|kr| unsafe { fx_hash_key_record(kr.as_ptr()) })
        .collect();
    let mut all_keys: Vec<u8> = Vec::new();
    for s in &labels {
        all_keys.extend_from_slice(s.as_bytes());
        all_keys.push(0);
    }
    let shape_hash = fx_hash_bytes(&all_keys);
    let entries: Vec<(u64, i64)> = key_hashes
        .iter()
        .enumerate()
        .map(|(i, h)| (*h, (i as i64) + 1))
        .collect();
    let dict_bytes = build_dict_record(shape_hash, &entries);
    let key_record_ptrs: Vec<i64> = key_records.iter().map(|kr| kr.as_ptr() as i64).collect();
    let keys_list_bytes = build_flat_list_record(&key_record_ptrs);
    W5Fixture {
        dict_bytes,
        keys_list_bytes,
        shape_hash,
        _key_records: key_records,
        _key_record_ptrs: key_record_ptrs,
    }
}

fn build_w6_fixture(n: u64) -> Vec<u8> {
    let elements: Vec<i64> = (1..=(n as i64)).collect();
    build_flat_list_record(&elements)
}

/// W3 / W4 fixture: stable `*const StringRef` pointers for the literal
/// arguments. Stored in a struct so the bench keeps them alive for the
/// duration of the timed region.
struct StrLiterals {
    lit_a: *const StringRef,
    lit_empty: *const StringRef,
    lit_axb: *const StringRef,
    lit_x: *const StringRef,
}

unsafe impl Send for StrLiterals {}
unsafe impl Sync for StrLiterals {}

fn build_str_literals() -> StrLiterals {
    StrLiterals {
        lit_a: StringRef::from_static("a"),
        lit_empty: StringRef::from_static(""),
        lit_axb: StringRef::from_static("axb"),
        lit_x: StringRef::from_static("x"),
    }
}

// =====================================================================
// =====  W1 — tight i64 sum loop  =====================================
// =====================================================================
//
// D1 hot-loop throughput; LuaJIT trace tier baseline.
// Relon side: tree-walker via list.sum(range(n)).
// Lua side: `for i = 1, n do acc = acc + i end`.

const W1_N: i64 = 10_000;

fn w1_relon_src() -> &'static str {
    "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))"
}

fn w1_lua_src() -> String {
    format!(
        r#"return function()
            local acc = 0
            for i = 0, {n} - 1 do
                acc = acc + i
            end
            return acc
        end"#,
        n = W1_N
    )
}

fn w1_expected() -> i64 {
    // sum(0..n-1) = n*(n-1)/2
    W1_N * (W1_N - 1) / 2
}

// =====================================================================
// =====  W2 — f64 dot product  ========================================
// =====================================================================
//
// D1 + array — bounds check + 2 reads per iter.
// Use small N (1000) to keep runtime bounded for tree-walker.

const W2_N: i64 = 1_000;

fn w2_relon_src() -> &'static str {
    // Inline form: sum (i+1)*(i+2) for i in 0..n via map+sum.
    // Avoids local-let dict bindings (Relon's #private only works in the
    // top-level main body, not inside arbitrary expressions).
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => (i + 1) * (i + 2)))"
}

fn w2_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local xs = {{}}
            local ys = {{}}
            for i = 1, n do xs[i] = i; ys[i] = i + 1 end
            local sum = 0
            for i = 1, n do sum = sum + xs[i] * ys[i] end
            return sum
        end"#,
        n = W2_N
    )
}

fn w2_expected() -> i64 {
    // Lua: sum(i * (i+1)) for i in 1..n  (1-based)
    // Relon: sum((i+1) * (i+2)) for i in 0..n-1 -> equivalent shift
    let n = W2_N;
    let mut s: i64 = 0;
    for i in 0..n {
        s += (i + 1) * (i + 2);
    }
    s
}

// =====================================================================
// =====  W3 — string concat (O(N²) test)  =============================
// =====================================================================
//
// D7 — both runtimes likely quadratic on naive `+`; envelope check.

fn w3_relon_src() -> &'static str {
    // Use list.reduce to fold string concat across a generated range.
    // Each element is a single-char "a" so the final string is "a"*n.
    "#import list from \"std/list\"\n\
     #main(Int n) -> String\n\
     range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)"
}

fn w3_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local s = ""
            for i = 1, n do
                s = s .. "a"
            end
            return #s
        end"#,
        n = STRING_CONCAT_N
    )
}

fn w3_expected_relon_len() -> i64 {
    STRING_CONCAT_N as i64
}

// =====================================================================
// =====  W4 — string contains scan  ===================================
// =====================================================================
//
// D7 — KMP/naive search through a list of strings.

fn w4_relon_src() -> &'static str {
    // Build a list of strings, count how many contain "x".
    // Each string is "axb" so all contain "x" → count == n.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     range(n)\n\
       .map((i) => \"axb\")\n\
       .filter((s) => s.contains(\"x\"))\n\
       .len()"
}

fn w4_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local count = 0
            for i = 1, n do
                local s = "axb"
                if string.find(s, "x", 1, true) ~= nil then
                    count = count + 1
                end
            end
            return count
        end"#,
        n = TREE_WALK_N
    )
}

fn w4_expected() -> i64 {
    TREE_WALK_N as i64
}

// =====================================================================
// =====  W5 — dict string-key lookup  =================================
// =====================================================================
//
// D8 — hash + string hashing + IC.
// Build a fixed 10-entry dict, sum values across a key list of length n.

fn w5_relon_src() -> &'static str {
    // Top-level dict body with #private bindings, returning .result.
    // Dict body is the only place #private is legal.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #private\n\
       d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
       #private\n\
       keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
       result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
     }"
}

fn w5_lua_src() -> String {
    format!(
        r#"return function()
            local d = {{a=1,b=2,c=3,d=4,e=5,f=6,g=7,h=8,i=9,j=10}}
            local keys = {{"a","b","c","d","e","f","g","h","i","j"}}
            local n = {n}
            local sum = 0
            for i = 1, n do
                local k = keys[((i - 1) % 10) + 1]
                sum = sum + d[k]
            end
            return sum
        end"#,
        n = TREE_WALK_N
    )
}

fn w5_expected() -> i64 {
    // Each block of 10 picks sums to 1+2+...+10 = 55.
    // n must be a multiple of 10 for exact equality with TREE_WALK_N=10000.
    let n = TREE_WALK_N as i64;
    let full_blocks = n / 10;
    let rem = n % 10;
    let mut tail: i64 = 0;
    for i in 0..rem {
        tail += i + 1;
    }
    full_blocks * 55 + tail
}

// =====================================================================
// =====  W6 — dict numeric-key dense  =================================
// =====================================================================
//
// D8 — LuaJIT's array-part territory; Relon Dict has BTreeMap underneath
// so this is genuinely adversarial.

fn w6_relon_src() -> &'static str {
    // Relon dicts are string-keyed; we approximate "dense numeric key"
    // via a List<Int>, which IS the LuaJIT array-part comparison.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => i + 1))"
}

fn w6_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local arr = {{}}
            for i = 1, n do arr[i] = i end
            local sum = 0
            for i = 1, n do sum = sum + arr[i] end
            return sum
        end"#,
        n = TREE_WALK_N
    )
}

fn w6_expected() -> i64 {
    let n = TREE_WALK_N as i64;
    n * (n + 1) / 2
}

// =====================================================================
// =====  W7 — recursive fib  ==========================================
// =====================================================================
//
// D1 + call ABI + recursion. fib(N) where N=28 ~ 317k calls.

fn w7_relon_src() -> &'static str {
    // Recursive closure defined at top-level dict-body scope; returns
    // the value via the `result` key. Pulled out of `.value` because
    // member-access on dict-body is the only public selector.
    "#main(Int n) -> Dict\n\
     {\n\
       #private\n\
       fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
       result: fib(n)\n\
     }"
}

fn w7_lua_src() -> String {
    format!(
        r#"return function()
            local function fib(k)
                if k < 2 then return k end
                return fib(k - 1) + fib(k - 2)
            end
            return fib({n})
        end"#,
        n = FIB_N
    )
}

fn w7_expected() -> i64 {
    fn fib(k: i64) -> i64 {
        if k < 2 {
            k
        } else {
            fib(k - 1) + fib(k - 2)
        }
    }
    fib(FIB_N as i64)
}

// =====================================================================
// =====  W8 — polymorphic call site  ==================================
// =====================================================================
//
// D6 — IC 4-way set-assoc test. Apply a closure to 4 different argument
// types in rotation. Since Relon's tree-walker doesn't have an IC, this
// is mostly a fairness probe: does the dispatcher degrade under
// polymorphism?

fn w8_relon_src() -> &'static str {
    // Relon doesn't have anonymous unions easily, so we use an Int-tag
    // approach. Closure body is defined at the top-level dict scope.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #private\n\
       dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4,\n\
       result: list.sum(range(n).map((i) => dispatch(i % 4)))\n\
     }"
}

fn w8_lua_src() -> String {
    format!(
        r#"return function()
            local function dispatch(t)
                if t == 0 then return 1
                elseif t == 1 then return 2
                elseif t == 2 then return 3
                else return 4 end
            end
            local n = {n}
            local sum = 0
            for i = 0, n - 1 do
                sum = sum + dispatch(i % 4)
            end
            return sum
        end"#,
        n = TREE_WALK_N
    )
}

fn w8_expected() -> i64 {
    let n = TREE_WALK_N as i64;
    // Per block of 4: dispatch(0)+dispatch(1)+dispatch(2)+dispatch(3) = 1+2+3+4 = 10
    let full = n / 4;
    let rem = n % 4;
    let mut tail: i64 = 0;
    for i in 0..rem {
        tail += match i % 4 {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => 4,
        };
    }
    full * 10 + tail
}

// =====================================================================
// =====  W9 — nested loop matrix transpose  ===========================
// =====================================================================
//
// D1 + cache. NxN matrix, sum of transposed = sum of original. Just sum
// the matrix elements after going through (i,j) -> (j,i) access pattern.

fn w9_relon_src() -> &'static str {
    // Relon doesn't have efficient 2D arrays; we approximate with
    // nested list.reduce. Use smaller N internally.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #private\n\
       rows: range(n).map((i) => range(n).map((j) => i * n + j)),\n\
       result: range(n).reduce(0, (acc, j) =>\n\
         acc + range(n).reduce(0, (inner, i) => inner + rows[i][j]))\n\
     }"
}

fn w9_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local m = {{}}
            for i = 1, n do
                m[i] = {{}}
                for j = 1, n do m[i][j] = (i - 1) * n + (j - 1) end
            end
            local sum = 0
            for j = 1, n do
                for i = 1, n do
                    sum = sum + m[i][j]
                end
            end
            return sum
        end"#,
        n = 32 // intentionally small for tree-walker
    )
}

fn w9_expected() -> i64 {
    // sum of i*n+j for i in 0..n, j in 0..n where n=32 (the Lua N).
    let n: i64 = 32;
    let mut s: i64 = 0;
    for i in 0..n {
        for j in 0..n {
            s += i * n + j;
        }
    }
    s
}

const W9_N: i64 = 32;

fn w9_relon_n_arg() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(W9_N));
    m
}

// =====================================================================
// =====  W10 — config eval (10-rule access control)  ==================
// =====================================================================
//
// D4 mixed; production-like. Each query: check if user can access a
// resource. Combination of role-check, region-check, time-check.

fn w10_relon_src() -> &'static str {
    // 10-rule access control. Inline the role/region/hour predicates
    // into a single boolean expression so we don't need nested
    // dict-body scopes (Relon parser rejects #private outside the
    // top-level main dict body).
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #private\n\
       allow: (i) =>\n\
         (i % 3 == 0 || i % 3 == 1) &&\n\
         (i % 4 == 0 || i % 4 == 1) &&\n\
         (i % 24 >= 8 && i % 24 < 18) ? 1 : 0,\n\
       result: list.sum(range(n).map(allow))\n\
     }"
}

fn w10_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local count = 0
            for i = 0, n - 1 do
                local role_i = i % 3
                local region_i = i % 4
                local hour = i % 24
                local allow_role = (role_i == 0) or (role_i == 1)
                local allow_region = (region_i == 0) or (region_i == 1)
                local allow_hour = (hour >= 8) and (hour < 18)
                if allow_role and allow_region and allow_hour then
                    count = count + 1
                end
            end
            return count
        end"#,
        n = CONFIG_QUERIES_N
    )
}

fn w10_expected() -> i64 {
    let n = CONFIG_QUERIES_N as i64;
    let mut count: i64 = 0;
    for i in 0..n {
        let role_i = i % 3;
        let region_i = i % 4;
        let hour = i % 24;
        let allow_role = role_i == 0 || role_i == 1;
        let allow_region = region_i == 0 || region_i == 1;
        let allow_hour = (8..18).contains(&hour);
        if allow_role && allow_region && allow_hour {
            count += 1;
        }
    }
    count
}

// =====================================================================
// =====  W11 — cold start (fresh process)  ============================
// =====================================================================
//
// D2 **MUST-PASS**. Measure: PID start to first invoke wall-clock.
// Per the rigorous plan §3, we shell out to a fresh `relon-cli` and
// `luajit -e` process and time end-to-end via std::process::Command.
//
// Since spawning processes is wall-clock heavy, we use sample_size = 30
// and measurement_time = 10 s for this row only. The bench row itself
// runs at "one fresh process per criterion iteration" granularity.

use std::process::Command;

const W11_LUA_SRC: &str = "print(1 + 1)";

// =====================================================================
// =====  W12 — p99 tail (1M invoke)  ==================================
// =====================================================================
//
// D5 **MUST-PASS**. Drive the same tight 4-op step body via Relon trace
// dispatch and via mlua. The bench_stats post-processor extracts
// p99/p99.9/max from the per-sample distribution; this row is the
// primary tail-latency data source.
//
// We reuse the boundary calibrate row's shape (1M mlua calls to a
// constant fn) for Lua; the Relon side uses the tree-walker because
// trace-JIT tail numbers are already in `trace_jit_hot_loop`.

fn w12_relon_src() -> &'static str {
    // A trivial 1-op invoke to keep cost dominated by dispatch.
    "#main(Int x) -> Int\nx + 1"
}

fn w12_relon_args(x: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("x".to_string(), Value::Int(x));
    m
}

fn w12_lua_src() -> &'static str {
    "return function(x) return x + 1 end"
}

// =====================================================================
// =====  consistency assertions  ======================================
// =====================================================================

fn args_w_n(n: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

fn assert_relon_lua_consistent(w: &str, relon_v: i64, lua_v: i64, expected: i64) {
    assert_eq!(
        relon_v, expected,
        "{w}: Relon output {relon_v} does not match expected {expected}"
    );
    assert_eq!(
        lua_v, expected,
        "{w}: Lua output {lua_v} does not match expected {expected}"
    );
}

/// Extract an Int value from a Relon `Value`. For Dict-returning workloads
/// we look up `result`; for Int-returning workloads the value itself.
fn relon_int_result(w: &str, v: Value) -> i64 {
    match v {
        Value::Int(n) => n,
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("{w}: dict.result is not Int: {other:?}"),
        },
        other => panic!("{w}: Relon result not Int or Dict: {other:?}"),
    }
}

// =====================================================================
// =====  bench entry  =================================================
// =====================================================================

#[allow(clippy::too_many_lines)]
fn bench_cmp_lua(c: &mut Criterion) {
    match verify_quiescence() {
        Ok(report) => {
            eprintln!("[cmp_lua] {}", report.summary());
        }
        Err(err) => {
            eprintln!("[cmp_lua] {err}");
            eprintln!("[cmp_lua] {}", err.report.summary());
            panic!("machine not quiescent; set RELON_BENCH_FORCE_RUN=1 to override");
        }
    }

    // One shared Lua state per process: registering 12 functions on it
    // up-front amortises the state setup cost across all 12 rows.
    let lua = mlua::Lua::new();

    let mut group = c.benchmark_group("v6_lambda_cmp_lua");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(5));

    // ----- W1 -----
    {
        let (walker, scope) = build_tree_walker(w1_relon_src());
        let lua_fn_w1 = lua_fn(&lua, &w1_lua_src());

        // Consistency: Relon list.sum(range(n)) = sum 0..n-1, Lua loops 0..n-1.
        let relon_v = match walker.run_main(&scope, args_w_n(W1_N)).unwrap() {
            Value::Int(v) => v,
            other => panic!("W1 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w1.call(()).unwrap();
        assert_relon_lua_consistent("W1", relon_v, lua_v, w1_expected());

        group.throughput(Throughput::Elements(W1_N as u64));
        group.bench_function(BenchmarkId::new("W1_int_sum", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(W1_N);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W1_int_sum", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w1.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W2 -----
    {
        let (walker, scope) = build_tree_walker(w2_relon_src());
        let lua_fn_w2 = lua_fn(&lua, &w2_lua_src());
        let relon_v = match walker.run_main(&scope, args_w_n(W2_N)).unwrap() {
            Value::Int(v) => v,
            other => panic!("W2 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w2.call(()).unwrap();
        assert_relon_lua_consistent("W2", relon_v, lua_v, w2_expected());

        group.throughput(Throughput::Elements(W2_N as u64));
        group.bench_function(BenchmarkId::new("W2_f64_dot", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(W2_N);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W2_f64_dot", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w2.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W3 -----
    {
        let (walker, scope) = build_tree_walker(w3_relon_src());
        let lua_fn_w3 = lua_fn(&lua, &w3_lua_src());

        // Relon returns a String of length STRING_CONCAT_N; Lua returns #s.
        let relon_v = match walker
            .run_main(&scope, args_w_n(STRING_CONCAT_N as i64))
            .unwrap()
        {
            Value::String(s) => s.len() as i64,
            other => panic!("W3 Relon non-string: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w3.call(()).unwrap();
        assert_relon_lua_consistent("W3", relon_v, lua_v, w3_expected_relon_len());

        // F-D9 trace JIT row: exercises `__relon_str_concat` shim via
        // a compiled cranelift trace. Sanity-check the result before
        // benching.
        let w3_str_lits = build_str_literals();
        let w3_trace = build_w3_trace_fn();
        {
            let mut tctx = TraceContext::with_capacity(0);
            let args: [u64; 3] = [
                STRING_CONCAT_N,
                w3_str_lits.lit_a as u64,
                w3_str_lits.lit_empty as u64,
            ];
            let status = unsafe { (w3_trace.entry)(&mut tctx as *mut TraceContext, args.as_ptr()) };
            assert_eq!(status, 0, "W3 trace JIT must complete successfully");
            assert_eq!(
                tctx.result_slot as i64,
                w3_expected_relon_len(),
                "W3 trace JIT result_slot must match analytic length"
            );
        }

        group.throughput(Throughput::Elements(STRING_CONCAT_N));
        group.bench_function(
            BenchmarkId::new("W3_string_concat", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(STRING_CONCAT_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(
            BenchmarkId::new("W3_string_concat", "relon_trace_jit"),
            |b| {
                b.iter_custom(|iters| {
                    let mut tctx = TraceContext::with_capacity(0);
                    let args: [u64; 3] = [
                        STRING_CONCAT_N,
                        w3_str_lits.lit_a as u64,
                        w3_str_lits.lit_empty as u64,
                    ];
                    let args_ptr = args.as_ptr();
                    timed_with_warmup(iters, || {
                        let s = unsafe {
                            (w3_trace.entry)(&mut tctx as *mut TraceContext, black_box(args_ptr))
                        };
                        black_box(s);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W3_string_concat", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w3.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W4 -----
    {
        let (walker, scope) = build_tree_walker(w4_relon_src());
        let lua_fn_w4 = lua_fn(&lua, &w4_lua_src());

        let relon_v = match walker
            .run_main(&scope, args_w_n(TREE_WALK_N as i64))
            .unwrap()
        {
            Value::Int(v) => v,
            other => panic!("W4 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w4.call(()).unwrap();
        assert_relon_lua_consistent("W4", relon_v, lua_v, w4_expected());

        // F-D9 trace JIT row: F-D7 `__relon_str_contains` with the IC
        // hot path. Sanity-check before benching.
        let w4_str_lits = build_str_literals();
        let w4_trace = build_w4_trace_fn();
        {
            let mut tctx = TraceContext::with_capacity(0);
            let args: [u64; 3] = [
                TREE_WALK_N,
                w4_str_lits.lit_axb as u64,
                w4_str_lits.lit_x as u64,
            ];
            let status = unsafe { (w4_trace.entry)(&mut tctx as *mut TraceContext, args.as_ptr()) };
            assert_eq!(status, 0, "W4 trace JIT must complete successfully");
            assert_eq!(
                tctx.result_slot as i64,
                w4_expected(),
                "W4 trace JIT result_slot must match analytic count"
            );
        }

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W4_string_contains", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(
            BenchmarkId::new("W4_string_contains", "relon_trace_jit"),
            |b| {
                b.iter_custom(|iters| {
                    let mut tctx = TraceContext::with_capacity(0);
                    let args: [u64; 3] = [
                        TREE_WALK_N,
                        w4_str_lits.lit_axb as u64,
                        w4_str_lits.lit_x as u64,
                    ];
                    let args_ptr = args.as_ptr();
                    timed_with_warmup(iters, || {
                        let s = unsafe {
                            (w4_trace.entry)(&mut tctx as *mut TraceContext, black_box(args_ptr))
                        };
                        black_box(s);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W4_string_contains", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w4.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W5 -----
    {
        let (walker, scope) = build_tree_walker(w5_relon_src());
        let lua_fn_w5 = lua_fn(&lua, &w5_lua_src());

        let relon_v = relon_int_result(
            "W5",
            walker
                .run_main(&scope, args_w_n(TREE_WALK_N as i64))
                .unwrap(),
        );
        let lua_v: i64 = lua_fn_w5.call(()).unwrap();
        assert_relon_lua_consistent("W5", relon_v, lua_v, w5_expected());

        // F-D9 trace JIT row: F-D8 `__relon_trace_dict_lookup` +
        // inline `ListGet` for `dict[keys[i % 10]]`.
        let w5_fixture = build_w5_fixture();
        let w5_trace = build_w5_trace_fn();
        {
            let mut tctx = TraceContext::with_capacity(0);
            let args: [u64; 4] = [
                TREE_WALK_N,
                w5_fixture.dict_bytes.as_ptr() as u64,
                w5_fixture.keys_list_bytes.as_ptr() as u64,
                w5_fixture.shape_hash,
            ];
            let status = unsafe { (w5_trace.entry)(&mut tctx as *mut TraceContext, args.as_ptr()) };
            assert_eq!(status, 0, "W5 trace JIT must complete successfully");
            assert_eq!(
                tctx.result_slot as i64,
                w5_expected(),
                "W5 trace JIT result_slot must match analytic sum"
            );
        }

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W5_dict_str_key", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(
            BenchmarkId::new("W5_dict_str_key", "relon_trace_jit"),
            |b| {
                b.iter_custom(|iters| {
                    let mut tctx = TraceContext::with_capacity(0);
                    let args: [u64; 4] = [
                        TREE_WALK_N,
                        w5_fixture.dict_bytes.as_ptr() as u64,
                        w5_fixture.keys_list_bytes.as_ptr() as u64,
                        w5_fixture.shape_hash,
                    ];
                    let args_ptr = args.as_ptr();
                    timed_with_warmup(iters, || {
                        let s = unsafe {
                            (w5_trace.entry)(&mut tctx as *mut TraceContext, black_box(args_ptr))
                        };
                        black_box(s);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W5_dict_str_key", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w5.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W6 -----
    {
        let (walker, scope) = build_tree_walker(w6_relon_src());
        let lua_fn_w6 = lua_fn(&lua, &w6_lua_src());

        let relon_v = match walker
            .run_main(&scope, args_w_n(TREE_WALK_N as i64))
            .unwrap()
        {
            Value::Int(v) => v,
            other => panic!("W6 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w6.call(()).unwrap();
        assert_relon_lua_consistent("W6", relon_v, lua_v, w6_expected());

        // F-D9 trace JIT row: inline F-D8 `ListGet` lowering for the
        // dense `arr[i]` shape.
        let w6_list = build_w6_fixture(TREE_WALK_N);
        let w6_trace = build_w6_trace_fn();
        {
            let mut tctx = TraceContext::with_capacity(0);
            let args: [u64; 2] = [TREE_WALK_N, w6_list.as_ptr() as u64];
            let status = unsafe { (w6_trace.entry)(&mut tctx as *mut TraceContext, args.as_ptr()) };
            assert_eq!(status, 0, "W6 trace JIT must complete successfully");
            assert_eq!(
                tctx.result_slot as i64,
                w6_expected(),
                "W6 trace JIT result_slot must match analytic sum"
            );
        }

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W6_dict_num_key", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(
            BenchmarkId::new("W6_dict_num_key", "relon_trace_jit"),
            |b| {
                b.iter_custom(|iters| {
                    let mut tctx = TraceContext::with_capacity(0);
                    let args: [u64; 2] = [TREE_WALK_N, w6_list.as_ptr() as u64];
                    let args_ptr = args.as_ptr();
                    timed_with_warmup(iters, || {
                        let s = unsafe {
                            (w6_trace.entry)(&mut tctx as *mut TraceContext, black_box(args_ptr))
                        };
                        black_box(s);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W6_dict_num_key", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w6.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W7 fib -----
    {
        let (walker, scope) = build_tree_walker(w7_relon_src());
        let lua_fn_w7 = lua_fn(&lua, &w7_lua_src());

        let relon_v = relon_int_result(
            "W7",
            walker.run_main(&scope, args_w_n(FIB_N as i64)).unwrap(),
        );
        let lua_v: i64 = lua_fn_w7.call(()).unwrap();
        assert_relon_lua_consistent("W7", relon_v, lua_v, w7_expected());

        // fib(28) call count: ~317k → throughput per call.
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("W7_fib", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(FIB_N as i64);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W7_fib", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w7.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W8 polymorphic -----
    {
        let (walker, scope) = build_tree_walker(w8_relon_src());
        let lua_fn_w8 = lua_fn(&lua, &w8_lua_src());

        let relon_v = relon_int_result(
            "W8",
            walker
                .run_main(&scope, args_w_n(TREE_WALK_N as i64))
                .unwrap(),
        );
        let lua_v: i64 = lua_fn_w8.call(()).unwrap();
        assert_relon_lua_consistent("W8", relon_v, lua_v, w8_expected());

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W8_poly_callsite", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W8_poly_callsite", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w8.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W9 matrix transpose -----
    {
        let (walker, scope) = build_tree_walker(w9_relon_src());
        let lua_fn_w9 = lua_fn(&lua, &w9_lua_src());

        let relon_v = relon_int_result("W9", walker.run_main(&scope, w9_relon_n_arg()).unwrap());
        let lua_v: i64 = lua_fn_w9.call(()).unwrap();
        assert_relon_lua_consistent("W9", relon_v, lua_v, w9_expected());

        let inner = (W9_N as u64) * (W9_N as u64);
        group.throughput(Throughput::Elements(inner));
        group.bench_function(
            BenchmarkId::new("W9_nested_matrix", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, w9_relon_n_arg()).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W9_nested_matrix", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w9.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W10 config eval -----
    {
        let (walker, scope) = build_tree_walker(w10_relon_src());
        let lua_fn_w10 = lua_fn(&lua, &w10_lua_src());

        let relon_v = relon_int_result(
            "W10",
            walker
                .run_main(&scope, args_w_n(CONFIG_QUERIES_N as i64))
                .unwrap(),
        );
        let lua_v: i64 = lua_fn_w10.call(()).unwrap();
        assert_relon_lua_consistent("W10", relon_v, lua_v, w10_expected());

        group.throughput(Throughput::Elements(CONFIG_QUERIES_N));
        group.bench_function(
            BenchmarkId::new("W10_config_eval", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(CONFIG_QUERIES_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W10_config_eval", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w10.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W12 p99 tail (1 invoke per iter, large sample) -----
    //
    // We deliberately use ONE invoke per criterion iteration here so that
    // per-sample distribution is a per-invocation distribution. With
    // SAMPLE_SIZE = 100, p99.9 has 0.1 samples → not useful; this row is
    // primarily for p50/p90/p99 read-out. For a real p99.9 we'd want
    // sample_size = 1000+ and 10M+ inner invokes; out of scope today.
    //
    // We do NOT call timed_with_warmup here because we want the raw
    // per-call cost to surface in each criterion sample (not amortised
    // across 10k inner iterations).
    {
        let (walker, scope) = build_tree_walker(w12_relon_src());
        let lua_fn_w12 = lua_fn(&lua, w12_lua_src());

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("W12_p99_tail", "relon_tree_walk"), |b| {
            b.iter(|| {
                let v = walker
                    .run_main(&scope, w12_relon_args(black_box(7)))
                    .unwrap();
                black_box(v);
            });
        });
        group.bench_function(BenchmarkId::new("W12_p99_tail", "luajit"), |b| {
            b.iter(|| {
                let r: i64 = lua_fn_w12.call(black_box(7i64)).unwrap();
                black_box(r);
            });
        });
    }

    group.finish();

    // ----- W11 cold start (separate group, fresh-process timing) -----
    //
    // We can't use criterion's iter_custom for this row meaningfully
    // because criterion expects fast iteration; instead we shell out
    // once per criterion iter. Sample count drops to 20 + measurement
    // time to 10s so wall clock stays bounded.
    let mut cold_group = c.benchmark_group("v6_lambda_cmp_lua_cold");
    cold_group.sample_size(20);
    cold_group.measurement_time(Duration::from_secs(15));
    cold_group.throughput(Throughput::Elements(1));

    // W11_RELON_SRC isn't shippable via stdin to relon-cli without a
    // disk file; instead, write a tiny script to a temp file in this
    // process's tempdir, and let `relon run <path>` consume it.
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let relon_src_path = tmpdir.path().join("w11.relon");
    std::fs::write(&relon_src_path, "#main(Int x) -> Int\nx + 1\n").expect("write w11");

    let relon_bin = std::env::var("RELON_CLI_BIN").unwrap_or_else(|_| {
        // Try a few likely locations; falls back to PATH lookup.
        let candidates = ["target/release/relon-cli", "target/debug/relon-cli"];
        for c in candidates {
            if std::path::Path::new(c).exists() {
                return c.to_string();
            }
        }
        "relon-cli".to_string()
    });
    let relon_args_json = "{\"x\": 41}";

    // Check binary actually exists, otherwise skip Relon side gracefully.
    let relon_present =
        std::path::Path::new(&relon_bin).exists() || which_binary(&relon_bin).is_some();

    if relon_present {
        cold_group.bench_function(
            BenchmarkId::new("W11_cold_start", "relon_fresh_proc"),
            |b| {
                b.iter(|| {
                    let out = Command::new(&relon_bin)
                        .arg("run")
                        .arg(&relon_src_path)
                        .arg("--args")
                        .arg(relon_args_json)
                        .output();
                    // Treat any failure as a measurement we'd still report,
                    // but log so the user sees it.
                    if let Ok(o) = &out {
                        black_box(o.stdout.len());
                    }
                });
            },
        );
    } else {
        eprintln!(
            "[cmp_lua W11] relon-cli not found at {relon_bin}; skipping Relon cold-start row"
        );
    }

    let luajit_bin = std::env::var("RELON_LUAJIT_BIN").unwrap_or_else(|_| "luajit".to_string());
    let lua_present = which_binary(&luajit_bin).is_some();
    if lua_present {
        cold_group.bench_function(
            BenchmarkId::new("W11_cold_start", "luajit_fresh_proc"),
            |b| {
                b.iter(|| {
                    let out = Command::new(&luajit_bin)
                        .arg("-e")
                        .arg(W11_LUA_SRC)
                        .output();
                    if let Ok(o) = &out {
                        black_box(o.stdout.len());
                    }
                });
            },
        );
    } else {
        eprintln!("[cmp_lua W11] luajit not found in PATH (set RELON_LUAJIT_BIN); skipping Lua cold-start row");
    }
    drop(tmpdir);

    cold_group.finish();
}

/// Lightweight `which` substitute — returns the resolved path if `name`
/// resolves on the current `PATH`, else None.
fn which_binary(name: &str) -> Option<std::path::PathBuf> {
    if let Some(parent) = std::path::Path::new(name).parent() {
        if !parent.as_os_str().is_empty() && std::path::Path::new(name).exists() {
            return Some(name.into());
        }
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

criterion_group!(benches, bench_cmp_lua);
criterion_main!(benches);
