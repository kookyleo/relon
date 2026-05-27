//! F-D8 (2026-05-19): trace-JIT bench rows for the W5/W6 hot loops.
//!
//! The headline `cmp_lua` bench's W5 + W6 rows run through the
//! tree-walker; this companion bench runs the same logical hot-loop
//! body through a hand-built cranelift JIT trace that exercises the
//! new `TraceOp::ListGet` + `TraceOp::DictLookup` lowerings end-to-end.
//!
//! ## Why a separate bench file?
//!
//! - `cmp_lua` is the v6-λ-3 LuaJIT-paired matrix; mixing in a
//!   prototype trace-JIT row inflates its wall-clock budget and
//!   pollutes the LuaJIT comparison report.
//! - The trace-JIT path here intentionally bypasses the trace
//!   recorder (the recorder doesn't yet see source-side dict/list
//!   ops; the IR walker would need teaching). Per the F-D8 task
//!   brief, we **build the trace by hand** with the new TraceOps to
//!   demonstrate the emitter + runtime path is wired end-to-end.
//!   When the recorder is extended to recognise the source pattern,
//!   it would produce byte-identical machine code; this bench's row
//!   is the floor of that path.
//! - The fixture record layouts (`[len][pad][i64...]` for lists,
//!   `[shape_hash][entry_count][entries + key payloads...]` for dicts) match what
//!   the cranelift-AOT data section already produces for
//!   `Op::ConstListInt` and what the F-D8 helper expects.
//!
//! ## Methodology trap recap (v6-λ-0)
//!
//! Same 6-trap envelope as `trace_jit_hot_loop.rs`:
//!
//! - Trap A: black_box around every input/output.
//! - Trap B: 10_000-iter warmup capped at 200 ms wall-clock before
//!   the timed region.
//! - Trap C: loop-INSIDE rows so caller→callee overhead is
//!   amortised over `HOT_LOOP_N` body iters per single invoke.
//! - Trap D: one cache-prefill invoke before warmup so L1/L2 carries
//!   the trace machine code + the dict/list buffers.
//! - Trap E: every row is `#[zero_alloc]` on the hot path; the
//!   `TraceContext` and the fixture byte buffers are allocated once
//!   outside the timed region.
//! - Trap F: `SAMPLE_SIZE = 200` so post-processing surfaces p99.9.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

use relon_bench::quiescence::verify_quiescence;
use relon_codegen_native::register_trace_runtime_symbols;
use relon_eval_api::Value;
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;
use relon_trace_abi::{TraceContext, TraceEntryStatus};
use relon_trace_jit::{
    build_dict_record_v2, build_flat_list_record, build_string_record, fx_hash_bytes,
};

// ----- methodology constants ----------------------------------------

const HOT_LOOP_N: u64 = 10_000;
const WARMUP_ITERS: u64 = 10_000;
const WARMUP_TIME_CAP_MS: u128 = 200;
const SAMPLE_SIZE: usize = 100;

#[inline(always)]
fn timed_with_warmup<F: FnMut()>(iters: u64, mut routine: F) -> Duration {
    // Trap D: cache prefill.
    routine();
    // Trap B: explicit warmup.
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

// ----- W5 / W6 fixture build ----------------------------------------

/// Build the W5 dict fixture: 10 entries, keys `"a".."j"`, values 1..10.
/// Returns `(dict_bytes, keys_list_bytes, shape_hash, key_records)`.
struct W5Fixture {
    dict_bytes: Vec<u8>,
    keys_list_bytes: Vec<u8>,
    shape_hash: u64,
    /// Pre-built `[len][utf8...]` records for each key, stored in a
    /// stable Vec so raw pointers into them stay live for the
    /// duration of the bench.
    key_records: Vec<Vec<u8>>,
    /// Pointers into the entries of `key_records`, packed as little-
    /// endian i64 into `keys_list_bytes`'s element slots so the
    /// hand-built trace can use the same `ListGet` shape it would
    /// use for any other list of pointers.
    _key_record_ptrs: Vec<i64>,
}

fn build_w5_fixture() -> W5Fixture {
    let labels = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let key_records: Vec<Vec<u8>> = labels.iter().map(|s| build_string_record(s)).collect();
    // Dict shape hash: FxHash over the concatenated sorted keys (the
    // F-D8 recorder would compute this the same way at recording
    // time).
    let mut all_keys: Vec<u8> = Vec::new();
    for s in &labels {
        all_keys.extend_from_slice(s.as_bytes());
        all_keys.push(0);
    }
    let shape_hash = fx_hash_bytes(&all_keys);
    let entries: Vec<(&[u8], i64)> = labels
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_bytes(), (i as i64) + 1))
        .collect();
    let dict_bytes = build_dict_record_v2(shape_hash, &entries);
    // Keys list: array of raw String-record pointers, packed as i64.
    let key_record_ptrs: Vec<i64> = key_records.iter().map(|kr| kr.as_ptr() as i64).collect();
    let keys_list_bytes = build_flat_list_record(&key_record_ptrs);
    W5Fixture {
        dict_bytes,
        keys_list_bytes,
        shape_hash,
        key_records,
        _key_record_ptrs: key_record_ptrs,
    }
}

