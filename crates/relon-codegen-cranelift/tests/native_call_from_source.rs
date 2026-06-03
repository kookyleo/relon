//! v5-β-2 stage 5: source-level native-call lowering reaches the
//! cranelift backend.
//!
//! The producer half of the capability/trust model (the IR lowering
//! pass that turns a host-registered free-call into an
//! `Op::CheckCap`-guarded `Op::CallNative`) is backend-agnostic, so
//! these pin that a gated native call is enforceable end-to-end from
//! Relon source on cranelift:
//!
//! * the single-file static capability-reachability check fails the
//!   build when the gated call's cap isn't granted;
//! * a build that passes the static check lowers the guarded call, and
//!   the `Op::CheckCap` prologue traps `CapabilityDenied` at runtime
//!   when the cap slot is unregistered.
//!
//! Full host-fn *dispatch* on this backend (the granted, slot-
//! registered path) additionally needs an `Arc<dyn RelonFunction>` →
//! `extern "C"` thunk and a vtable that separates the import-slot
//! namespace from the cap-bit namespace — tracked in the
//! capability/trust model doc §9.2.

use std::collections::{HashMap, HashSet};

use relon_codegen_cranelift::{AotEvaluator, CraneliftError};
use relon_eval_api::{Evaluator, RuntimeError, Value};

/// Build `AnalyzeOptions` describing one host-registered native fn.
fn host_options(
    name: &str,
    params: &[&str],
    ret: &str,
    gate: relon_analyzer::NativeFnGate,
    caps: relon_analyzer::Capabilities,
) -> relon_analyzer::AnalyzeOptions {
    let sig = relon_analyzer::FnSignature {
        name: name.to_string(),
        generics: Vec::new(),
        params: params
            .iter()
            .map(|p| relon_analyzer::FnParam {
                name: "_".to_string(),
                ty: relon_analyzer::type_node_simple(p),
                optional: false,
            })
            .collect(),
        return_type: relon_analyzer::type_node_simple(ret),
        variadic_tail: None,
    };
    let mut signatures = HashMap::new();
    signatures.insert(name.to_string(), sig);
    let mut gates = HashMap::new();
    gates.insert(name.to_string(), gate);
    let mut names = HashSet::new();
    names.insert(name.to_string());
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps,
        strict_mode: false,
        ..Default::default()
    }
}

fn reads_clock_gate() -> relon_analyzer::NativeFnGate {
    let mut gate = relon_analyzer::NativeFnGate::default();
    gate.reads_clock = true;
    gate
}

fn reads_clock_caps() -> relon_analyzer::Capabilities {
    let mut caps = relon_analyzer::Capabilities::default();
    caps.reads_clock = true;
    caps
}

#[test]
fn native_call_static_check_rejects_ungranted_cap_from_source() {
    // Zero-trust analyze caps: the static reachability check flags the
    // gated `clock_add` call (Error severity), so the build fails.
    let opts = host_options(
        "clock_add",
        &["Int"],
        "Int",
        reads_clock_gate(),
        relon_analyzer::Capabilities::default(),
    );
    let result = AotEvaluator::from_source_with_options("#main(Int x) -> Int\nclock_add(x)", &opts);
    match result {
        Err(CraneliftError::Analyze(_)) => {}
        Err(other) => panic!("expected Analyze error, got {other:?}"),
        Ok(_) => panic!("ungranted gated native call must fail the static check"),
    }
}

#[test]
fn native_call_runtime_deny_traps_when_cap_slot_unregistered() {
    // Static caps grant reads_clock so the build passes and lowers the
    // guarded call; the default sandbox vtable registers nothing, so
    // the `Op::CheckCap` prologue traps `CapabilityDenied` at runtime.
    let opts = host_options(
        "clock_add",
        &["Int"],
        "Int",
        reads_clock_gate(),
        reads_clock_caps(),
    );
    let evaluator =
        AotEvaluator::from_source_with_options("#main(Int x) -> Int\nclock_add(x)", &opts)
            .expect("granted-at-analyze source must build");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(35));
    let err = evaluator
        .run_main(args)
        .expect_err("unregistered cap slot must trap");
    assert!(
        matches!(err, RuntimeError::CapabilityDenied { .. }),
        "expected CapabilityDenied, got {err:?}"
    );
}
