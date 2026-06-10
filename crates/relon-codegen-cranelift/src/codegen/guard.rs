//! Trap + host-helper plumbing for the cranelift backend.
//!
//! All "abnormal exit" mechanics live here:
//!
//! * [`trap_code`] — translate the IR-side [`TrapKind`] discriminant
//!   into a cranelift [`TrapCode`] so emitted `trap` instructions
//!   carry a host-translatable byte payload.
//! * [`make_raise_trap_signature`] / [`make_now_signature`] /
//!   [`make_cap_lookup_signature`] — build the cranelift signatures
//!   for the three host helpers the lowering indirects through.
//! * [`declare_vtable_data`] — reserve the
//!   `__relon_capability_vtable` data symbol on the active module.
//! * [`emit_indirect_host_call`] — emit a single load-from-vtable +
//!   `call_indirect` pair shared by every host-helper call site.
//!
//! The `Codegen` impl-side trap helpers (`cond_trap`, `emit_trap`,
//! `emit_resource_check`) continue to live in [`super`] because they
//! touch private `Codegen` state (`builder`, `trap_block`, `sandbox`).
//! The split here just hoists the data-shape + signature builders out
//! of the monolithic file so the next phase can iterate on each
//! independently.

use cranelift_codegen::ir::types::{F64, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, GlobalValue, Inst, InstBuilder, MemFlags, SigRef, Signature, TrapCode,
    Value as CValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::FunctionBuilder;
use cranelift_module::{DataDescription, DataId, Linkage, Module as CrModule};

use crate::error::CraneliftError;
use crate::sandbox::TrapKind;
use crate::vtable::{VtableSlot, VTABLE_BYTES, VTABLE_SYMBOL};

/// Trap codes the cranelift lowering emits via `trap` /
/// `trapnz` / `trapz`. Aligned with [`TrapKind`] so the host
/// translates without a translation table.
///
/// v5-beta-1 uses cranelift's intrinsic `trap` instruction only for
/// guaranteed-fatal paths (Unreachable). Every guard reachable by
/// the lowered code (divide-by-zero, bounds, capability, resource)
/// routes through the `raise_trap` host helper + early-return
/// sequence instead, because:
///
/// 1. `cranelift_codegen::ir::trap` emits a `ud2` (SIGILL) on x86
///    Linux which Rust's panic runtime cannot intercept through
///    `catch_unwind`. Routing the trap through a host helper lets
///    us record the trap code in `SandboxState::trap_code` and
///    return a sentinel zero, which the trampoline interprets as
///    "trap fired — translate via the recorded code".
/// 2. Real `sigsetjmp` support is on the v5-beta-2 roadmap; until
///    then this is the cleanest path that preserves the typed
///    `RuntimeError` surface on every supported target.
#[allow(dead_code)]
pub(super) fn trap_code(kind: TrapKind) -> TrapCode {
    TrapCode::user(kind as u8).expect("TrapKind discriminant is non-zero")
}

/// Build the cranelift signature for the `RelonRaiseTrap` vtable
/// slot: `extern "C" fn(state: *const SandboxState, code: i64)`.
pub(super) fn make_raise_trap_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(I64));
    sig
}

/// Build the cranelift signature for the `RelonNow` vtable slot:
/// `extern "C" fn(state: *const SandboxState) -> i64`.
pub(super) fn make_now_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I64));
    sig
}

/// Build the cranelift signature for the `RelonCapLookup` vtable
/// slot: `extern "C" fn(state: *const SandboxState, cap_bit: i32) ->
/// *const u8`.
pub(super) fn make_cap_lookup_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(I32));
    sig.returns.push(AbiParam::new(pointer_ty));
    sig
}

/// Build the cranelift signature for the `RelonGlobMatch` vtable
/// slot: `extern "C" fn(state: *const SandboxState, s_off: i32,
/// p_off: i32) -> i32`. The two i32 args are arena-relative offsets
/// into the wasm-style String records the codegen layout pass
/// produced; the i32 return is the matched (1) / no-match (0) bool.
pub(super) fn make_glob_match_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(I32));
    sig.params.push(AbiParam::new(I32));
    sig.returns.push(AbiParam::new(I32));
    sig
}

/// Build the cranelift signature for the `RelonCallNative` vtable
/// slot: `extern "C" fn(state: *const SandboxState, import_idx: i32,
/// args_ptr: *const i64, arg_count: i32) -> i64`. Dispatches a
/// source-lowered `Op::CallNative` to the `Arc<dyn RelonFunction>`
/// registered at `import_idx`; the scalar args are spilled to a stack
/// slot the codegen passes by address.
pub(super) fn make_call_native_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty)); // state
    sig.params.push(AbiParam::new(I32)); // import_idx
    sig.params.push(AbiParam::new(pointer_ty)); // args_ptr
    sig.params.push(AbiParam::new(I32)); // arg_count
    sig.returns.push(AbiParam::new(I64)); // i64-encoded result
    sig
}

