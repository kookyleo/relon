//! Phase C — capability gate + sandbox state on the LLVM-native AOT
//! (`emit_object`) path.
//!
//! Verifies the three-state capability behaviour — **grant** / **deny**
//! / **dispatch** — for the LLVM backend's sandbox surface, anchored
//! against the cranelift gold standard
//! (`relon-codegen-cranelift::sandbox` + the `host_fn_capability` /
//! `vtable_indirection` / `trap_div_zero` integration tests).
//!
//! ## What "native path" means here, and what is / isn't e2e
//!
//! The LLVM capability gate is a `(caps & (1 << cap_bit)) != 0` test the
//! `Op::CheckCap` lowering bakes into the buffer-protocol entry the
//! emitter produces. A denied bit records
//! `NativeTrap::CapabilityDenied` in `ArenaState::trap_code` and returns
//! the negative sentinel so the host lifts
//! `RuntimeError::CapabilityDenied`; a granted bit lets the body run; a
//! source-lowered native call dispatches through
//! `relon_llvm_call_native` keyed by `import_idx`.
//!
//! Coverage layered by how close to the linked binary each assertion
//! gets:
//!
//! 1. **Native-object emit round-trips** — `emit_object` lowers a source
//!    to a real relocatable ELF `.o` (the linked-after artefact). Proven
//!    here for a non-gated source.
//! 2. **The gate is baked into the buffer-protocol native IR** — a
//!    host-gated `#native` source lowered via `from_source_with_options`
//!    emits the `(caps & mask)` test + the `cap_denied_trap` arm +
//!    `relon_llvm_call_native` dispatch. This is the SAME IR `emit_object`
//!    lowers into the `.o`; the IR dump pins it.
//! 3. **Sandbox/vtable module logic** — the new
//!    `sandbox::CapabilityVtable` three-state (grant / deny via the
//!    shared `CapabilityGate` policy / host-fn dispatch),
//!    `SandboxTrapKind` numbering + `RuntimeError` lifting, and the
//!    `vtable::VtableSlot` symbol registry.
//! 4. **Cranelift gold-standard anchor** — the grant / deny / elide
//!    *runtime outcomes* (cranelift carries the full state-pointer +
//!    vtable harness, so it can run the gate to completion).
//!
//! ### Compile-time capability rejection on the `.o` seam
//!
//! The options-carrying `emit_object_with_options` seam threads a
//! caller `AnalyzeOptions` into the same `lower_source_with_options`
//! the in-process `from_source` path uses, which forces
//! `standalone_capability_check: true`. A host-gated `#native` call
//! whose capability the options do **not** grant is therefore rejected
//! at *compile time* (`LlvmError::Analyze`) — it never reaches the `.o`,
//! matching cranelift's static accept/reject decision, rather than
//! compiling and deferring to a runtime `CapabilityDenied` trap. See
//! `emit_object_rejects_gated_call_without_granted_cap` below.
//!
//! The 3-arg `emit_object(src, symbol, path)` convenience form still
//! lowers with *default* options (no host `#native` declarations), so it
//! resolves no gated call and bakes no gate from a plain source — the
//! gated flow rides `emit_object_with_options`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use relon_codegen_llvm::{
    populate_global_mappings, CapabilityVtable, LlvmAotEvaluator, SandboxConfig, SandboxTrapKind,
    VtableSlot, WorldMode,
};
use relon_eval_api::{
    Capabilities, CapabilityBit, Evaluator, NativeArgs, NativeFnCaps, RelonFunction, RuntimeError,
    Value,
};
use relon_parser::TokenRange;

// ---------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------

/// No-callback `NativeFnCaps` for direct host-fn dispatch in unit tests.
struct NoCb;
impl NativeFnCaps for NoCb {
    fn call_relon(
        &self,
        _f: &Value,
        _a: Vec<Value>,
        _r: TokenRange,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "no cb".into(),
        })
    }
}

/// Host fn that adds 7 to its single Int arg — mirrors cranelift's
/// `AddSeven` gold-standard callable.
struct AddSeven;
impl RelonFunction for AddSeven {
    fn call(&self, args: NativeArgs, _r: TokenRange) -> Result<Value, RuntimeError> {
        match args.positional.first() {
            Some(Value::Int(x)) => Ok(Value::Int(x + 7)),
            _ => Err(RuntimeError::Unsupported {
                reason: "AddSeven expects Int".into(),
            }),
        }
    }
}

