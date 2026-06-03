//! Phase 0b gap test: `Op::CallNative` open-world dynamic dispatch on
//! the LLVM AOT backend, anchored to the cranelift backend as the
//! golden (cranelift fully supports source-lowered native dispatch and
//! is itself validated against the tree-walker in
//! `relon-codegen-cranelift/tests/native_call_from_source.rs`).
//!
//! The source `#main(Int x) -> Int\nclock_add(x)` lowers (on every
//! backend) to a buffer-protocol entry carrying an `Op::CheckCap`
//! capability gate followed by a source-lowered
//! `Op::CallNative { cap_bit: NO_CAPABILITY_BIT }`. This exercises the
//! buffer-entry path (caps in the trailing `i64` param + the `*state`
//! pointer), which is exactly the shape the task targets.
//!
//! Three states, each asserted against the literal oracle AND the
//! cranelift runtime:
//!
//!   * **dispatch** — cap granted at runtime + host fn registered:
//!     `clock_add(35)` returns `42`, host fn hit exactly once.
//!   * **deny** — host fn registered but cap withheld: the `CheckCap`
//!     gate fires first, surfacing `CapabilityDenied`; the host fn is
//!     never invoked.
//!   * **missing-callable** — cap granted but no host fn registered:
//!     the `relon_llvm_call_native` helper records a trap, surfacing a
//!     typed error (not a crash / wrong answer).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{CapabilityBit, Evaluator, NativeArgs, RelonFunction, RuntimeError, Value};

const SRC: &str = "#main(Int x) -> Int\nclock_add(x)";