/// Build the cranelift signature for the `RelonF64ToStr` vtable slot:
/// `extern "C" fn(state: *const SandboxState, bits: i64, dest_off:
/// i32) -> i32`. `bits` carries the IEEE-754 bit pattern of the `f64`
/// to render (bitcast at the call edge); `dest_off` is the
/// arena-relative offset of the pre-allocated scratch record the
/// helper fills with `[len: u32 LE][utf8 payload]`; the i32 return is
/// the payload length, negative on failure (the codegen traps on it).
pub(super) fn make_f64_to_str_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty)); // state
    sig.params.push(AbiParam::new(I64)); // f64 bit pattern
    sig.params.push(AbiParam::new(I32)); // dest record offset
    sig.returns.push(AbiParam::new(I32)); // payload len / -1
    sig
}

/// Build the cranelift signature for the libc `fmod` libcall:
/// `extern "C" fn(a: f64, b: f64) -> f64` (the SysV C ABI). The
/// cranelift backend has no native float-remainder instruction (x86
/// has no `frem`; LLVM itself lowers `frem` to this same `fmod`
/// libcall), so `Op::Mod(IrType::F64)` lowers to a call against this
/// signature. The JIT path resolves the `fmod` symbol to a Rust
/// `a % b` shim (see `compile_module_with`) so the result is
/// bit-identical to the tree-walker's `a.as_f64() % b.as_f64()`; the
/// cranelift-object path leaves `fmod` as an undefined ELF import the
/// dynamic linker binds to the process libc at `dlopen` (same IEEE-754
/// remainder, identical bits).
pub(super) fn make_fmod_signature() -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(F64));
    sig.params.push(AbiParam::new(F64));
    sig.returns.push(AbiParam::new(F64));
    sig
}

/// Build the cranelift signature for the libm `pow` libcall:
/// `extern "C" fn(base: f64, exp: f64) -> f64` (the SysV C ABI). The
/// cranelift backend has no native float-power instruction (LLVM
/// itself lowers `llvm.pow.f64` to this same `pow` libcall), so
/// `Op::F64Pow` lowers to a call against this signature. The JIT path
/// resolves the `pow` symbol to a Rust `a.powf(b)` shim (see
/// `compile_module_with`) so the result is bit-identical to the
/// tree-walker's `to_f64_val(a).powf(to_f64_val(b))`; the
/// cranelift-object path leaves `pow` as an undefined ELF import the
/// dynamic linker binds to the process libm at `dlopen` (the same
/// `pow` that `f64::powf` calls, identical bits).
pub(super) fn make_pow_signature() -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(F64));
    sig.params.push(AbiParam::new(F64));
    sig.returns.push(AbiParam::new(F64));
    sig
}

/// Declare the `__relon_capability_vtable` data symbol on the given
/// module. Reserves [`VTABLE_BYTES`] of zero-initialised space so the
/// host can populate the slots post-finalize (JIT) or post-dlopen
/// (cranelift-object).
///
/// Linkage rules:
/// - `JITModule`: `Linkage::Local` — the JIT resolves the symbol by
///   `DataId` rather than by name, so the linkage is advisory.
/// - `ObjectModule`: `Linkage::Export` — the ELF needs the symbol in
///   `.dynsym` so `dlsym` can find it from the host side.
///
/// We pick `Export` here because both backends accept it; the JIT's
/// `get_finalized_data` works either way.
pub(super) fn declare_vtable_data<M: CrModule>(module: &mut M) -> Result<DataId, CraneliftError> {
    // `writable = true` because the host populates the slots
    // post-link. `tls = false` — single-process shared vtable.
    let data_id = module
        .declare_data(
            VTABLE_SYMBOL,
            Linkage::Export,
            /*writable=*/ true,
            /*tls=*/ false,
        )
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare vtable data: {e}")))?;
    let mut desc = DataDescription::new();
    desc.define_zeroinit(VTABLE_BYTES);
    module
        .define_data(data_id, &desc)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define vtable data: {e}")))?;
    Ok(data_id)
}

/// Emit an indirect host-helper call: load the function pointer from
/// the vtable slot, then `call_indirect` with the supplied signature.
///
/// Used both inside `Codegen` (for body-level helper calls) and in
/// the `compile_module_with` driver (to lower the trap_block tail).
/// Centralising the load sequence keeps the codegen output uniform
/// across entry / lambda / trap-block call sites.
pub(super) fn emit_indirect_host_call(
    builder: &mut FunctionBuilder<'_>,
    vtable_gv: GlobalValue,
    pointer_ty: cranelift_codegen::ir::Type,
    slot: VtableSlot,
    sig_ref: SigRef,
    args: &[CValue],
) -> Inst {
    // Materialise the vtable base address in the function.
    let vtable_base = builder.ins().global_value(pointer_ty, vtable_gv);
    // Load the slot's host fn pointer.
    let fn_ptr = builder.ins().load(
        pointer_ty,
        MemFlags::trusted(),
        vtable_base,
        slot.offset_bytes(),
    );
    builder.ins().call_indirect(sig_ref, fn_ptr, args)
}
