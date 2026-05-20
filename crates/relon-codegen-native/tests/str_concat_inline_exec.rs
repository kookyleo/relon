//! F-D7-I end-to-end: JIT-compile a small function that exercises
//! `relon_trace_emitter::emit_str_concat_inline_short_rhs` and confirm
//! its output is byte-identical to the extern `__relon_str_concat`
//! shim for every const rhs length 0 / 1 / 8 / 16.
//!
//! Why this lives in `relon-codegen-native`: same rationale as the
//! sibling `str_contains_inline_exec.rs` — the emitter crate has no
//! `cranelift-jit` dep on purpose (it only produces IR), so we need a
//! downstream test harness that owns the JIT module + finalised
//! machine code.

use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{self, AbiParam, InstBuilder, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_codegen_native::register_trace_runtime_symbols;
use relon_trace_emitter::{emit_str_concat_inline_short_rhs, HostHookId};
use relon_trace_jit::runtime::{__relon_str_concat, StringRef};

/// JIT a small entry of shape `fn(lhs: *const StringRef) -> *const StringRef`
/// that calls `emit_str_concat_inline_short_rhs` with the supplied const
/// rhs bytes baked in.
struct InlineConcatFn {
    entry: unsafe extern "C" fn(*const StringRef) -> *const StringRef,
    _module: JITModule,
}

fn build_inline_concat_fn(rhs: &[u8]) -> InlineConcatFn {
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

    // Pre-declare the alloc helper as an Import so the JIT can
    // resolve it via `register_trace_runtime_symbols`.
    let mut alloc_sig = Signature::new(CallConv::SystemV);
    alloc_sig.params.push(AbiParam::new(pointer_ty));
    alloc_sig.params.push(AbiParam::new(I64));
    alloc_sig.returns.push(AbiParam::new(pointer_ty));
    let alloc_func_id = module
        .declare_function(
            HostHookId::StrConcatAlloc.symbol(),
            Linkage::Import,
            &alloc_sig,
        )
        .expect("declare str_concat_alloc");

    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(pointer_ty));

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 1), sig.clone());

    // Import the alloc helper into the function's symbol table BEFORE
    // creating the FunctionBuilder (mirroring the
    // `relon-codegen-native::trace_inline` pattern).
    let alloc_sig_ref = ctx.func.import_signature(alloc_sig.clone());
    let alloc_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, alloc_func_id.as_u32()));
    let alloc_funcref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(alloc_name),
        signature: alloc_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fb = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = fb.create_block();
    fb.append_block_params_for_function_params(entry);
    let lhs = fb.block_params(entry)[0];
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let result = emit_str_concat_inline_short_rhs(&mut fb, alloc_funcref, lhs, rhs);
    fb.ins().return_(&[result]);
    fb.finalize();

    let func_id = module
        .declare_function(
            &format!("str_concat_inline_{}", rhs.len()),
            Linkage::Local,
            &sig,
        )
        .expect("declare entry");
    module
        .define_function(func_id, &mut ctx)
        .expect("define inline_concat fn");
    module.finalize_definitions().expect("finalize");
    let raw = module.get_finalized_function(func_id);
    let entry: unsafe extern "C" fn(*const StringRef) -> *const StringRef =
        unsafe { std::mem::transmute(raw) };
    InlineConcatFn {
        entry,
        _module: module,
    }
}

/// Reference oracle: the public shim. We rely on its
/// `String::with_capacity + push_str` path as the ground truth.
fn reference_concat(lhs: *const StringRef, rhs: *const StringRef) -> *const StringRef {
    unsafe { __relon_str_concat(lhs, rhs) }
}

fn read_payload(p: *const StringRef) -> Vec<u8> {
    if p.is_null() {
        return Vec::new();
    }
    let r = unsafe { &*p };
    if r.ptr.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(r.ptr, r.len).to_vec() }
}

/// Case-grid runner: cross every lhs in `lhs_cases` with the inline
/// build for `rhs_bytes` and verify byte-for-byte parity against the
/// reference shim.
fn assert_inline_matches_shim(rhs_bytes: &[u8], lhs_cases: &[&'static str]) {
    let f = build_inline_concat_fn(rhs_bytes);
    let rhs_ref = StringRef::from_owned(
        std::str::from_utf8(rhs_bytes)
            .expect("test rhs must be UTF-8")
            .to_string(),
    );
    for lhs_text in lhs_cases {
        let lhs_ref = StringRef::from_static(lhs_text);
        let inline_ptr = unsafe { (f.entry)(lhs_ref) };
        let shim_ptr = reference_concat(lhs_ref, rhs_ref);
        let inline_bytes = read_payload(inline_ptr);
        let shim_bytes = read_payload(shim_ptr);
        assert_eq!(
            inline_bytes,
            shim_bytes,
            "inline `StrConcat({lhs_text}, {rhs:?})` must match extern shim; \
             inline={inline_bytes:?}, shim={shim_bytes:?}",
            rhs = std::str::from_utf8(rhs_bytes).unwrap(),
        );
    }
}

#[test]
fn inline_concat_matches_shim_one_byte_rhs() {
    assert_inline_matches_shim(b"a", &["", "x", "hello"]);
}

#[test]
fn inline_concat_matches_shim_empty_rhs() {
    assert_inline_matches_shim(b"", &["", "x", "hello"]);
}

#[test]
fn inline_concat_matches_shim_eight_byte_rhs() {
    assert_inline_matches_shim(b"01234567", &["", "abc", "longer payload"]);
}

#[test]
fn inline_concat_matches_shim_sixteen_byte_rhs() {
    assert_inline_matches_shim(b"0123456789abcdef", &["", "z", "ABCDEFGHIJKLM"]);
}
