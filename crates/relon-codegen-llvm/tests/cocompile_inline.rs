//! Stage 1.B — LTO co-compile inline test (closed-world `CallNative`).
//!
//! Differential + inline-count proof that the closed-world native
//! dispatch path actually *inlines* the host fn rather than silently
//! falling back to the open-world dynamic helper (a value-only check
//! would pass even on the wrong path, so we assert the inline counts
//! too — risk 2 in the execution plan).
//!
//! Fixture: a hand-built legacy-i64 entry `(i64 x) -> i64` whose body
//! is `LocalGet(0); CallNative host_add7(x); Return`. The host shim
//! `host_add7` is `#[no_mangle] extern "C" fn(i64) -> i64 { x + 7 }`.
//!
//! Assertions (post-O3):
//!   1. **zero** `call @relon_llvm_call_native` — the open-world
//!      dynamic helper was never emitted on this path.
//!   2. **zero** `call @host_add7` — the linked host fn was inlined
//!      away (the `add ... 7` survives in the entry instead).
//!   3. the **pre-link** IR carried a direct `call ... @host_add7` —
//!      proves the closed-world emit produced the static-dispatch
//!      shape (not the dynamic helper) before inlining erased it.
//!   4. **value** `run_i64(35) == 42`, byte-equal to the open-world
//!      LLVM path's result for the same `x + 7` semantics (the
//!      open-world path is itself anchored to cranelift's
//!      `native_call_from_source.rs` golden in `gap_callnative.rs`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use relon_codegen_llvm::cocompile::cocompile_legacy_i64;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{CapabilityBit, Evaluator, NativeArgs, RelonFunction, RuntimeError, Value};
use relon_ir::ir::{
    Func, IrType, Module as IrModule, NativeImport, Op, TaggedOp, NO_CAPABILITY_BIT,
};
use relon_parser::TokenRange;

const HOST_FN: &str = "host_add7";

/// `#[no_mangle] extern "C"` host shim the co-compile links in. The
/// `+ 7` mirrors `gap_callnative.rs`'s `AddSeven` so the closed-world
/// value lines up with the open-world / cranelift golden.
const HOST_SHIM_SRC: &str = r#"
#[no_mangle]
pub extern "C" fn host_add7(x: i64) -> i64 {
    x.wrapping_add(7)
}
"#;

fn tagged(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Hand-built closed-world legacy-i64 module:
/// `#main(Int x) -> Int : host_add7(x)`.
fn build_closed_world_ir() -> IrModule {
    let body = vec![
        tagged(Op::LocalGet(0)),
        tagged(Op::CallNative {
            import_idx: 0,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: NO_CAPABILITY_BIT,
        }),
        tagged(Op::Return),
    ];
    let func = Func {
        name: "run_main".to_string(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    };
    IrModule {
        imports: vec![NativeImport {
            name: HOST_FN.to_string(),
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: NO_CAPABILITY_BIT,
        }],
        funcs: vec![func],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

// --- open-world differential oracle (anchored to gap_callnative) -----------

const OPEN_WORLD_SRC: &str = "#main(Int x) -> Int\nclock_add(x)";

fn open_world_options() -> relon_analyzer::AnalyzeOptions {
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
    caps.reads_clock = true;
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps,
        strict_mode: false,
        ..Default::default()
    }
}

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

/// Run the open-world LLVM path (dynamic `relon_llvm_call_native`
/// helper) for `clock_add(35)`; this is the differential oracle.
fn open_world_value(x: i64) -> i64 {
    let native = Arc::new(AddSeven {
        hits: AtomicU64::new(0),
    });
    let dynn: Arc<dyn RelonFunction> = native.clone();
    let mut host_fns: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    host_fns.insert("clock_add".to_string(), dynn);
    let llvm = LlvmAotEvaluator::from_source_with_options(OPEN_WORLD_SRC, &open_world_options())
        .expect("open-world build")
        .with_host_fns(&host_fns)
        .with_granted_cap(CapabilityBit::ReadsClock.bit_index());
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(x));
    match llvm.run_main(args).expect("open-world dispatch") {
        Value::Int(v) => v,
        other => panic!("open-world returned non-Int: {other:?}"),
    }
}

// --- the test --------------------------------------------------------------

#[test]
fn closed_world_callnative_inlines_host_fn_and_matches_open_world() {
    let ir = build_closed_world_ir();
    let cc = cocompile_legacy_i64(&ir, HOST_SHIM_SRC).expect("co-compile closed-world module");

    // (3) The pre-link IR must carry the direct static-dispatch call to
    // the host symbol — proves the closed-world emit produced
    // `call @host_add7`, NOT the dynamic helper.
    let pre = &cc.ir_before_link;
    assert!(
        pre.contains(&format!("@{HOST_FN}")),
        "pre-link IR must reference the host symbol @{HOST_FN} (direct dispatch); got:\n{pre}"
    );
    assert!(
        !pre.contains("relon_llvm_call_native"),
        "pre-link IR must NOT reference the open-world helper on the closed-world path; got:\n{pre}"
    );

    let post = &cc.ir_after_opt;
    if std::env::var_os("RELON_DUMP_COCOMPILE").is_some() {
        eprintln!("--- PRE-LINK IR ---\n{pre}\n--- POST-O3 IR ---\n{post}");
    }

    // Positive inline proof: the host fn body (`x + 7`) must have been
    // folded INTO the entry. Without this, the zero-`call` assertions
    // below could pass vacuously if codegen emitted nothing at all.
    assert!(
        post.contains("add i64") || post.contains("add nsw i64") || post.contains(", 7"),
        "post-O3 IR must contain the inlined `add ... 7` from the host fn body; got:\n{post}"
    );

    // (1) Zero open-world dynamic-helper calls.
    assert_eq!(
        count_calls_to(post, "relon_llvm_call_native"),
        0,
        "post-O3 IR must have ZERO `call @relon_llvm_call_native` (open-world helper); got:\n{post}"
    );

    // (2) Zero residual calls to the host fn — it was inlined.
    assert_eq!(
        count_calls_to(post, HOST_FN),
        0,
        "post-O3 IR must have ZERO `call @{HOST_FN}` (host fn must be inlined); got:\n{post}"
    );

    // (4) Value differential: closed-world == open-world == 42.
    let closed = cc.run_i64(35).expect("run closed-world entry");
    let open = open_world_value(35);
    assert_eq!(open, 42, "open-world oracle (clock_add(35)) must be 42");
    assert_eq!(
        closed, open,
        "closed-world result ({closed}) must byte-match the open-world path ({open})"
    );
    assert_eq!(closed, 42, "closed-world host_add7(35) must be 42");
}

/// Count `call`-instruction references to the LLVM symbol `@name` in
/// the textual IR (matches both `call` and `tail call`, both direct
/// and through a bitcast). Conservative substring match on
/// `@<name>(` / `@<name> ` is enough for the spike fixture.
fn count_calls_to(ir: &str, name: &str) -> usize {
    ir.lines()
        .filter(|line| {
            let l = line.trim_start();
            (l.starts_with("call ") || l.starts_with("tail call ") || l.contains(" call "))
                && (l.contains(&format!("@{name}(")) || l.contains(&format!("@{name} ")))
        })
        .count()
}
