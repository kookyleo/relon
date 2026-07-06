//! Direct IR-level regression anchors for the lowering peephole rules
//! (`src/lowering/peephole.rs`).
//!
//! Each test drives the public `frontend::compile` pipeline on a tiny
//! source whose body triggers exactly one rewrite rule, then asserts
//! the precise post-rewrite op shape of the lowered entry function —
//! so a broken rewrite fails HERE instead of surfacing two levels
//! later as a backend miscompile. Negative tests pin down adjacent
//! shapes that must NOT be rewritten (fall through to the generic
//! stdlib-call path) or must cap loudly (oracle `function_not_found`
//! parity), guarding against both over-matching and silent fallback.
//!
//! Covered rules: `try_lower_type_const`, `try_lower_scalar_math`,
//! `try_lower_list_len`, `try_lower_list_count`,
//! `try_lower_list_sum_range`, `try_lower_list_sum_value`,
//! `try_lower_nested_range_map_reduce`, `try_lower_len_filter_range`,
//! `try_lower_list_map` / `emit_list_hof_method`,
//! `try_lower_range_value` / `emit_range_materialize`,
//! `flatten_list_spread`, `classify_runtime_spread` /
//! `emit_list_spread_runtime_materialize`, and the const-pool vs
//! computed-element list-literal routing.

use relon_analyzer::AnalyzeOptions;
use relon_ir::{compile, stdlib_function_index, IrType, Module, Op, TaggedOp};

/// Parse + analyze + lower `src` through the shared compiled-backend
/// frontend and return the lowered module.
fn lower(src: &str) -> Module {
    compile(src, &AnalyzeOptions::default())
        .unwrap_or_else(|e| panic!("compile failed: {e}\nsource:\n{src}"))
        .module
}

/// Assert `src` is rejected before codegen and return the error text.
fn lower_err(src: &str) -> String {
    match compile(src, &AnalyzeOptions::default()) {
        Ok(_) => panic!("expected compile to fail loudly\nsource:\n{src}"),
        Err(e) => e.to_string(),
    }
}

/// Flatten the entry func's op tree (including `If` / `Block` / `Loop`
/// bodies, pre-order) into a linear stream for shape assertions.
fn entry_ops(module: &Module) -> Vec<Op> {
    fn walk(body: &[TaggedOp], out: &mut Vec<Op>) {
        for t in body {
            out.push(t.op.clone());
            match &t.op {
                Op::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    walk(then_body, out);
                    walk(else_body, out);
                }
                Op::Block { body, .. } => walk(body, out),
                Op::Loop { body, .. } => walk(body, out),
                _ => {}
            }
        }
    }
    let entry = &module.funcs[module.entry_func_index.expect("entry func index")];
    let mut acc = Vec::new();
    walk(&entry.body, &mut acc);
    acc
}

fn lower_ops(src: &str) -> Vec<Op> {
    entry_ops(&lower(src))
}

/// Op kind name (`Debug` head without payload) — lets small tests
/// assert an exact op sequence without spelling out every field.
fn kind(op: &Op) -> String {
    let dbg = format!("{op:?}");
    dbg.split(['{', '('])
        .next()
        .expect("split always yields one item")
        .trim()
        .to_string()
}

fn kinds(ops: &[Op]) -> Vec<String> {
    ops.iter().map(kind).collect()
}

fn count_kind(ops: &[Op], name: &str) -> usize {
    ops.iter().filter(|op| kind(op) == name).count()
}

/// All `Op::Call` target indices, in emission order.
fn call_indices(ops: &[Op]) -> Vec<u32> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Call { fn_index, .. } => Some(*fn_index),
            _ => None,
        })
        .collect()
}

fn stdlib_idx(name: &str) -> u32 {
    stdlib_function_index(name).unwrap_or_else(|| panic!("stdlib body `{name}` missing"))
}

// =====================================================================
// `try_lower_type_const` — static const-fold of the `type(v)` builtin.
// =====================================================================

/// `type(1)` folds to a constant `"Int"` string: the argument is
/// evaluated (into a discarded let-local) and the type name is a
/// compile-time `ConstString`. No runtime call survives.
#[test]
fn type_of_int_folds_to_const_string() {
    let ops = lower_ops("#main() -> String\ntype(1)");
    assert_eq!(
        kinds(&ops),
        ["ConstI64", "LetSet", "ConstString", "StoreField", "Return"],
        "expected the exact type-const fold shape"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::ConstString { value, .. } if value == "Int")),
        "folded type name must be \"Int\""
    );
}

