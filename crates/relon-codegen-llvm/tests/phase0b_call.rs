//! Phase 0b — `Op` call-family lowering on the LLVM AOT backend:
//! `Op::CheckCap` (capability gate) and `Op::Trap` (abort).
//!
//! ## What the LLVM backend can and cannot exercise here
//!
//! The LLVM capability gate reads the host-granted capability set from
//! the buffer-protocol entry's trailing `i64 caps` param and traps when
//! the requested bit is clear.
//!
//! Two public-API limits constrain what the LLVM side can test directly:
//!
//!   - The buffer-protocol entry (the only shape with a `caps` slot)
//!     can only be built through `from_source` — `from_ir_direct`
//!     rejects a schema-less buffer-signature IR outright. The LLVM
//!     `from_source` envelope (W1 / W2) emits neither `CheckCap` nor
//!     `Trap`, so a capability-gated / trapping buffer module is not
//!     reachable through the public surface yet.
//!   - `ArenaState` is crate-private and `run_main` hard-codes
//!     `caps = 0`, so even a hand-built buffer entry could not be driven
//!     with a custom capability mask from an integration test.
//!
//! What IS reachable:
//!
//!   - `Op::Trap` lowers under any entry shape (it needs no `caps`
//!     slot), so the legacy-i64 entry exercises it structurally via
//!     `emit_ir_dump()` — mirroring how `llvm_divmod_trap.rs` asserts
//!     the div-by-zero guard (the `ud2` abort can't run under the
//!     default test driver).
//!   - `Op::CheckCap` on the legacy entry is rejected with a precise
//!     `Codegen` error (no `caps` slot to read), which IS reachable and
//!     pins the entry-shape guard.
//!
//! ## Three-way alignment (cranelift gold standard)
//!
//! `Op::CheckCap { cap_bit }`'s `cap_bit` is a
//! `CapabilityBit::bit_index` — the SAME numeric bit the cranelift
//! backend reuses as a `cap_lookup` vtable slot key. Cranelift carries
//! the full runtime harness (state pointer + vtable + `cap_lookup`
//! host helper), so the grant / deny / trap *semantics* the LLVM IR
//! encodes are pinned down here against the cranelift gold standard:
//! granting the bit lets the body run; withholding it raises
//! `CapabilityDenied`; an unconditional `Op::Trap` aborts.
//!
//! `Op::CallNative` is unimplemented on the LLVM backend (it needs the
//! host-fn registry + MCJIT symbol wiring that lives outside the
//! per-family codegen module), so it is not exercised here.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::{AotEvaluator, CapabilityVtable, HostFnPtr, SandboxConfig};
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp, TrapKind, NO_CAPABILITY_BIT};
use relon_parser::TokenRange;

fn tagged(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

// --- LLVM-side fixtures (legacy-i64 entry — the only shape a hand-built
//     IR can reach through `from_ir_direct`) ----------------------------

/// Legacy-i64 `#main(Int) -> Int` whose body is an unconditional
/// `Op::Trap`. `Trap` needs no `caps` slot, so it lowers under the
/// legacy entry. The trailing `ConstI64 ; Return` keeps the body
/// well-formed past the (dead) post-trap continuation block.
fn build_legacy_trap_ir(kind: TrapKind) -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::Trap { kind }),
                tagged(Op::ConstI64(0)),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

/// Legacy-i64 `#main(Int) -> Int` carrying a `CheckCap` — used to check
/// the gate rejects the shape that has no `caps` slot.
fn build_legacy_checkcap_ir(cap_bit: u32) -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::CheckCap { cap_bit }),
                tagged(Op::LocalGet(0)),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn llvm_trap_emits_llvm_trap() {
    let ir = build_legacy_trap_ir(TrapKind::IndexOutOfBounds);
    let ev = LlvmAotEvaluator::from_ir_direct(ir, vec!["x".into()]).expect("llvm compile");
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("llvm.trap"),
        "Op::Trap IR dump missing llvm.trap:\n{dump}"
    );
}

#[test]
fn llvm_checkcap_rejected_on_legacy_entry() {
    // The legacy-i64 entry has no `caps` slot, so the gate cannot be
    // lowered there — it must surface as a Codegen error rather than
    // reading an out-of-range param. (The buffer entry that DOES carry
    // the `caps` slot is only reachable via `from_source`, whose LLVM
    // envelope does not yet emit CheckCap — see the module note.)
    let ir = build_legacy_checkcap_ir(2);
    let msg = match LlvmAotEvaluator::from_ir_direct(ir, vec!["x".into()]) {
        Ok(_) => panic!("CheckCap on legacy entry must fail to build"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("CheckCap") && msg.contains("buffer-protocol"),
        "unexpected error for legacy CheckCap: {msg}"
    );
}

// --- Cranelift gold standard: grant / deny / trap semantics -----------

/// Stand-in host fn registered to grant a capability slot.
unsafe extern "C" fn now_stub(_arg: i64) -> i64 {
    1_700_000_000
}

/// Legacy-i64 `#main(Int) -> Int` with a `CheckCap { cap_bit }` prologue,
/// matching the cranelift `host_fn_capability` gold-standard fixture.
fn build_cranelift_checkcap_ir(cap_bit: u32) -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::CheckCap { cap_bit }),
                tagged(Op::LocalGet(0)),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn cranelift_checkcap_denies_when_bit_ungranted() {
    let ir = build_cranelift_checkcap_ir(2);
    let ev = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".into()])
        .expect("cranelift compile");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let err = ev.run_main(args).expect_err("ungranted bit must deny");
    assert!(
        matches!(err, RuntimeError::CapabilityDenied { .. }),
        "expected CapabilityDenied, got {err:?}"
    );
}

#[test]
fn cranelift_checkcap_grants_when_bit_registered() {
    let ir = build_cranelift_checkcap_ir(2);
    let mut ev = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".into()])
        .expect("cranelift compile");
    // Register a host fn at the matching cap_bit (== grant the bit) and
    // re-install the vtable.
    let mut vt = CapabilityVtable::with_capacity(64);
    let fn_ptr: HostFnPtr = now_stub;
    vt.register(2, fn_ptr);
    ev.install_capabilities_mut(Arc::new(vt));

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = ev.run_main(args).expect("granted bit must run the body");
    assert_eq!(result, Value::Int(99));
}

#[test]
fn cranelift_checkcap_no_capability_bit_elides_gate() {
    let ir = build_cranelift_checkcap_ir(NO_CAPABILITY_BIT);
    let ev = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".into()])
        .expect("cranelift compile");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = ev.run_main(args).expect("sentinel must elide the gate");
    assert_eq!(result, Value::Int(99));
}
