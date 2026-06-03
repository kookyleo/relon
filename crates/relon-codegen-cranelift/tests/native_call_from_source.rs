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
//! Full host-fn *dispatch* (the granted, registered path) routes a
//! source-lowered `Op::CallNative { cap_bit: NO_CAPABILITY_BIT }`
//! through the `relon_call_native` helper to the
//! `Arc<dyn RelonFunction>` registered at the matching `import_idx`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use relon_codegen_cranelift::{AotEvaluator, CraneliftError};
use relon_eval_api::{CapabilityBit, Evaluator, NativeArgs, RelonFunction, RuntimeError, Value};

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

/// Native fn that adds 7 to its single Int arg. Counts invocations so
/// a denied call can be proven to never reach the host body.
struct AddSeven {
    hits: std::sync::atomic::AtomicU64,
}

impl RelonFunction for AddSeven {
    fn call(
        &self,
        args: NativeArgs,
        _range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match args.positional.first() {
            Some(Value::Int(x)) => Ok(Value::Int(x.wrapping_add(7))),
            other => Err(RuntimeError::Unsupported {
                reason: format!("AddSeven expects Int, got {other:?}"),
            }),
        }
    }
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

#[test]
fn native_call_dispatches_to_host_fn_when_granted_at_runtime() {
    // Static caps grant reads_clock (build passes + lowers the guarded
    // call); the runtime grants the same bit and registers the Arc host
    // fn, so the call dispatches through `relon_call_native` and returns
    // `x + 7`.
    let opts = host_options(
        "clock_add",
        &["Int"],
        "Int",
        reads_clock_gate(),
        reads_clock_caps(),
    );
    let native = Arc::new(AddSeven {
        hits: std::sync::atomic::AtomicU64::new(0),
    });
    let native_dyn: Arc<dyn RelonFunction> = native.clone();
    let mut host_fns: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    host_fns.insert("clock_add".to_string(), native_dyn);

    let evaluator =
        AotEvaluator::from_source_with_options("#main(Int x) -> Int\nclock_add(x)", &opts)
            .expect("granted-at-analyze source must build")
            .with_host_fns(&host_fns)
            .with_granted_cap(CapabilityBit::ReadsClock.bit_index());

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(35));
    let value = evaluator
        .run_main(args)
        .expect("granted native call must dispatch");
    assert_eq!(value, Value::Int(42));
    assert_eq!(
        native.hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host fn invoked exactly once"
    );
}

#[test]
fn native_call_runtime_deny_skips_host_fn_even_when_registered() {
    // Arc host fn registered, but the runtime withholds the cap grant.
    // The `Op::CheckCap` prologue must trap before `relon_call_native`
    // runs, so the host fn is never invoked.
    let opts = host_options(
        "clock_add",
        &["Int"],
        "Int",
        reads_clock_gate(),
        reads_clock_caps(),
    );
    let native = Arc::new(AddSeven {
        hits: std::sync::atomic::AtomicU64::new(0),
    });
    let native_dyn: Arc<dyn RelonFunction> = native.clone();
    let mut host_fns: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    host_fns.insert("clock_add".to_string(), native_dyn);

    // Register the Arc but do NOT grant the cap at runtime.
    let evaluator =
        AotEvaluator::from_source_with_options("#main(Int x) -> Int\nclock_add(x)", &opts)
            .expect("build")
            .with_host_fns(&host_fns);

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(35));
    let err = evaluator
        .run_main(args)
        .expect_err("withheld cap must trap");
    assert!(
        matches!(err, RuntimeError::CapabilityDenied { .. }),
        "expected CapabilityDenied, got {err:?}"
    );
    assert_eq!(
        native.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "host fn must not run when the capability prong fires"
    );
}