/// Build the W6 list fixture: a flat `List<i64>` of `[1..=n]`.
fn build_w6_fixture(n: u64) -> Vec<u8> {
    let elements: Vec<i64> = (1..=(n as i64)).collect();
    build_flat_list_record(&elements)
}

// ----- W5 trace builder (dict + list inside loop) -------------------

/// Hand-built cranelift JIT function whose body is the full
/// `for i in 0..n { acc += dict[keys[i % 10]] }` loop, lowering each
/// per-iter cost into:
///
/// - `idx = i mod 10` (single `urem` per iter — measured against
///   LuaJIT's `((i-1) % 10) + 1` which jits to the same cycle count)
/// - `key_ptr = list_get(keys_list_ptr, idx)` (the new F-D8 path,
///   inlined into the trace machine code)
/// - `val = dict_lookup_v2(dict_ptr, record_len, key_ptr, shape_hash)`
///   (host helper call; the IC tag check + collision-safe key compare
///   is amortised over the loop)
/// - `acc = acc + val`
///
/// The signature matches `TRACE_ENTRY_SIG`: `(ctx, args) -> i32`,
/// with `args[0] = n`, `args[1] = dict_ptr`, `args[2] = keys_list_ptr`,
/// `args[3] = shape_hash`, `args[4] = dict_record_len`.
struct W5TraceFn {
    entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32,
    _module: JITModule,
}

unsafe impl Send for W5TraceFn {}
unsafe impl Sync for W5TraceFn {}