/// `type(1.5)` folds to `"Float"` through the same shape.
#[test]
fn type_of_float_folds_to_const_string() {
    let ops = lower_ops("#main() -> String\ntype(1.5)");
    assert_eq!(
        kinds(&ops),
        ["ConstF64", "LetSet", "ConstString", "StoreField", "Return"],
    );
    assert!(ops
        .iter()
        .any(|op| matches!(op, Op::ConstString { value, .. } if value == "Float")));
}

/// The fold must KEEP the argument evaluation (trap / side-effect
/// ordering parity with the tree-walk oracle): `type(n + 1)` still
/// emits the `Add` before the folded name, then discards the value
/// into a never-read let-local.
#[test]
fn type_fold_keeps_argument_evaluation() {
    let ops = lower_ops("#main(Int n) -> String\ntype(n + 1)");
    let add_pos = ops
        .iter()
        .position(|op| matches!(op, Op::Add(IrType::I64)))
        .expect("argument Add must survive the fold");
    let const_pos = ops
        .iter()
        .position(|op| matches!(op, Op::ConstString { value, .. } if value == "Int"))
        .expect("folded \"Int\" ConstString");
    assert!(
        add_pos < const_pos,
        "argument must be evaluated before the folded constant"
    );
    assert!(
        matches!(ops[add_pos + 1], Op::LetSet { .. }),
        "evaluated argument must be discarded into a let-local"
    );
}

// =====================================================================
// `try_lower_scalar_math` — free-fn numeric builtins.
// =====================================================================

/// `floor(n)` on an `Int` operand folds to the identity — the operand
/// passes through untouched, with no stdlib call at all.
#[test]
fn floor_on_int_folds_to_identity() {
    let ops = lower_ops("#main(Int n) -> Int\nfloor(n)");
    assert_eq!(
        kinds(&ops),
        ["LoadField", "StoreField", "Return"],
        "Int floor must be a pure pass-through"
    );
}

/// `floor(x)` on a `Float` operand emits exactly one call to the
/// bundled `floor` body.
#[test]
fn floor_on_float_calls_floor_body() {
    let ops = lower_ops("#main(Float x) -> Float\nfloor(x)");
    assert_eq!(kinds(&ops), ["LoadField", "Call", "StoreField", "Return"]);
    assert_eq!(call_indices(&ops), [stdlib_idx("floor")]);
}

/// `abs(n)` on an `Int` routes to the bundled integer `abs` body.
#[test]
fn abs_on_int_calls_abs_body() {
    let ops = lower_ops("#main(Int n) -> Int\nabs(n)");
    assert_eq!(call_indices(&ops), [stdlib_idx("abs")]);
}

/// Negative: the METHOD form `x.floor()` must not be rewritten — the
/// tree-walk oracle only registers `floor` as a free fn, so the
/// compiled path has to cap loudly instead of silently accepting it.
#[test]
fn floor_method_form_caps_loudly() {
    let err = lower_err("#main(Float x) -> Float\nx.floor()");
    assert!(
        err.contains("unknown stdlib method `floor`"),
        "expected the loud method-form cap, got: {err}"
    );
}

// =====================================================================
// `try_lower_list_len` / `try_lower_list_count` — header-read shortcut.
// =====================================================================

/// `len([1, 2, 3])` reads the list record's `[len: u32]` header
/// directly (`ReadStringLen`) off the const-pool literal — no stdlib
/// length body is called.
#[test]
fn len_of_const_list_reads_header() {
    let ops = lower_ops("#main() -> Int\nlen([1, 2, 3])");
    assert_eq!(
        kinds(&ops),
        ["ConstListInt", "ReadStringLen", "StoreField", "Return"],
    );
}

/// `count(xs)` is `len` generalised over every list record shape —
/// same header-read rewrite on a `List<Int>` argument.
#[test]
fn count_of_const_list_reads_header() {
    let ops = lower_ops("#main() -> Int\ncount([1, 2, 3])");
    assert_eq!(
        kinds(&ops),
        ["ConstListInt", "ReadStringLen", "StoreField", "Return"],
    );
}

/// Negative: `len("abc")` produces a `String`, not a `ListInt`, so the
/// header-read rewrite must roll back and the generic dispatch emits
/// the bundled free-fn `len` body call (Unicode code-point count — a
/// raw byte-length header read would be a silent wrong value for
/// multi-byte strings).
#[test]
fn len_of_string_falls_back_to_len_body() {
    let ops = lower_ops("#main() -> Int\nlen(\"abc\")");
    assert_eq!(count_kind(&ops, "ReadStringLen"), 0);
    assert_eq!(call_indices(&ops), [stdlib_idx("len")]);
}