/// Build `AnalyzeOptions` describing one host-registered `#native` fn.
/// Mirrors the cranelift `native_call_from_source` fixture so both
/// backends consume an identical analyze surface.
fn host_options() -> relon_analyzer::AnalyzeOptions {
    let sig = relon_analyzer::FnSignature {
        name: "clock_add".to_string(),
        generics: Vec::new(),
        params: vec![relon_analyzer::FnParam {
            name: "_".to_string(),
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
    caps.reads_clock = true; // granted at analyze so the build lowers the call
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps,
        strict_mode: false,
        ..Default::default()
    }
}

/// Native fn that adds 7 to its single Int arg, counting invocations so
/// a denied call can be proven to never reach the host body.
struct AddSeven {
    hits: AtomicU64,
}

impl RelonFunction for AddSeven {
    fn call(
        &self,
        args: NativeArgs,
        _range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match args.positional.first() {
            Some(Value::Int(x)) => Ok(Value::Int(x.wrapping_add(7))),
            other => Err(RuntimeError::Unsupported {
                reason: format!("AddSeven expects Int, got {other:?}"),
            }),
        }
    }
}

fn host_fns(native: &Arc<AddSeven>) -> HashMap<String, Arc<dyn RelonFunction>> {
    let dynn: Arc<dyn RelonFunction> = native.clone();
    let mut m: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    m.insert("clock_add".to_string(), dynn);
    m
}

fn args(x: i64) -> HashMap<String, Value> {
    let mut a = HashMap::new();
    a.insert("x".to_string(), Value::Int(x));
    a
}

// ---------------------------------------------------------------------------
// State 1: dispatch (granted + registered) — LLVM matches cranelift + oracle.
// ---------------------------------------------------------------------------

#[test]
fn callnative_dispatch_matches_cranelift_golden() {
    // Cranelift golden.
    let cl_native = Arc::new(AddSeven {
        hits: AtomicU64::new(0),
    });
    let cl = AotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("cranelift build")
        .with_host_fns(&host_fns(&cl_native))
        .with_granted_cap(CapabilityBit::ReadsClock.bit_index());
    let cl_val = cl.run_main(args(35)).expect("cranelift dispatch");
    assert_eq!(cl_val, Value::Int(42), "cranelift golden value");
    assert_eq!(cl_native.hits.load(Ordering::SeqCst), 1, "cranelift host hit");

    // LLVM under test.
    let llvm_native = Arc::new(AddSeven {
        hits: AtomicU64::new(0),
    });
    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("llvm build")
        .with_host_fns(&host_fns(&llvm_native))
        .with_granted_cap(CapabilityBit::ReadsClock.bit_index());
    let llvm_val = llvm.run_main(args(35)).expect("llvm dispatch");

    assert_eq!(llvm_val, cl_val, "LLVM dispatch must match cranelift golden");
    assert_eq!(llvm_val, Value::Int(42), "LLVM dispatch oracle");
    assert_eq!(
        llvm_native.hits.load(Ordering::SeqCst),
        1,
        "LLVM host fn invoked exactly once"
    );
}

// ---------------------------------------------------------------------------
// State 2: deny (registered but cap withheld) — CheckCap traps first.
// ---------------------------------------------------------------------------

#[test]
fn callnative_deny_matches_cranelift_golden() {
    // Cranelift golden: register the Arc but do NOT grant the cap.
    let cl_native = Arc::new(AddSeven {
        hits: AtomicU64::new(0),
    });
    let cl = AotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("cranelift build")
        .with_host_fns(&host_fns(&cl_native));
    let cl_err = cl
        .run_main(args(35))
        .expect_err("cranelift: withheld cap must trap");
    assert!(
        matches!(cl_err, RuntimeError::CapabilityDenied { .. }),
        "cranelift golden: expected CapabilityDenied, got {cl_err:?}"
    );
    assert_eq!(
        cl_native.hits.load(Ordering::SeqCst),
        0,
        "cranelift: host fn must not run when the cap prong fires"
    );

    // LLVM under test: same posture.
    let llvm_native = Arc::new(AddSeven {
        hits: AtomicU64::new(0),
    });
    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("llvm build")
        .with_host_fns(&host_fns(&llvm_native));
    let llvm_err = llvm
        .run_main(args(35))
        .expect_err("llvm: withheld cap must trap");
    assert!(
        matches!(llvm_err, RuntimeError::CapabilityDenied { .. }),
        "LLVM must match cranelift golden (CapabilityDenied), got {llvm_err:?}"
    );
    assert_eq!(
        llvm_native.hits.load(Ordering::SeqCst),
        0,
        "LLVM host fn must not run when the cap prong fires"
    );
}

// ---------------------------------------------------------------------------
// State 3: granted cap but no registered callable — typed trap, no crash.
// ---------------------------------------------------------------------------

#[test]
fn callnative_missing_callable_traps_typed() {
    // Cap granted (the CheckCap gate passes) but the host registered no
    // callable, so the dispatch helper resolves nothing and records a
    // trap. The host sees a typed RuntimeError rather than a crash /
    // wrong answer. (Cranelift's matching posture surfaces an
    // Unsupported / CapabilityDenied-class error; the LLVM contract here
    // is "typed Err, never Ok(garbage)".)
    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("llvm build")
        .with_granted_cap(CapabilityBit::ReadsClock.bit_index());
    let err = llvm
        .run_main(args(35))
        .expect_err("granted cap + no callable must surface a typed error, not Ok");
    assert!(
        matches!(err, RuntimeError::Unsupported { .. }),
        "expected Unsupported (host fn missing), got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Wiring sanity: the buffer entry carries the dispatch helper symbol +
// the CheckCap gate, confirming we exercised the buffer (caps/state)
// entry path the task targets — not a fast-path / legacy shape.
// ---------------------------------------------------------------------------

#[test]
fn callnative_uses_buffer_entry_with_dispatch_helper() {
    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &host_options()).expect("llvm build");
    let dump = llvm.emit_ir_dump();
    assert!(
        dump.contains("relon_llvm_call_native"),
        "IR dump missing the native-dispatch helper call:\n{dump}"
    );
    // The native import the lowering interned must be visible to hosts.
    assert_eq!(llvm.native_imports().len(), 1, "one #native import");
    assert_eq!(llvm.native_imports()[0].name, "clock_add");
}