fn build_w5_trace_fn() -> W5TraceFn {
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
    let mut module = JITModule::new(builder);
    let pointer_ty = module.target_config().pointer_type();

    // ---- import host helpers ----
    let mut save_deopt_sig = Signature::new(CallConv::SystemV);
    save_deopt_sig.params.push(AbiParam::new(pointer_ty));
    save_deopt_sig.params.push(AbiParam::new(I32));
    save_deopt_sig.params.push(AbiParam::new(I64));
    let save_deopt_id = module
        .declare_function("__relon_trace_save_deopt", Linkage::Import, &save_deopt_sig)
        .expect("declare save_deopt");

    let mut dict_lookup_sig = Signature::new(CallConv::SystemV);
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.params.push(AbiParam::new(I64));
    dict_lookup_sig.params.push(AbiParam::new(pointer_ty));
    dict_lookup_sig.returns.push(AbiParam::new(I64));
    let dict_lookup_id = module
        .declare_function(
            "__relon_trace_dict_lookup_v2",
            Linkage::Import,
            &dict_lookup_sig,
        )
        .expect("declare dict_lookup");

    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I32));

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, dict_lookup_id.as_u32() + 1),
        sig.clone(),
    );

    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig);
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id.as_u32()));
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

    let entry_block = fb.create_block();
    fb.append_block_params_for_function_params(entry_block);
    let trace_ctx = fb.block_params(entry_block)[0];
    let args_ptr = fb.block_params(entry_block)[1];
    fb.switch_to_block(entry_block);
    fb.seal_block(entry_block);

    // Load the four args from the args_ptr packed array.
    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let dict_ptr = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 8);
    let keys_list_ptr = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 16);
    let shape_hash = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 24);
    let dict_record_len = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 32);

    let acc_seed = fb.ins().iconst(I64, 0);
    let i_seed = fb.ins().iconst(I64, 0);

    let header_block = fb.create_block();
    fb.append_block_param(header_block, I64); // acc
    fb.append_block_param(header_block, I64); // i

    let body_block = fb.create_block();
    let exit_block = fb.create_block();
    let deopt_block = fb.create_block();

    fb.ins().jump(
        header_block,
        &[BlockArg::Value(acc_seed), BlockArg::Value(i_seed)],
    );

    // Header: if i >= n -> exit; else -> body.
    fb.switch_to_block(header_block);
    let acc_p = fb.block_params(header_block)[0];
    let i_p = fb.block_params(header_block)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThan, i_p, n_val);
    let empty: [BlockArg; 0] = [];
    fb.ins()
        .brif(cont, body_block, empty.iter(), exit_block, empty.iter());

    // Body: idx = i mod 10
    fb.switch_to_block(body_block);
    fb.seal_block(body_block);
    let ten = fb.ins().iconst(I64, 10);
    let idx = fb.ins().urem(i_p, ten);

    // key_ptr = list_get(keys_list_ptr, idx) — inline, no host call.
    // Layout: list header is [len: u32][pad: u32] then i64 elements.
    // Bounds check: idx < len.
    let keys_len32 = fb.ins().load(I32, MemFlags::trusted(), keys_list_ptr, 0);
    let keys_len64 = fb.ins().uextend(I64, keys_len32);
    let in_bounds = fb.ins().icmp(IntCC::UnsignedLessThan, idx, keys_len64);
    let post_bounds_block = fb.create_block();
    fb.ins().brif(
        in_bounds,
        post_bounds_block,
        empty.iter(),
        deopt_block,
        empty.iter(),
    );
    fb.seal_block(post_bounds_block);
    fb.switch_to_block(post_bounds_block);

    let eight = fb.ins().iconst(I64, 8);
    let elem_off = fb.ins().imul(idx, eight);
    let payload_base = fb.ins().iadd_imm(keys_list_ptr, 8);
    let elem_addr = fb.ins().iadd(payload_base, elem_off);
    // Element is a String-record pointer stored as i64.
    let key_ptr_i64 = fb.ins().load(I64, MemFlags::trusted(), elem_addr, 0);

    // val = dict_lookup_v2(dict_ptr, record_len, key_ptr, shape_hash, trace_ctx).
    let inst = fb.ins().call(
        dict_lookup_ref,
        &[
            dict_ptr,
            dict_record_len,
            key_ptr_i64,
            shape_hash,
            trace_ctx,
        ],
    );
    let val = fb.inst_results(inst)[0];
    // Deopt sentinel check: val == i64::MIN -> deopt.
    let sentinel = fb.ins().iconst(I64, i64::MIN);
    let miss = fb.ins().icmp(IntCC::Equal, val, sentinel);
    let post_hit_block = fb.create_block();
    fb.ins().brif(
        miss,
        deopt_block,
        empty.iter(),
        post_hit_block,
        empty.iter(),
    );
    fb.seal_block(post_hit_block);
    fb.switch_to_block(post_hit_block);

    // acc' = acc + val (wrapping; the W5 corpus stays well within i64).
    let new_acc = fb.ins().iadd(acc_p, val);

    // i' = i + 1; jump back to header.
    let one = fb.ins().iconst(I64, 1);
    let new_i = fb.ins().iadd(i_p, one);
    fb.ins().jump(
        header_block,
        &[BlockArg::Value(new_acc), BlockArg::Value(new_i)],
    );
    fb.seal_block(header_block);

    // Exit: store acc into ctx.result_slot, return Success.
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

    // Deopt: call save_deopt, return GuardFailed.
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
        .declare_function("relon_w5_dict_trace", Linkage::Local, &ctx.func.signature)
        .expect("declare W5 trace fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define W5 trace fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    W5TraceFn {
        entry,
        _module: module,
    }
}

// ----- W6 trace builder (list inside loop) --------------------------

struct W6TraceFn {
    entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32,
    _module: JITModule,
}

unsafe impl Send for W6TraceFn {}
unsafe impl Sync for W6TraceFn {}