/// Negative: the METHOD form `[1, 2, 3].count()` must stay unrewritten
/// and cap loudly — the oracle registers `count` only as a free fn.
#[test]
fn count_method_form_caps_loudly() {
    let err = lower_err("#main() -> Int\n[1, 2, 3].count()");
    assert!(
        err.contains("unknown stdlib method `count`"),
        "expected the loud method-form cap, got: {err}"
    );
}

/// Negative: `count("abc")` — the oracle's `count` rejects non-lists,
/// so the rewrite rolls back on the `String` operand and the generic
/// path caps (there is no bundled free-fn `count` body to fall into).
#[test]
fn count_of_string_caps_loudly() {
    let err = lower_err("#main() -> Int\ncount(\"abc\")");
    assert!(
        err.contains("unknown stdlib method `count`"),
        "expected the loud cap, got: {err}"
    );
}

// =====================================================================
// `try_lower_list_sum_range` / `try_lower_list_sum_value` — sum fusion.
// =====================================================================

/// `list.sum(range(n))` fuses into a pure i64 accumulator loop: no
/// list is materialised and no stdlib body is called.
#[test]
fn sum_of_bare_range_fuses_to_accumulator_loop() {
    let ops = lower_ops("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))");
    assert_eq!(
        call_indices(&ops),
        Vec::<u32>::new(),
        "no stdlib call may survive"
    );
    assert_eq!(count_kind(&ops, "Loop"), 1, "exactly one fused loop");
    assert!(
        ops.iter().any(|op| matches!(op, Op::Ge(IrType::I64))),
        "loop bound test must be an i64 Ge"
    );
    assert!(
        ops.iter().any(|op| matches!(op, Op::Add(IrType::I64))),
        "accumulator Add must be inline"
    );
}

/// Adjacent shape: `list.sum([1, 2, 3])` is NOT the range fusion — the
/// already-materialised list goes through exactly one call to the
/// bundled `list_int_sum` body, with no loop in the entry func.
#[test]
fn sum_of_const_list_calls_sum_body() {
    let ops = lower_ops("#import list from \"std/list\"\n#main() -> Int\nlist.sum([1, 2, 3])");
    assert_eq!(
        kinds(&ops),
        ["ConstListInt", "Call", "StoreField", "Return"],
    );
    assert_eq!(call_indices(&ops), [stdlib_idx("list_int_sum")]);
}

/// `list.sum(range(n).map((i) => list.sum(range(i))))` — the nested
/// range-map-reduce recogniser inlines BOTH folds: two nested loops,
/// zero calls, zero materialised lists.
#[test]
fn nested_range_sum_fuses_to_nested_loops() {
    let ops = lower_ops(
        "#unstrict\n#import list from \"std/list\"\n#main(Int n) -> Int\n\
         list.sum(range(n).map((i) => list.sum(range(i))))",
    );
    assert_eq!(call_indices(&ops), Vec::<u32>::new());
    assert_eq!(count_kind(&ops, "Loop"), 2, "outer + inner fused loops");
    assert_eq!(
        count_kind(&ops, "MakeClosure"),
        0,
        "closure must be inlined"
    );
}

// =====================================================================
// `try_lower_len_filter_range` / range-chain `.len()` — count fusion.
// =====================================================================

/// `range(n).filter(p).len()` fuses into a scalar count loop — the
/// filter closure body is inlined as a `BrIf` short-circuit and no
/// list_int_* body is called.
#[test]
fn range_filter_len_fuses_to_count_loop() {
    let ops = lower_ops("#unstrict\n#main(Int n) -> Int\nrange(n).filter((i) => i > 1).len()");
    assert_eq!(call_indices(&ops), Vec::<u32>::new());
    assert_eq!(count_kind(&ops, "Loop"), 1);
    assert!(
        ops.iter().any(|op| matches!(op, Op::Gt(IrType::I64))),
        "inlined predicate comparison must survive"
    );
    assert!(
        count_kind(&ops, "BrIf") >= 2,
        "loop exit + filter short-circuit branches expected"
    );
}

// =====================================================================
// `try_lower_list_map` / `emit_list_hof_method` — HOF via bundled body.
// =====================================================================

