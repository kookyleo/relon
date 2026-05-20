//! F-D7-C end-to-end: JIT-compile a small function that calls
//! `relon_trace_emitter::emit_str_contains_inline` and confirm its
//! output is byte-identical to the extern `__relon_str_contains`
//! shim for every needle length 0 / 1 / 8 / 16, on both hit and miss
//! haystacks. The shim is the reference (it just delegates to Rust's
//! `str::contains`); the inline scan is the new fast path.
//!
//! Why this lives in `relon-codegen-native` rather than the emitter
//! crate: the emitter crate has no `cranelift-jit` dep on purpose (it
//! only produces IR), so we need a downstream test harness that owns
//! the JIT module + finalised machine code.

use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, AbiParam, InstBuilder, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_codegen_native::register_trace_runtime_symbols;
use relon_trace_emitter::emit_str_contains_inline;
use relon_trace_jit::runtime::{__relon_str_contains, StringRef};

/// Build a `fn(haystack_ptr: i64) -> i32` that calls the inline
/// byte-scan for the supplied compile-time needle and returns the i32
/// hit/miss bit. Returns a pointer to the JIT'd entry and the owning
/// module (so the caller keeps the module alive across calls).
struct InlineContainsFn {
    entry: unsafe extern "C" fn(*const StringRef) -> i32,
    _module: JITModule,
}

fn build_inline_contains_fn(needle: &[u8]) -> InlineContainsFn {
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
    sig.returns.push(AbiParam::new(I32));

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 1), sig.clone());

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let haystack = fb.block_params(entry)[0];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    // Widen pointer to i64 if needed (we requested pointer_ty which is
    // i64 on x86_64; explicit widen keeps the lowering safe on hosts
    // where pointer width might differ).
    let haystack_i64 = if pointer_ty != I64 {
        fb.ins().uextend(I64, haystack)
    } else {
        haystack
    };

    let r = emit_str_contains_inline(&mut fb, haystack_i64, needle);
    fb.ins().return_(&[r]);
    fb.finalize();

    let func_id = module
        .declare_function(
            &format!("str_contains_inline_{}", needle.len()),
            Linkage::Local,
            &sig,
        )
        .expect("declare");
    module
        .define_function(func_id, &mut ctx)
        .expect("define inline_contains fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*const StringRef) -> i32 = unsafe { std::mem::transmute(raw) };
    InlineContainsFn {
        entry,
        _module: module,
    }
}

/// Reference oracle: the public shim. We rely on its
/// `Rust str::contains` semantics as the ground truth.
fn reference_contains(haystack: *const StringRef, needle: *const StringRef) -> i32 {
    unsafe { __relon_str_contains(haystack, needle) }
}

/// One-shot helper: build the inline fn for `needle`, then check both
/// hit and miss haystacks and a null haystack edge case.
fn check_inline_matches_extern(needle: &str, hit_haystack: &str, miss_haystack: &str) {
    let needle_ref = StringRef::from_static(string_to_static(needle));
    let hit_ref = StringRef::from_static(string_to_static(hit_haystack));
    let miss_ref = StringRef::from_static(string_to_static(miss_haystack));

    let f = build_inline_contains_fn(needle.as_bytes());

    let inline_hit = unsafe { (f.entry)(hit_ref) };
    let extern_hit = reference_contains(hit_ref, needle_ref);
    assert_eq!(
        inline_hit, extern_hit,
        "needle={needle:?} hit haystack={hit_haystack:?}: inline={inline_hit} extern={extern_hit}",
    );

    let inline_miss = unsafe { (f.entry)(miss_ref) };
    let extern_miss = reference_contains(miss_ref, needle_ref);
    assert_eq!(
        inline_miss, extern_miss,
        "needle={needle:?} miss haystack={miss_haystack:?}: inline={inline_miss} extern={extern_miss}",
    );

    let inline_null = unsafe { (f.entry)(std::ptr::null()) };
    assert_eq!(
        inline_null, 0,
        "needle={needle:?}: null haystack must be miss (got {inline_null})"
    );
}

/// `StringRef::from_static` wants `&'static str` — leak the input so
/// each test gets a stable pointer.
fn string_to_static(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

#[test]
fn inline_one_byte_needle_matches_extern() {
    check_inline_matches_extern("x", "axb", "abc");
}

#[test]
fn inline_eight_byte_needle_matches_extern() {
    // Needle fits in a single u64 — common JSON-keyword case.
    check_inline_matches_extern("password", "user-password-field", "secret-field");
}

#[test]
fn inline_sixteen_byte_needle_matches_extern() {
    // Boundary: longest needle the inline path accepts. The miss
    // haystack also covers the "haystack shorter than needle" branch.
    check_inline_matches_extern("0123456789abcdef", "xxx0123456789abcdef-yy", "short");
}

#[test]
fn inline_empty_needle_is_always_hit() {
    let needle = "";
    let f = build_inline_contains_fn(needle.as_bytes());
    let any = StringRef::from_static("anything");
    assert_eq!(unsafe { (f.entry)(any) }, 1);
    // Empty haystack too: `"".contains("")` is true.
    let empty = StringRef::from_static("");
    assert_eq!(unsafe { (f.entry)(empty) }, 1);
}

#[test]
fn inline_needle_at_end_of_haystack_is_a_hit() {
    // Regression: the candidate-position loop must include
    // `last_start = h_len - m_len` so the final-position match is
    // found, not skipped.
    check_inline_matches_extern("end", "the end", "ending");
}

#[test]
fn inline_repeated_match_short_circuits_correctly() {
    // Multiple matches in the haystack — early-exit must still return
    // 1, not iterate past the first hit (we don't observe count, only
    // bit equality with extern).
    check_inline_matches_extern("ab", "ab__ab__ab", "ba__ba");
}