/// Hand-built `for i in 0..n { acc += arr[i] }` over a flat
/// `[len][pad][i64 elements...]` record. Same inline `ListGet`
/// lowering as the W5 row's per-iter dict[keys[idx]] inner step,
/// minus the dict_lookup helper.
fn build_w6_trace_fn() -> W6TraceFn {
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
    let mut module = JITModule::new(builder);
    let pointer_ty = module.target_config().pointer_type();

    let mut save_deopt_sig = Signature::new(CallConv::SystemV);
    save_deopt_sig.params.push(AbiParam::new(pointer_ty));
    save_deopt_sig.params.push(AbiParam::new(I32));
    save_deopt_sig.params.push(AbiParam::new(I64));
    let save_deopt_id = module
        .declare_function("__relon_trace_save_deopt", Linkage::Import, &save_deopt_sig)
        .expect("declare save_deopt");

    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I32));

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, save_deopt_id.as_u32() + 1),
        sig.clone(),
    );

    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig);
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
    let entry_block = fb.create_block();
    fb.append_block_params_for_function_params(entry_block);
    let trace_ctx = fb.block_params(entry_block)[0];
    let args_ptr = fb.block_params(entry_block)[1];
    fb.switch_to_block(entry_block);
    fb.seal_block(entry_block);

    let n_val = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 0);
    let list_ptr = fb.ins().load(I64, MemFlags::trusted(), args_ptr, 8);

    let acc_seed = fb.ins().iconst(I64, 0);
    let i_seed = fb.ins().iconst(I64, 0);

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

    fb.switch_to_block(header_block);
    let acc_p = fb.block_params(header_block)[0];
    let i_p = fb.block_params(header_block)[1];
    let cont = fb.ins().icmp(IntCC::SignedLessThan, i_p, n_val);
    let empty: [BlockArg; 0] = [];
    fb.ins()
        .brif(cont, body_block, empty.iter(), exit_block, empty.iter());

    fb.switch_to_block(body_block);
    fb.seal_block(body_block);
    // Bounds: idx (= i) < len.
    let len32 = fb.ins().load(I32, MemFlags::trusted(), list_ptr, 0);
    let len64 = fb.ins().uextend(I64, len32);
    let in_bounds = fb.ins().icmp(IntCC::UnsignedLessThan, i_p, len64);
    let post_bounds = fb.create_block();
    fb.ins().brif(
        in_bounds,
        post_bounds,
        empty.iter(),
        deopt_block,
        empty.iter(),
    );
    fb.seal_block(post_bounds);
    fb.switch_to_block(post_bounds);
    let eight = fb.ins().iconst(I64, 8);
    let off = fb.ins().imul(i_p, eight);
    let base = fb.ins().iadd_imm(list_ptr, 8);
    let addr = fb.ins().iadd(base, off);
    let val = fb.ins().load(I64, MemFlags::trusted(), addr, 0);
    let new_acc = fb.ins().iadd(acc_p, val);
    let one = fb.ins().iconst(I64, 1);
    let new_i = fb.ins().iadd(i_p, one);
    fb.ins().jump(
        header_block,
        &[BlockArg::Value(new_acc), BlockArg::Value(new_i)],
    );
    fb.seal_block(header_block);

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
        .declare_function("relon_w6_list_trace", Linkage::Local, &ctx.func.signature)
        .expect("declare W6 trace fn");
    module
        .define_function(func_id, &mut ctx)
        .expect("define W6 trace fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
        unsafe { std::mem::transmute(raw) };
    W6TraceFn {
        entry,
        _module: module,
    }
}

// ----- tree-walker baselines (for in-bench ratio computation) -------

fn w5_relon_src() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
       #internal\n\
       keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
       result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
     }"
}

fn w6_relon_src() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => i + 1))"
}

fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src).expect("parse");
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