/// `[1, 2, 3].map(f)` keeps the closure as a real closure: one
/// `MakeClosure`, then `list_int_map` and (from the wrapping sum)
/// `list_int_sum` bundled-body calls, in that order.
#[test]
fn list_map_method_calls_map_body_with_closure() {
    let ops = lower_ops(
        "#unstrict\n#import list from \"std/list\"\n#main() -> Int\n\
         list.sum([1, 2, 3].map((x) => x * 2))",
    );
    assert_eq!(
        kinds(&ops),
        [
            "ConstListInt",
            "MakeClosure",
            "Call",
            "Call",
            "StoreField",
            "Return"
        ],
    );
    assert_eq!(
        call_indices(&ops),
        [stdlib_idx("list_int_map"), stdlib_idx("list_int_sum")],
    );
}

// =====================================================================
// `try_lower_range_value` / `emit_range_materialize`.
// =====================================================================

/// A bare `range(n)` in value position materialises inline: guard
/// `If`, dynamic scratch alloc, one store loop — no stdlib call.
#[test]
fn bare_range_as_value_materializes_inline() {
    let ops = lower_ops("#main(Int n) -> List<Int>\nrange(n)");
    assert_eq!(call_indices(&ops), Vec::<u32>::new());
    assert_eq!(count_kind(&ops, "AllocScratchDyn"), 1);
    assert_eq!(count_kind(&ops, "Loop"), 1, "one element-store loop");
    assert_eq!(count_kind(&ops, "If"), 1, "empty-range guard");
}

// =====================================================================
// `flatten_list_spread` — static spread flattening.
// =====================================================================

/// `[1, ...[2, 3], 4]` statically flattens to the four-element
/// materialise: header count 4 (record size 8 + 8*4 = 40), then the
/// element constants 1, 2, 3, 4 stored in source order.
#[test]
fn static_spread_flattens_in_source_order() {
    let ops = lower_ops("#main() -> Int\nlen([1, ...[2, 3], 4])");
    assert_eq!(
        ops.first(),
        Some(&Op::ConstI32(40)),
        "record size must reflect the flattened count of 4"
    );
    let elems: Vec<i64> = ops
        .iter()
        .filter_map(|op| match op {
            Op::ConstI64(v) => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(elems, [1, 2, 3, 4], "flattened elements in source order");
    assert_eq!(count_kind(&ops, "StoreI64AtAbsolute"), 4);
    assert_eq!(call_indices(&ops), Vec::<u32>::new());
    assert_eq!(count_kind(&ops, "Loop"), 0, "straight-line materialise");
}

/// Negative: a spread whose source is NOT a list literal cannot be
/// statically flattened — it must route to the runtime spread
/// materialiser (header count computed from the source's length, one
/// `Memcpy` per runtime segment), never to the const-pool literal.
#[test]
fn runtime_spread_source_memcpys_segments() {
    let ops = lower_ops("#main(List<Int> xs) -> Int\nlen([1, ...xs, 4])");
    assert_eq!(
        count_kind(&ops, "ConstListInt"),
        0,
        "not a const-pool literal"
    );
    assert_eq!(
        count_kind(&ops, "MemcpyAtAbsolute"),
        1,
        "one runtime segment copy"
    );
    assert_eq!(
        count_kind(&ops, "StoreI64AtAbsolute"),
        2,
        "the two static scalar elements around the spread"
    );
    assert_eq!(call_indices(&ops), Vec::<u32>::new());
}

// =====================================================================
// Const-pool vs computed-element list literal routing.
// =====================================================================

/// An all-const `[1, 2, 3, 4]` literal is interned into the const pool
/// — a single `ConstListInt`, no per-element stores.
#[test]
fn all_const_list_literal_uses_const_pool() {
    let ops = lower_ops("#main() -> Int\nlen([1, 2, 3, 4])");
    assert_eq!(count_kind(&ops, "ConstListInt"), 1);
    assert_eq!(count_kind(&ops, "StoreI64AtAbsolute"), 0);
    assert_eq!(count_kind(&ops, "AllocScratchDyn"), 0);
}

/// Negative: one computed element (`[1, n, 3]`) defeats the const-pool
/// route — the literal materialises element-by-element, reading the
/// param for the middle slot.
#[test]
fn computed_element_defeats_const_pool() {
    let ops = lower_ops("#main(Int n) -> Int\nlen([1, n, 3])");
    assert_eq!(count_kind(&ops, "ConstListInt"), 0);
    assert_eq!(count_kind(&ops, "AllocScratchDyn"), 1);
    assert_eq!(count_kind(&ops, "StoreI64AtAbsolute"), 3);
    assert_eq!(
        count_kind(&ops, "LoadField"),
        1,
        "the computed element reads the `n` param"
    );
}
