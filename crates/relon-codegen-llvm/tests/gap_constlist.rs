//! Phase 0b collections close-out: `Op::ConstListInt` /
//! `ConstListFloat` / `ConstListBool` lowering for the LLVM-AOT backend.
//!
//! ## Validated at codegen + byte-layout *and* (since S2.②) end to end
//!
//! A const-list literal only surfaces in a source whose `#main` returns
//! (or stores) a `List<scalar>` field. The LLVM-AOT host-side return
//! *decoder* (`read_value_from_reader` in `evaluator.rs`) used to refuse
//! a `List<T>` return field (Phase B limitation). Phase 1 Stage 2.②
//! widened it: `read_value_from_reader` now has a `List<Int>` arm
//! (`marshal_list_int_out`) walking the `[len][i64…]` tail record, so the
//! value-level three-way `run_main` diff against the cranelift golden +
//! tree-walk gold standard is now reachable (see
//! `int_list_run_main_three_way_after_return_decode` below).
//!
//! What this file pins:
//!
//! 1. **Op presence** — the const-list source lowers to the target op
//!    (a future lowering change that stops emitting it trips the
//!    assert).
//! 2. **Codegen parity** — both the LLVM backend and the cranelift
//!    golden accept the source through `from_source`. Before this
//!    change the LLVM side raised an `unsupported` codegen error at the
//!    `ConstListInt` arm; cranelift (which carries `list_int_offsets`)
//!    accepted it. Parity now holds — the LLVM lowering emits real IR.
//! 3. **IR shape** — the emitted LLVM IR materialises the const-pool
//!    offset and copies the `[len][payload]` list record into the
//!    return buffer's tail area (the `EmitTailRecordFromAbsoluteAddr`
//!    `ListInt` path: `shl 3` + `add 8` size, 8-aligned `llvm.memcpy`),
//!    so the op is provably wired rather than a silent no-op.
//!
//! ## Float / Bool reachability
//!
//! Only `ConstListInt` is reachable through `from_source`: a `List<Int>`
//! schema field lowers cleanly, but `List<Float>` / `List<Bool>` schema
//! fields are refused by the *frontend* lowering pass
//! (`LoweringError`: "unsupported dict field type `List<Float>`") and
//! the inline `: [..]` expression form for these does not parse. That
//! gate is in `relon-ir` (shared, outside this task's two-file
//! envelope), so no `from_source` shape exercises the float / bool
//! lowering arms today. Their layout is therefore pinned at the
//! const-pool layer in `src/codegen/mod.rs::const_pool_tests`
//! (byte-identical to cranelift's `visit_const_list_float/bool`); the
//! `emit_const_list` lowering arm itself is shared across all three
//! list types (a single `match` on the offset map) and is exercised
//! end-to-end by the Int case below.
//!
//! Both `ConstPool`s are crate-private (`pub(crate)` / private `mod`)
//! so a `tests/` integration test cannot read their bytes directly —
//! hence the byte-level parity lives in the in-crate unit test module.

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_ir::ir::{Op, TaggedOp};

/// `List<Int>` literal stored into a returned schema field. The only
/// `from_source`-reachable const-list shape (see module docs).
const INT_LIST_SRC: &str = "#schema R { List<Int> xs: * }\n\
                            #main(Int n) -> R\n\
                            { xs: [10, 20, 30] }";

/// Lower `src` to IR (non-strict, matching the LLVM backend) and
/// flatten every op in every function body, recursing into structured
/// ops, so a test can assert a given op is present.
fn flatten_ops(src: &str) -> Vec<Op> {
    let options = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let lowered = relon_ir::compile(src, &options).expect("frontend compile");
    let mut out = Vec::new();
    for func in &lowered.module.funcs {
        collect(&func.body, &mut out);
    }
    out
}

fn collect(body: &[TaggedOp], out: &mut Vec<Op>) {
    for t in body {
        out.push(t.op.clone());
        match &t.op {
            Op::Block { body, .. } | Op::Loop { body, .. } => collect(body, out),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect(then_body, out);
                collect(else_body, out);
            }
            _ => {}
        }
    }
}

