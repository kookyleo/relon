//! Stage 2.⑤ — source-driven closed-world LTO co-compile on the
//! buffer-protocol path.
//!
//! Where `cocompile_inline.rs` proves the closed-world dispatch on a
//! hand-built legacy-i64 fixture, this test drives the *real* source →
//! buffer-protocol pipeline: a `.relon` `#main` calling a host
//! `#native` fn, lowered with `AnalyzeOptions` (so the `#native`
//! declaration resolves), emitted closed-world (`Op::CallNative` →
//! `call @<host_symbol>`), with the host shim crate linked + inlined by
//! the `crate::cocompile` orchestration.
//!
//! Assertions (post-O3, on the JIT module IR dump):
//!   1. **zero** `call @relon_llvm_call_native` — the open-world dynamic
//!      helper was never emitted on the closed-world path.
//!   2. **zero** `call @add_seven` — the linked host fn was inlined
//!      away (its `+ 7` body folds into the entry).
//!   3. the inlined `add ... 7` survives — positive inline proof so the
//!      zero-`call` checks can't pass vacuously.
//!   4. **value** the closed-world `run_main` result byte-matches the
//!      open-world `from_source_with_options` + `run_main` result for
//!      the same source (the open-world path is itself anchored to
//!      cranelift's `native_call_from_source.rs` golden).
//!
//! Plus: `emit_object_with_options` lowers the SAME closed-world source
//! to a real relocatable ELF `.o` (the production object path), proving
//! the options-carrying object seam + closed-world buffer emit work
//! end-to-end (W1-C capability-gate object emit enabler).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use relon_codegen_llvm::{LlvmAotEvaluator, WorldMode};
use relon_eval_api::{Evaluator, NativeArgs, RelonFunction, RuntimeError, Value};

/// Source: `#main(Int x) -> Int` returning `add_seven(x)`. `add_seven`
/// is a host-registered, **ungated** `#native` fn — no capability, so
/// no `Op::CheckCap` is emitted and the path is purely native dispatch
/// + inline.
const SRC: &str = "#main(Int x) -> Int\nadd_seven(x)";

const HOST_FN: &str = "add_seven";

/// `#[no_mangle] extern "C"` host shim the co-compile links in. The
/// `+ 7` mirrors cranelift's `AddSeven` golden so the closed-world
/// value lines up with the open-world / cranelift differential.
const HOST_SHIM_SRC: &str = r#"
#[no_mangle]
pub extern "C" fn add_seven(x: i64) -> i64 {
    x.wrapping_add(7)
}
"#;

/// `AnalyzeOptions` describing one ungated host-registered native fn
/// `add_seven(Int) -> Int`. Mirrors cranelift's `host_options` shape
/// (`native_call_from_source.rs`) minus the gate.
fn host_options() -> relon_analyzer::AnalyzeOptions {
    let sig = relon_analyzer::FnSignature {
        name: HOST_FN.to_string(),
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
    signatures.insert(HOST_FN.to_string(), sig);
    let mut gates = HashMap::new();
    gates.insert(HOST_FN.to_string(), relon_analyzer::NativeFnGate::default());
    let mut names = HashSet::new();
    names.insert(HOST_FN.to_string());
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps: relon_analyzer::Capabilities::default(),
        strict_mode: false,
        ..Default::default()
    }
}

/// Host fn that adds 7 to its single Int arg; counts invocations so a
/// never-dispatched path is observable.
struct AddSeven {
    hits: AtomicU64,
}

impl RelonFunction for AddSeven {
    fn call(&self, args: NativeArgs, _r: relon_parser::TokenRange) -> Result<Value, RuntimeError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match args.positional.first() {
            Some(Value::Int(x)) => Ok(Value::Int(x.wrapping_add(7))),
            other => Err(RuntimeError::Unsupported {
                reason: format!("AddSeven expects Int, got {other:?}"),
            }),
        }
    }
}

/// Differential oracle: the open-world LLVM buffer path (dynamic
/// `relon_llvm_call_native` helper) for the same source.
fn open_world_value(x: i64) -> i64 {
    let native = Arc::new(AddSeven {
        hits: AtomicU64::new(0),
    });
    let dynn: Arc<dyn RelonFunction> = native.clone();
    let mut host_fns: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    host_fns.insert(HOST_FN.to_string(), dynn);
    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("open-world build")
        .with_host_fns(&host_fns);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(x));
    match llvm.run_main(args).expect("open-world dispatch") {
        Value::Int(v) => v,
        other => panic!("open-world returned non-Int: {other:?}"),
    }
}

