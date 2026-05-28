//! 2026-05-21 Tier-2 end-to-end: JIT-compile a tiny trace that emits
//! `TraceOp::StrGlobMatch` against two `StringRef` inputs, link it
//! against the `__relon_str_glob_match` helper registered by
//! `register_trace_runtime_symbols`, and confirm the runtime output
//! matches `relon_ir::glob::glob_match` across match / non-match /
//! Unicode / null inputs.
//!
//! Why this lives in `relon-codegen-cranelift` rather than the emitter
//! crate: the emitter only produces cranelift IR; the helper symbol
//! resolution and JIT finalisation happen here.

use cranelift_codegen::ir::types::I32;
use cranelift_codegen::ir::{self, AbiParam, InstBuilder, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_codegen_cranelift::register_trace_runtime_symbols;
use relon_trace_emitter::HostHookId;
use relon_trace_jit::runtime::StringRef;

/// Build a `fn(s_ptr, pattern_ptr) -> i32` that calls the
/// `__relon_str_glob_match` helper directly. We deliberately do NOT
/// go through `TraceEmitter::emit` here so the test stays self-
/// contained — the helper-call shape is identical to what the
/// emitter's `emit_str_glob_match` produces (one declared FuncRef,
/// one `call` with two i64 operands, returns i32).
struct GlobMatchFn {
    entry: unsafe extern "C" fn(*const StringRef, *const StringRef) -> i32,
    _module: JITModule,
}

fn build_glob_match_fn() -> GlobMatchFn {
    let mut flag_builder = settings::builder();
    flag_builder.set("is_pic", "false").unwrap();
    flag_builder.set("opt_level", "speed").unwrap();
    flag_builder.set("enable_verifier", "true").unwrap();
    let flags = settings::Flags::new(flag_builder);
    let isa_builder = cranelift_native::builder().expect("cranelift-native builder");
    let isa = isa_builder.finish(flags).expect("isa finish");

    let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    register_trace_runtime_symbols(&mut jit_builder);
    let mut module = JITModule::new(jit_builder);

    let pointer_ty = module.target_config().pointer_type();
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I32));

    // Declare the helper as a module import resolved by the
    // JITBuilder's symbol table; the linker stitches in the body that
    // `register_trace_runtime_symbols` registered.
    let mut helper_sig = Signature::new(CallConv::SystemV);
    helper_sig.params.push(AbiParam::new(pointer_ty));
    helper_sig.params.push(AbiParam::new(pointer_ty));
    helper_sig.returns.push(AbiParam::new(I32));
    let helper_id = module
        .declare_function(
            HostHookId::StrGlobMatch.symbol(),
            Linkage::Import,
            &helper_sig,
        )
        .expect("declare __relon_str_glob_match");

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 1), sig.clone());
    let helper_ref = module.declare_func_in_func(helper_id, &mut ctx.func);

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let s_ptr = fb.block_params(entry)[0];
    let p_ptr = fb.block_params(entry)[1];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let inst = fb.ins().call(helper_ref, &[s_ptr, p_ptr]);
    let r = fb.inst_results(inst)[0];
    fb.ins().return_(&[r]);
    fb.finalize();

    let func_id = module
        .declare_function("glob_match_trace_exec", Linkage::Local, &sig)
        .expect("declare");
    module
        .define_function(func_id, &mut ctx)
        .expect("define glob_match fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*const StringRef, *const StringRef) -> i32 =
        unsafe { std::mem::transmute(raw) };
    GlobMatchFn {
        entry,
        _module: module,
    }
}

fn run(s: &'static str, p: &'static str) -> i32 {
    let f = build_glob_match_fn();
    let s_ref = StringRef::from_static(s);
    let p_ref = StringRef::from_static(p);
    unsafe { (f.entry)(s_ref, p_ref) }
}

#[test]
fn anchored_star_glob_matches() {
    assert_eq!(run("hello world", "hello *"), 1);
}

#[test]
fn literal_prefix_mismatch_returns_zero() {
    assert_eq!(run("hello world", "goodbye *"), 0);
}

#[test]
fn question_mark_matches_single_codepoint() {
    assert_eq!(run("ab", "a?"), 1);
    assert_eq!(run("abc", "a?"), 0);
}

#[test]
fn unicode_payload_round_trips_through_helper() {
    assert_eq!(run("αβγ🦀", "α*🦀"), 1);
}

#[test]
fn null_pointer_returns_no_match() {
    let f = build_glob_match_fn();
    let p_ref = StringRef::from_static("*");
    let r = unsafe { (f.entry)(std::ptr::null(), p_ref) };
    assert_eq!(r, 0);
}

/// Sanity: the JIT call really reaches the helper body — pin against
/// the reference implementation for a non-trivial pattern set so a
/// future relocation regression surfaces here rather than in a vague
/// downstream behaviour-equivalence bench.
#[test]
fn jit_output_agrees_with_relon_ir_glob_match() {
    let f = build_glob_match_fn();
    for (s, p, expected) in [
        ("abc", "a*c", true),
        ("abc", "a?c", true),
        ("abc", "*", true),
        ("abc", "abc", true),
        ("abc", "abcd", false),
        ("", "", true),
        ("", "*", true),
        ("abc", "?", false),
        ("a", "?", true),
    ] {
        let s_ref = StringRef::from_static(s);
        let p_ref = StringRef::from_static(p);
        let jit = unsafe { (f.entry)(s_ref, p_ref) };
        let oracle = relon_ir::glob::glob_match(s, p);
        assert_eq!(
            jit == 1,
            oracle,
            "s={s:?} p={p:?}: jit={jit} oracle={oracle} expected={expected}"
        );
        // Double-check the IR matcher itself agrees with the table.
        assert_eq!(oracle, expected, "oracle drift for s={s:?} p={p:?}");
    }
}