fn args_w_n(n: i64) -> std::collections::HashMap<String, Value> {
    let mut m = std::collections::HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

// ----- bench entry --------------------------------------------------

fn bench_dict_list(c: &mut Criterion) {
    match verify_quiescence() {
        Ok(report) => eprintln!("[bench] {}", report.summary()),
        Err(err) => {
            eprintln!("[bench] {err}");
            eprintln!("[bench] {}", err.report.summary());
            panic!("machine not quiescent; set RELON_BENCH_FORCE_RUN=1 to override");
        }
    }

    let mut group = c.benchmark_group("fd8_dict_list_trace");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(6));
    group.throughput(Throughput::Elements(HOT_LOOP_N));

    // -------- W5: dict[keys[i % 10]] hot loop --------

    let w5_fixture = build_w5_fixture();
    let w5_trace = build_w5_trace_fn();

    // Sanity: invoke once and check the result matches the expected
    // sum for n = HOT_LOOP_N.
    {
        let mut ctx = TraceContext::with_capacity(0);
        let args: [u64; 5] = [
            HOT_LOOP_N,
            w5_fixture.dict_bytes.as_ptr() as u64,
            w5_fixture.keys_list_bytes.as_ptr() as u64,
            w5_fixture.shape_hash,
            w5_fixture.dict_bytes.len() as u64,
        ];
        let status = unsafe { (w5_trace.entry)(&mut ctx as *mut TraceContext, args.as_ptr()) };
        assert_eq!(status, 0, "W5 trace must complete successfully");
        let full_blocks = (HOT_LOOP_N as i64) / 10;
        let rem = (HOT_LOOP_N as i64) % 10;
        let mut tail: i64 = 0;
        for i in 0..rem {
            tail += i + 1;
        }
        let expected = full_blocks * 55 + tail;
        assert_eq!(
            ctx.result_slot as i64, expected,
            "W5 trace JIT result must match analytic sum"
        );
        // Keep key_records alive via this assert reference: the fixture
        // owns them but rustc would otherwise see no consumer.
        assert_eq!(w5_fixture.key_records.len(), 10);
    }

    group.bench_function(BenchmarkId::new("W5_dict_str_key", "trace_jit"), |b| {
        b.iter_custom(|iters| {
            let mut ctx = TraceContext::with_capacity(0);
            let args: [u64; 5] = [
                HOT_LOOP_N,
                w5_fixture.dict_bytes.as_ptr() as u64,
                w5_fixture.keys_list_bytes.as_ptr() as u64,
                w5_fixture.shape_hash,
                w5_fixture.dict_bytes.len() as u64,
            ];
            let args_ptr = args.as_ptr();
            timed_with_warmup(iters, || {
                let s =
                    unsafe { (w5_trace.entry)(&mut ctx as *mut TraceContext, black_box(args_ptr)) };
                black_box(s);
            })
        });
    });

    // Tree-walker baseline for ratio context. Drives the same source
    // the W5 row in cmp_lua uses, so the absolute numbers compare 1:1.
    let (walker_w5, scope_w5) = build_tree_walker(w5_relon_src());
    group.bench_function(
        BenchmarkId::new("W5_dict_str_key", "relon_tree_walk"),
        |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(HOT_LOOP_N as i64);
                timed_with_warmup(iters, || {
                    let v = walker_w5
                        .run_main(&scope_w5, args_w_n(black_box(n_in)))
                        .expect("tree-walk W5");
                    black_box(v);
                })
            });
        },
    );

    // -------- W6: arr[i] hot loop --------

    let w6_list_bytes = build_w6_fixture(HOT_LOOP_N);
    let w6_trace = build_w6_trace_fn();

    {
        let mut ctx = TraceContext::with_capacity(0);
        let args: [u64; 2] = [HOT_LOOP_N, w6_list_bytes.as_ptr() as u64];
        let status = unsafe { (w6_trace.entry)(&mut ctx as *mut TraceContext, args.as_ptr()) };
        assert_eq!(status, 0, "W6 trace must complete successfully");
        let n = HOT_LOOP_N as i64;
        let expected = n * (n + 1) / 2;
        assert_eq!(
            ctx.result_slot as i64, expected,
            "W6 trace JIT result must match analytic sum"
        );
    }

    group.bench_function(BenchmarkId::new("W6_dict_num_key", "trace_jit"), |b| {
        b.iter_custom(|iters| {
            let mut ctx = TraceContext::with_capacity(0);
            let args: [u64; 2] = [HOT_LOOP_N, w6_list_bytes.as_ptr() as u64];
            let args_ptr = args.as_ptr();
            timed_with_warmup(iters, || {
                let s =
                    unsafe { (w6_trace.entry)(&mut ctx as *mut TraceContext, black_box(args_ptr)) };
                black_box(s);
            })
        });
    });

    let (walker_w6, scope_w6) = build_tree_walker(w6_relon_src());
    group.bench_function(
        BenchmarkId::new("W6_dict_num_key", "relon_tree_walk"),
        |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(HOT_LOOP_N as i64);
                timed_with_warmup(iters, || {
                    let v = walker_w6
                        .run_main(&scope_w6, args_w_n(black_box(n_in)))
                        .expect("tree-walk W6");
                    black_box(v);
                })
            });
        },
    );

    group.finish();
}

criterion_group!(benches, bench_dict_list);
criterion_main!(benches);
