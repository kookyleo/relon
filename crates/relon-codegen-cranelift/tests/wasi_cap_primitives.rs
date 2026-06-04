//! P-clock / P-random — built-in WASI-backed capability primitives,
//! cranelift native backend.
//!
//! The cranelift backend lowers the source-level `clock()` / `random()`
//! primitives (`Op::ReadClock` / `Op::ReadRandom`) to an indirect call
//! through the capability vtable's `RelonClockWall` / `RelonRandom`
//! host helpers (SystemTime / /dev/urandom). The capability gate is the
//! preceding `Op::CheckCap` (cranelift resolves the bit's vtable slot
//! via `cap_lookup`; a null slot traps `CapabilityDenied`).
//!
//! Non-determinism honesty: clock / random values are NOT bit-equal, so
//! the assertions are per-tier credibility (clock lands in a wall-clock
//! window; random is non-constant) plus the capability gate (slot
//! granted -> value, slot empty -> trap).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use relon_codegen_cranelift::{AotEvaluator, CapabilityVtable, HostFnPtr, SandboxConfig};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

/// Canonical capability bit indices (mirror `relon_cap::CapabilityBit`).
const READS_CLOCK: u32 = 3;
const USES_RNG: u32 = 5;

/// Dummy host fn registered to mark a vtable slot as granted — the
/// cranelift `Op::CheckCap` only checks that `cap_lookup(bit)` is
/// non-null, so any non-null fn ptr opens the gate.
unsafe extern "C" fn grant_marker(_arg: i64) -> i64 {
    0
}

/// IR: `CheckCap { cap_bit } ; <read op> ; Return`. Nullary `#main`
/// (no params) returning the i64 the primitive pushed.
fn build_primitive_module(cap_bit: u32, read_op: Op) -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![],
            ret: IrType::I64,
            body: vec![
                TaggedOp {
                    op: Op::CheckCap { cap_bit },
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: read_op,
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

fn granted_evaluator(cap_bit: u32, read_op: Op) -> AotEvaluator {
    let ir = build_primitive_module(cap_bit, read_op);
    let mut ev = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec![])
        .expect("compile primitive module");
    let mut vt = CapabilityVtable::with_capacity(64);
    let fp: HostFnPtr = grant_marker;
    vt.register(cap_bit, fp);
    ev.install_capabilities_mut(Arc::new(vt));
    ev
}

#[test]
fn cranelift_clock_granted_lands_in_wall_clock_window() {
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let ev = granted_evaluator(READS_CLOCK, Op::ReadClock);
    let v = ev.run_main(HashMap::new()).expect("run clock");
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let ns = match v {
        Value::Int(n) => n,
        other => panic!("clock() expected Int, got {other:?}"),
    };
    let slack = 5_000_000_000i64;
    assert!(
        ns >= before - slack && ns <= after + slack,
        "cranelift clock {ns} ns outside window [{before}, {after}] (+/-5s)"
    );
}

#[test]
fn cranelift_clock_ungranted_traps() {
    // No vtable slot registered for the bit -> cap_lookup is null -> trap.
    let ir = build_primitive_module(READS_CLOCK, Op::ReadClock);
    let ev = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec![]).expect("compile");
    let err = ev
        .run_main(HashMap::new())
        .expect_err("ungranted clock must trap");
    assert!(
        matches!(err, RuntimeError::CapabilityDenied { .. }),
        "expected CapabilityDenied, got {err:?}"
    );
}

#[test]
fn cranelift_random_granted_is_non_constant() {
    let pick = || match granted_evaluator(USES_RNG, Op::ReadRandom)
        .run_main(HashMap::new())
        .expect("run random")
    {
        Value::Int(n) => n,
        other => panic!("random() expected Int, got {other:?}"),
    };
    let (a, b, c) = (pick(), pick(), pick());
    assert!(
        !(a == b && b == c),
        "three cranelift random() reads identical ({a}); RNG frozen"
    );
}

#[test]
fn cranelift_random_ungranted_traps() {
    let ir = build_primitive_module(USES_RNG, Op::ReadRandom);
    let ev = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec![]).expect("compile");
    let err = ev
        .run_main(HashMap::new())
        .expect_err("ungranted random must trap");
    assert!(
        matches!(err, RuntimeError::CapabilityDenied { .. }),
        "expected CapabilityDenied, got {err:?}"
    );
}