/// Assert both backends accept `src` through `from_source`. The
/// cranelift golden's acceptance is the codegen oracle: cranelift
/// supports `ConstList*` (it carries `list_*_offsets`), so the LLVM
/// port must too. Returns the LLVM post-opt IR dump for shape asserts.
fn assert_codegen_parity(src: &str) -> String {
    AotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("cranelift golden must compile src:\n{src}\nerr: {e:?}"));
    let llvm = LlvmAotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("llvm backend must compile src:\n{src}\nerr: {e:?}"));
    llvm.emit_ir_dump().to_string()
}

#[test]
fn int_list_lowers_to_const_list_int() {
    let ops = flatten_ops(INT_LIST_SRC);
    assert!(
        ops.iter().any(|o| matches!(o, Op::ConstListInt { .. })),
        "INT_LIST_SRC must lower to Op::ConstListInt; ops:\n{ops:#?}"
    );
}

#[test]
fn int_list_codegen_parity_and_shape() {
    let dump = assert_codegen_parity(INT_LIST_SRC);
    // The const-pool offset is pushed, then the `ListInt`
    // `EmitTailRecordFromAbsoluteAddr` copies the `[len][i64...]` record
    // into the return buffer's tail area: size = (len << 3) + 8, copied
    // through an 8-aligned `llvm.memcpy`.
    assert!(
        dump.contains("tail_rec_size8"),
        "int-list IR must compute the ListInt tail-record size (shl 3 + 8). Dump:\n{dump}"
    );
    assert!(
        dump.contains("llvm.memcpy"),
        "int-list IR must memcpy the list record into the tail. Dump:\n{dump}"
    );
}

/// S2.② closed the Phase B `List<T>` return-decode gap this test used to
/// pin as blocked: `read_value_from_reader` now has a `List<Int>` arm
/// (`marshal_list_int_out`) that walks the `[len][i64…]` tail record. The
/// old negative assertion (`run_main` is_err) is therefore promoted, per
/// its own instructions, to a real three-way value diff: the LLVM buffer
/// body decode must match both the cranelift golden and the tree-walk
/// gold standard for the schema-wrapped `List<Int>` return.
#[test]
fn int_list_run_main_three_way_after_return_decode() {
    use std::collections::HashMap;
    use std::sync::Arc;

    use relon_eval_api::{Evaluator, Value};
    use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
    use relon_parser::parse_document;

    /// Pull the `xs` field's `Vec<i64>` out of the returned schema dict.
    fn xs_of(v: &Value) -> Vec<i64> {
        let dict = match v {
            Value::Dict(d) => d,
            other => panic!("expected schema Dict return, got {other:?}"),
        };
        let list = dict.map.get("xs").expect("return dict has `xs` field");
        match list {
            Value::List(items) => items
                .iter()
                .map(|e| match e {
                    Value::Int(n) => *n,
                    other => panic!("expected Int element, got {other:?}"),
                })
                .collect(),
            other => panic!("expected List for `xs`, got {other:?}"),
        }
    }

    // Tree-walk gold standard.
    let node = parse_document(INT_LIST_SRC).expect("parse INT_LIST_SRC");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    let mut tw_args = HashMap::new();
    tw_args.insert("n".to_string(), Value::Int(0));
    let want = xs_of(&walker.run_main(&scope, tw_args).expect("tree-walk run_main"));
    assert_eq!(want, vec![10, 20, 30], "oracle sanity");

    let llvm = LlvmAotEvaluator::from_source(INT_LIST_SRC).expect("llvm compiles the lowering");
    let cl = AotEvaluator::from_source(INT_LIST_SRC).expect("cranelift golden compiles");

    let mut a = HashMap::new();
    a.insert("n".to_string(), Value::Int(0));

    let got_llvm = xs_of(&llvm.run_main(a.clone()).expect("llvm run_main decodes List<Int>"));
    let got_cl = xs_of(&cl.run_main(a).expect("cranelift run_main"));

    assert_eq!(got_llvm, want, "LLVM List<Int> return decode diverged from oracle");
    assert_eq!(got_cl, want, "cranelift List<Int> return decode diverged from oracle");
}