/// Count `call`-instruction references to the LLVM symbol `@name`.
fn count_calls_to(ir: &str, name: &str) -> usize {
    ir.lines()
        .filter(|line| {
            let l = line.trim_start();
            (l.starts_with("call ") || l.starts_with("tail call ") || l.contains(" call "))
                && (l.contains(&format!("@{name}(")) || l.contains(&format!("@{name} ")))
        })
        .count()
}

#[test]
fn source_driven_closed_world_buffer_inlines_host_fn_and_matches_open_world() {
    let cc = LlvmAotEvaluator::from_source_closed_world(SRC, &host_options(), HOST_SHIM_SRC)
        .expect("source-driven closed-world buffer build");

    let ir = cc.emit_ir_dump().to_string();
    if std::env::var_os("RELON_DUMP_COCOMPILE").is_some() {
        eprintln!("--- CLOSED-WORLD BUFFER POST-O3 IR ---\n{ir}");
    }

    // (3) Positive inline proof: the host body (`x + 7`) folded in.
    assert!(
        ir.contains("add i64") || ir.contains("add nsw i64") || ir.contains(", 7"),
        "post-O3 IR must contain the inlined `add ... 7` from the host body; got:\n{ir}"
    );

    // (1) Zero open-world dynamic-helper calls.
    assert_eq!(
        count_calls_to(&ir, "relon_llvm_call_native"),
        0,
        "closed-world IR must have ZERO `call @relon_llvm_call_native`; got:\n{ir}"
    );

    // (2) Zero residual calls to the host fn — it was inlined.
    assert_eq!(
        count_calls_to(&ir, HOST_FN),
        0,
        "closed-world IR must have ZERO `call @{HOST_FN}` (host fn must be inlined); got:\n{ir}"
    );

    // (4) Value differential: closed-world == open-world == 42.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(35));
    let closed = match cc.run_main(args).expect("closed-world dispatch") {
        Value::Int(v) => v,
        other => panic!("closed-world returned non-Int: {other:?}"),
    };
    let open = open_world_value(35);
    assert_eq!(open, 42, "open-world oracle (add_seven(35)) must be 42");
    assert_eq!(
        closed, open,
        "closed-world result ({closed}) must byte-match the open-world path ({open})"
    );
}

#[test]
fn emit_object_with_options_closed_world_produces_object() {
    // The production object path: the same closed-world source lowers to
    // a real relocatable ELF `.o`. Proves the options-carrying object
    // seam resolves the `#native` declaration AND the closed-world
    // buffer emit + host link/inline run to completion under the
    // object-codegen pipeline (not just JIT).
    let dir =
        std::env::temp_dir().join(format!("relon_cocompile_buffer_obj_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tmp");
    let out = dir.join("closed_world_main.o");

    let info = LlvmAotEvaluator::emit_object_with_options(
        SRC,
        "relon_closed_world_main",
        &out,
        &host_options(),
        WorldMode::ClosedWorld,
        Some(HOST_SHIM_SRC),
    )
    .expect("closed-world emit_object_with_options");

    assert_eq!(info.entry_symbol, "relon_closed_world_main");
    let meta = std::fs::metadata(&out).expect("object file written");
    assert!(meta.len() > 0, "emitted object must be non-empty");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn emit_object_default_open_world_still_works() {
    // The historical 3-arg `emit_object` must remain byte-identical to
    // the pre-S2.⑤ open-world path (thin wrapper around
    // `emit_object_with_options`). A non-native Int source proves the
    // wrapper didn't regress the default seam.
    let dir =
        std::env::temp_dir().join(format!("relon_cocompile_buffer_ow_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tmp");
    let out = dir.join("plain_main.o");
    let info =
        LlvmAotEvaluator::emit_object("#main(Int x) -> Int\nx + 1", "relon_plain_main", &out)
            .expect("open-world emit_object");
    assert_eq!(info.entry_symbol, "relon_plain_main");
    assert!(std::fs::metadata(&out).expect("object written").len() > 0);
    let _ = std::fs::remove_dir_all(&dir);
}