/// `AnalyzeOptions` describing one host-registered native fn gated on
/// `reads_clock`, granting that capability. Mirrors the cranelift
/// `native_call_from_source::host_options` shape.
fn clock_add_options(grant_clock: bool) -> relon_analyzer::AnalyzeOptions {
    let sig = relon_analyzer::FnSignature {
        name: "clock_add".to_string(),
        generics: Vec::new(),
        params: vec![relon_analyzer::FnParam {
            name: "_".into(),
            ty: relon_analyzer::type_node_simple("Int"),
            optional: false,
        }],
        return_type: relon_analyzer::type_node_simple("Int"),
        variadic_tail: None,
    };
    let mut signatures = HashMap::new();
    signatures.insert("clock_add".to_string(), sig);
    let mut gate = relon_analyzer::NativeFnGate::default();
    gate.reads_clock = true;
    let mut gates = HashMap::new();
    gates.insert("clock_add".to_string(), gate);
    let mut names = HashSet::new();
    names.insert("clock_add".to_string());
    let mut caps = relon_analyzer::Capabilities::default();
    caps.reads_clock = grant_clock;
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps,
        strict_mode: false,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// 1. Native-object emit round-trips (the linked-after artefact)
// ---------------------------------------------------------------------

#[test]
fn emit_object_produces_a_linkable_native_artefact() {
    // The native (emit_object) path lowers a source to a relocatable
    // ELF `.o`. Pins that the native artefact the linker consumes is
    // actually produced — the surface the Phase C gate rides on.
    let dir = std::env::temp_dir().join(format!("relon_aot_cap_gate_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mk tmp dir");
    let out = dir.join("cap_gate_smoke.o");
    let info = LlvmAotEvaluator::emit_object("#main(Int x) -> Int\nx + 1", "relon_main", &out)
        .expect("emit_object");
    assert_eq!(info.entry_symbol, "relon_main");
    let meta = std::fs::metadata(&out).expect("object file exists");
    assert!(meta.len() > 0, "emitted object must be non-empty");
    // ELF magic — confirm it is a real relocatable object, not a stub.
    let bytes = std::fs::read(&out).expect("read object");
    assert_eq!(
        &bytes[..4],
        b"\x7fELF",
        "emitted file must be an ELF object"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn emit_object_rejects_gated_call_without_granted_cap() {
    // Alignment guard: the `.o` seam runs the SAME single-file
    // capability-reachability check as the in-process `from_source`
    // path (`standalone_capability_check` is forced on in
    // `lower_source_with_options`). A `#native` call gated on
    // `reads_clock` whose options do NOT grant that bit must be
    // rejected at *compile time* — the emit fails with `Analyze` and no
    // `.o` is produced — rather than compiling and deferring to a
    // runtime `CapabilityDenied` trap. Granting the bit compiles the
    // exact same source, isolating the rejection to the capability gate.
    let dir = std::env::temp_dir().join(format!("relon_aot_cap_reject_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mk tmp dir");
    let src = "#main(Int x) -> Int\nclock_add(x)";

    let ungranted = clock_add_options(/*grant_clock=*/ false);
    let out_deny = dir.join("cap_reject.o");
    let err = LlvmAotEvaluator::emit_object_with_options(
        src,
        "relon_main",
        &out_deny,
        &ungranted,
        WorldMode::OpenWorld,
        None,
    )
    .expect_err("ungranted gated call must be rejected at compile time");
    assert!(
        matches!(err, relon_codegen_llvm::LlvmError::Analyze(_)),
        "expected a static Analyze rejection (CapabilityRequired), got {err:?}"
    );
    assert!(
        !out_deny.exists(),
        "a compile-time-rejected source must NOT leave a `.o` behind"
    );

    // Same source, capability granted -> compiles to a real `.o`.
    let granted = clock_add_options(/*grant_clock=*/ true);
    let out_ok = dir.join("cap_grant.o");
    LlvmAotEvaluator::emit_object_with_options(
        src,
        "relon_main",
        &out_ok,
        &granted,
        WorldMode::OpenWorld,
        None,
    )
    .expect("granted gated call must compile");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------
// 2. The gate is baked into the buffer-protocol native IR
// ---------------------------------------------------------------------

#[test]
fn capability_gate_is_emitted_into_the_buffer_protocol_native_ir() {
    // A host-gated `#native` call lowers to the buffer-protocol entry
    // whose trailing `i64 caps` param the `Op::CheckCap` gate tests.
    // This is the SAME IR `emit_object` lowers into the `.o`; asserting
    // it here pins the native gate emission without needing the
    // options-carrying `emit_object` seam (the wiring gap above).
    let opts = clock_add_options(/*grant_clock=*/ true);
    let ev = LlvmAotEvaluator::from_source_with_options("#main(Int x) -> Int\nclock_add(x)", &opts)
        .expect("gated source must compile");
    let dump = ev.emit_ir_dump();

    // The `(caps & mask)` test + the deny-trap branch + the granted
    // continuation are all present — the full gate shape.
    assert!(
        dump.contains("cap_mask"),
        "gate IR missing the caps-bitmask AND:\n{dump}"
    );
    assert!(
        dump.contains("cap_denied"),
        "gate IR missing the denied compare:\n{dump}"
    );
    assert!(
        dump.contains("cap_denied_trap"),
        "gate IR missing the deny trap arm:\n{dump}"
    );
    assert!(
        dump.contains("cap_granted"),
        "gate IR missing the granted continuation:\n{dump}"
    );
    // The native dynamic-dispatch helper is declared (import_idx-keyed
    // call) — mirrors cranelift's RelonCallNative slot.
    assert!(
        dump.contains("relon_llvm_call_native"),
        "gate IR missing the native dispatch helper:\n{dump}"
    );
}

// ---------------------------------------------------------------------
// 3. Sandbox/vtable module logic — grant / deny / dispatch three-state
// ---------------------------------------------------------------------

#[test]
fn sandbox_vtable_grant_state() {
    // GRANT: granting a bit sets it in the `caps` mask the linked entry
    // receives, and the gate reads it back as granted.
    let mut vt = CapabilityVtable::with_capacity(64);
    assert!(!vt.is_granted(CapabilityBit::ReadsClock.bit_index()));
    vt.grant(CapabilityBit::ReadsClock.bit_index());
    assert!(vt.is_granted(CapabilityBit::ReadsClock.bit_index()));
    assert_eq!(
        vt.caps_mask(),
        1i64 << CapabilityBit::ReadsClock.bit_index(),
        "the runtime carrier is the caps bitmask"
    );
}

#[test]
fn sandbox_vtable_deny_state_via_shared_gate() {
    // DENY: the same `CapabilityGate` policy the cranelift backend and
    // the tree-walker consult is consulted here. A default (sandboxed)
    // Capabilities denies `reads_clock`, so the mask bit stays clear and
    // the IR-level gate would trap CapabilityDenied.
    let denied = Capabilities::default();
    let mut vt = CapabilityVtable::with_capacity(64);
    let populated = vt.register_via_gate(&denied, CapabilityBit::ReadsClock);
    assert!(!populated, "denied gate must leave the bit clear");
    assert!(!vt.is_granted(CapabilityBit::ReadsClock.bit_index()));
    assert_eq!(vt.caps_mask(), 0);

    // Granting the bit on the policy flips the same gate to populate.
    let granted = Capabilities::all_granted();
    let mut vt2 = CapabilityVtable::with_capacity(64);
    assert!(vt2.register_via_gate(&granted, CapabilityBit::ReadsClock));
    assert!(vt2.is_granted(CapabilityBit::ReadsClock.bit_index()));
}

#[test]
fn sandbox_vtable_dispatch_state() {
    // DISPATCH: a source-lowered `Op::CallNative` resolves the
    // import_idx-keyed callable through the host-fn registry half —
    // exactly the cranelift `host_fns` split.
    let mut vt = CapabilityVtable::with_capacity(64);
    assert!(vt.resolve_host_fn(0).is_none());
    vt.register_host_fn(0, Arc::new(AddSeven));
    assert_eq!(vt.host_fn_count(), 1);
    let f = vt.resolve_host_fn(0).expect("registered callable");
    let r = f
        .call(
            NativeArgs::from_positional(vec![Value::Int(35)], Arc::new(NoCb)),
            TokenRange::default(),
        )
        .expect("dispatch");
    assert_eq!(r, Value::Int(42));
}

#[test]
fn sandbox_trap_kind_lifts_capability_denied() {
    // The deny trap arm records CapabilityDenied; lifting it produces
    // the same RuntimeError class the cranelift gate produces.
    let err = SandboxTrapKind::CapabilityDenied.to_runtime_error(TokenRange::default());
    assert!(matches!(err, RuntimeError::CapabilityDenied { .. }));
    // Numbering parity: 3 across backends.
    assert_eq!(SandboxTrapKind::CapabilityDenied as u64, 3);
    assert_eq!(
        SandboxTrapKind::from_code(3),
        SandboxTrapKind::CapabilityDenied
    );
}

#[test]
fn sandbox_config_mirrors_cranelift_knobs() {
    assert_eq!(
        SandboxConfig::default(),
        SandboxConfig {
            bounds_check: true,
            deadline_check: true,
            capability_check: true,
            div_check: true,
        }
    );
    let u = SandboxConfig::unchecked();
    assert!(!u.capability_check && !u.div_check && !u.bounds_check && !u.deadline_check);
}

#[test]
fn vtable_symbol_registry_resolves_host_helpers() {
    // The LLVM "vtable" is a symbol registry (vs cranelift's data-slot
    // vtable). Every slot resolves to a non-null host address under a
    // stable symbol the emitted module declares.
    let mappings = populate_global_mappings();
    assert_eq!(mappings.len() as u32, VtableSlot::COUNT);
    for (sym, addr) in mappings {
        assert!(!sym.is_empty());
        assert_ne!(addr, 0, "host helper {sym} must resolve to a real address");
    }
    // The native-dispatch slot carries the same symbol `state.rs`
    // exposes for `add_global_mapping`.
    assert_eq!(
        VtableSlot::RelonCallNative.symbol(),
        "relon_llvm_call_native"
    );
}

// ---------------------------------------------------------------------
// 4. Cranelift gold-standard anchor — grant / deny / elide outcomes
// ---------------------------------------------------------------------
//
// Cranelift carries the full runtime harness (state pointer + vtable +
// cap_lookup helper), so it can run the gate to completion and pin the
// observable grant / deny / elide *outcomes* the LLVM IR encodes. This
// is the same anchoring pattern `phase0b_call.rs` established.

use relon_codegen_cranelift::{
    AotEvaluator, CapabilityVtable as CraneliftVtable, HostFnPtr,
    SandboxConfig as CraneliftSandboxConfig,
};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp, NO_CAPABILITY_BIT};

unsafe extern "C" fn now_stub(_arg: i64) -> i64 {
    1_700_000_000
}

fn build_checkcap_ir(cap_bit: u32) -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![
                TaggedOp {
                    op: Op::CheckCap { cap_bit },
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn anchor_cranelift_denies_when_bit_ungranted() {
    let ir = build_checkcap_ir(2);
    let ev = AotEvaluator::from_ir_direct(ir, CraneliftSandboxConfig::default(), vec!["x".into()])
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
fn anchor_cranelift_grants_when_bit_registered() {
    let ir = build_checkcap_ir(2);
    let mut ev =
        AotEvaluator::from_ir_direct(ir, CraneliftSandboxConfig::default(), vec!["x".into()])
            .expect("cranelift compile");
    let mut vt = CraneliftVtable::with_capacity(64);
    let fn_ptr: HostFnPtr = now_stub;
    vt.register(2, fn_ptr);
    ev.install_capabilities_mut(Arc::new(vt));
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = ev.run_main(args).expect("granted bit must run the body");
    assert_eq!(result, Value::Int(99));
}

#[test]
fn anchor_cranelift_no_capability_bit_elides_gate() {
    let ir = build_checkcap_ir(NO_CAPABILITY_BIT);
    let ev = AotEvaluator::from_ir_direct(ir, CraneliftSandboxConfig::default(), vec!["x".into()])
        .expect("cranelift compile");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = ev.run_main(args).expect("sentinel must elide the gate");
    assert_eq!(result, Value::Int(99));
}

/// Cross-backend numbering: the LLVM `SandboxTrapKind::CapabilityDenied`
/// MUST match the cranelift `TrapKind::CapabilityDenied` numeric code so
/// a denied gate decodes to the same cause on both AOT backends.
#[test]
fn anchor_trap_numbering_matches_cranelift() {
    use relon_codegen_cranelift::TrapKind as CraneliftTrapKind;
    assert_eq!(
        SandboxTrapKind::CapabilityDenied as u64,
        CraneliftTrapKind::CapabilityDenied as u8 as u64
    );
    assert_eq!(
        SandboxTrapKind::DivisionByZero as u64,
        CraneliftTrapKind::DivisionByZero as u8 as u64
    );
    assert_eq!(
        SandboxTrapKind::NumericOverflow as u64,
        CraneliftTrapKind::NumericOverflow as u8 as u64
    );
}
