//! Lowering sub-module: end-to-end `#[cfg(test)]` suites moved out of
//! `mod.rs` verbatim.
//!
//! Four suites live here — `intern_tests` (compile-time intern
//! invariants, also hosting the shared `test_helpers_lower_source`
//! pipeline driver), `str_concat_chain_tests`, `range_pipeline_tests`,
//! and `w7_closure_boundary_tests`. All drive the public parse →
//! analyze → lower pipeline; none reach into `LowerCtx` internals.

use super::*;

// =====================================================================
// #151 — Compile-time intern invariants.
//
// End-to-end checks that drive the analyzer + lowering pipeline so the
// invariants exercise the same code path real callers hit (rather
// than synthesising a `LowerCtx` directly, which would bypass the
// schema-method composition step where the latent idx-collision bug
// lived).
// =====================================================================

#[cfg(test)]
mod intern_tests {
    use super::*;

    fn type_node(name: &str, generics: Vec<TypeNode>) -> TypeNode {
        TypeNode {
            path: vec![name.to_string()],
            generics,
            is_optional: false,
            range: TokenRange::default(),
            variant_fields: None,
            doc_comment: None,
        }
    }

    #[test]
    fn tuple_type_canonicalizer_accepts_normal_nested_types() {
        let ty = type_node(
            "Tuple",
            vec![
                type_node("Int", vec![]),
                type_node(
                    "Tuple",
                    vec![type_node("String", vec![]), type_node("Bool", vec![])],
                ),
                type_node("List", vec![type_node("Int", vec![])]),
                type_node("Option", vec![type_node("String", vec![])]),
                type_node(
                    "Result",
                    vec![type_node("Int", vec![]), type_node("String", vec![])],
                ),
            ],
        );

        let TypeRepr::Schema { schema } = type_node_to_canonical(&ty).expect("tuple canonical")
        else {
            panic!("Tuple<...> should canonicalize as a tuple schema");
        };
        assert!(schema.is_tuple);
        assert_eq!(schema.fields.len(), 5);
        assert!(matches!(schema.fields[0].ty, TypeRepr::Int));
        assert!(matches!(&schema.fields[1].ty, TypeRepr::Schema { schema } if schema.is_tuple));
        assert!(
            matches!(&schema.fields[2].ty, TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int))
        );
        assert!(
            matches!(&schema.fields[3].ty, TypeRepr::Option { inner } if matches!(inner.as_ref(), TypeRepr::String))
        );
        assert!(
            matches!(&schema.fields[4].ty, TypeRepr::Result { ok, err } if matches!(ok.as_ref(), TypeRepr::Int) && matches!(err.as_ref(), TypeRepr::String))
        );
    }

    #[test]
    fn null_and_unit_type_names_do_not_canonicalize() {
        assert!(type_node_to_canonical(&type_node("Null", vec![])).is_none());
        assert!(type_node_to_canonical(&type_node("Unit", vec![])).is_none());
    }

    #[test]
    fn tuple_return_lowers_nested_tuple_and_list_elements() {
        let src = r#"
            #main(Int n) -> Tuple<Tuple<Int, String>, List<Int>, String>
            ((n, "x"), [n, n + 1], "done")
        "#;
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        assert!(
            !analyzed.has_errors(),
            "analyze errors: {:?}",
            analyzed.diagnostics
        );
        let lowered = lower_workspace_single(&analyzed, &ast).expect("lower nested tuple return");

        assert!(lowered.return_schema.is_tuple);
        assert_eq!(lowered.return_schema.fields.len(), 3);
        assert!(
            matches!(&lowered.return_schema.fields[0].ty, TypeRepr::Schema { schema } if schema.is_tuple)
        );
        assert!(
            matches!(&lowered.return_schema.fields[1].ty, TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int))
        );
        assert!(matches!(
            lowered.return_schema.fields[2].ty,
            TypeRepr::String
        ));
    }

    /// Recursively flatten a func body's op stream into `out`, descending
    /// into `If` / `Block` / `Loop` arms so assertions see ops wherever
    /// the control-flow places them.
    fn flatten_into(body: &[TaggedOp], out: &mut Vec<Op>) {
        for t in body {
            out.push(t.op.clone());
            match &t.op {
                Op::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    flatten_into(then_body, out);
                    flatten_into(else_body, out);
                }
                Op::Block { body, .. } | Op::Loop { body, .. } => flatten_into(body, out),
                _ => {}
            }
        }
    }

    /// AOT-4 (W16 slice): a 1D `xs[i]` index on a materialised
    /// `List<Int>` receiver lowers to the inline payload addressing —
    /// `(base + 11) & -8` then `+ i*8` then `Op::LoadI64AtAbsolute
    /// { offset: 0 }` — and NEVER to `Op::ListGetByIntIdx` (a
    /// trace-recorder-only op that static codegen rejects) nor to an
    /// eliding peephole that would collapse the index away. Pins the
    /// lowering shape so a regression that swaps the op surfaces here.
    #[test]
    fn list_int_index_emits_inline_payload_load() {
        // `arr: range(0, n)` materialises a `List<Int>`; `arr[1]` indexes
        // it. The `_len <= 1` guard keeps the bench-shape in-bounds; the
        // index lowering itself is what we inspect.
        let src = "#unstrict\n#main(Int n) -> Int\n\
                   (_len(arr) <= 1 ? 0 : arr[1]) where { arr: range(0, n) }";
        let m = lower_source(src);

        // Collect every op (recursing into If / Block / Loop arms) so the
        // assertions see the index ops wherever the ternary places them.
        fn collect(body: &[TaggedOp], out: &mut Vec<Op>) {
            for t in body {
                out.push(t.op.clone());
                match &t.op {
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        collect(then_body, out);
                        collect(else_body, out);
                    }
                    Op::Block { body, .. } | Op::Loop { body, .. } => collect(body, out),
                    _ => {}
                }
            }
        }
        let mut ops = Vec::new();
        for f in &m.funcs {
            collect(&f.body, &mut ops);
        }

        // The index must lower to an inline `LoadI64AtAbsolute { 0 }`.
        let has_inline_load = ops
            .iter()
            .any(|op| matches!(op, Op::LoadI64AtAbsolute { offset: 0 }));
        assert!(
            has_inline_load,
            "xs[i] index must emit an inline `LoadI64AtAbsolute {{ offset: 0 }}`; ops = {ops:?}"
        );

        // It must NOT lower to the trace-recorder-only index op.
        let has_trace_index = ops
            .iter()
            .any(|op| matches!(op, Op::ListGetByIntIdx { .. }));
        assert!(
            !has_trace_index,
            "xs[i] index must NOT emit `Op::ListGetByIntIdx` (trace-recorder-only; static codegen rejects it)"
        );

        // The payload-alignment math must be present: `& -8` (BitAnd I32)
        // plus the `* 8` element stride (Mul I32). A peephole that
        // collapsed the index would drop these.
        let has_align = ops.iter().any(|op| matches!(op, Op::BitAnd(IrType::I32)));
        let has_stride = ops.iter().any(|op| matches!(op, Op::Mul(IrType::I32)));
        assert!(
            has_align && has_stride,
            "xs[i] index must emit payload-align (`BitAnd I32`) + element-stride (`Mul I32`) math; \
             has_align={has_align} has_stride={has_stride}"
        );
    }

    /// AOT-4 (W19 slice): a where-bound `List<List<Int>>` materialises
    /// nested arena records and a 2D index `m[i][k]` composes TWO inline
    /// payload loads — NOT `Op::ListGetByIntIdx`, NOT an eliding peephole
    /// that would collapse the matrix. Pins the materialised 2D path
    /// distinct from the reduce-only fused nested-range shape (which
    /// allocates no list at all).
    #[test]
    fn matmul_2d_materializes_nested_records_and_double_indexes() {
        // `m` is a where-bound `List<List<Int>>` (outer map over an
        // inner map). `m[i][k]` is a cross-row double index — the kernel
        // shape the eliding peephole cannot serve. The `_len` guards keep
        // the read in-bounds without changing the lowering under test.
        let src = "#unstrict\n#main(Int n) -> Int\n\
                   (_len(m) <= 1 ? 0 : m[1][0]) \
                   where { m: range(n).map((i) => range(n).map((j) => i * 10 + j)) }";
        let m = lower_source(src);
        let mut ops = Vec::new();
        for f in &m.funcs {
            flatten_into(&f.body, &mut ops);
        }

        // A 2D materialise allocates the outer record PLUS one inner row
        // record per outer iteration — at least two distinct
        // `AllocScratchDyn` sites in the op stream (outer + inner-in-loop).
        let alloc_dyn = ops
            .iter()
            .filter(|op| matches!(op, Op::AllocScratchDyn))
            .count();
        assert!(
            alloc_dyn >= 2,
            "2D materialise must emit >=2 AllocScratchDyn (outer record + inner rows), got {alloc_dyn}"
        );

        // The double index composes inline payload loads.
        let inline_loads = ops
            .iter()
            .filter(|op| matches!(op, Op::LoadI64AtAbsolute { offset: 0 }))
            .count();
        assert!(
            inline_loads >= 2,
            "2D index m[i][k] must emit >=2 inline LoadI64AtAbsolute (outer handle + inner cell), got {inline_loads}"
        );

        // Never the trace-recorder-only index op.
        assert!(
            !ops.iter()
                .any(|op| matches!(op, Op::ListGetByIntIdx { .. })),
            "2D index must NOT emit Op::ListGetByIntIdx (trace-only; static codegen rejects it)"
        );

        // The materialise path fills the payload with StoreI64AtAbsolute
        // — an eliding peephole would collapse the matrix to a scalar
        // loop and leave none.
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::StoreI64AtAbsolute { .. })),
            "2D materialise must fill payloads with StoreI64AtAbsolute (no eliding collapse)"
        );

        // Payload-align (`BitAnd I32`) + element-stride (`Mul I32`) math
        // is present (both materialise + index emit it).
        assert!(
            ops.iter().any(|op| matches!(op, Op::BitAnd(IrType::I32)))
                && ops.iter().any(|op| matches!(op, Op::Mul(IrType::I32))),
            "2D path must emit payload-align + element-stride math"
        );
    }

    /// AOT-4 (W19 slice): the production `c.reduce(0, (row_acc, row) =>
    /// row_acc + row.reduce(...))` over a materialised `List<List<Int>>`
    /// lowers through the reduce-over-materialised-list path — it reads
    /// the record headers (`LoadI32AtAbsolute`) for the loop bounds and
    /// the elements (`LoadI64AtAbsolute`) inline, NOT through
    /// `Op::ListGetByIntIdx` or a `list_int_fold` stdlib `Op::Call`.
    #[test]
    fn matmul_reduce_over_materialized_list_lowers_inline() {
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))\n\
                   where {\n\
                     size: n,\n\
                     c: range(size).map((i) => range(size).map((j) => i + j))\n\
                   }";
        let m = lower_source(src);
        let mut ops = Vec::new();
        for f in &m.funcs {
            flatten_into(&f.body, &mut ops);
        }

        // The reduce loops read the `[len]` header with LoadI32AtAbsolute.
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::LoadI32AtAbsolute { offset: 0 })),
            "reduce-over-list must read the record header via LoadI32AtAbsolute"
        );
        // And the elements with inline LoadI64AtAbsolute.
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::LoadI64AtAbsolute { offset: 0 })),
            "reduce-over-list must read elements via inline LoadI64AtAbsolute"
        );
        // Never via the trace-only index op.
        assert!(
            !ops.iter()
                .any(|op| matches!(op, Op::ListGetByIntIdx { .. })),
            "reduce-over-list must NOT emit Op::ListGetByIntIdx"
        );
        // The fold must NOT route through the `list_int_fold` stdlib body
        // (that would require a closure conversion the inline reduce
        // avoids). Pins the inline reduce-loop path.
        if let Some(fold_idx) = stdlib_function_index("list_int_fold") {
            assert!(
                !ops.iter()
                    .any(|op| matches!(op, Op::Call { fn_index, .. } if *fn_index == fold_idx)),
                "reduce-over-list must lower inline, not via Op::Call(list_int_fold) (fold_idx={fold_idx})"
            );
        }
    }

    /// AOT-4 (W16 slice): the recursive `sum_qs(xs)` helper's param is
    /// inferred as `List<Int>` (not the I64 default) from the body using
    /// `xs` as a list (`xs[0]`, `_len(xs)`, `_list_filter(xs, ...)`), so
    /// the recursive list arg type-checks. Pins the inference.
    #[test]
    fn recursive_list_helper_param_inferred_list_int() {
        let src = "#unstrict\n#main(Int n) -> Int\n\
                   sum_lt(arr) where { arr: range(0, n), \
                   sum_lt(xs): _len(xs) == 0 ? 0 : (xs[0] + sum_lt(_list_filter(xs, (x) => x > xs[0]))) }";
        let m = lower_source(src);
        // The lifted recursive helper (a lambda) takes `(captures_ptr:
        // I32, xs: ListInt)`. Find a lambda whose user param is ListInt.
        let has_list_param = m.funcs.iter().any(|f| {
            f.params.len() == 2 && f.params[0] == IrType::I32 && f.params[1] == IrType::ListInt
        });
        assert!(
            has_list_param,
            "recursive `sum_lt(xs)` helper must take a `List<Int>` param; funcs = {:?}",
            m.funcs
                .iter()
                .map(|f| (&f.name, &f.params))
                .collect::<Vec<_>>()
        );
    }

    /// Walk `funcs` and collect every `Op::ConstString { idx, value }`
    /// across each func's body (and into any nested `If` / `Block` /
    /// `Loop` arms). Used by the invariant tests below to project the
    /// flat `(idx, value)` ground truth out of the lowered module.
    fn collect_const_strings(funcs: &[Func]) -> Vec<(u32, String)> {
        fn walk(body: &[TaggedOp], out: &mut Vec<(u32, String)>) {
            for t in body {
                match &t.op {
                    Op::ConstString { idx, value } => out.push((*idx, value.clone())),
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
        let mut acc = Vec::new();
        for f in funcs {
            walk(&f.body, &mut acc);
        }
        acc
    }

    fn lower_source(src: &str) -> Module {
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        assert!(
            !analyzed.has_errors(),
            "analyze errors: {:?}",
            analyzed.diagnostics
        );
        let lowered = lower_workspace_single(&analyzed, &ast).expect("lower");
        lowered.module
    }

    /// Re-export `lower_source` under a stable name so sibling test
    /// modules can drive the same parse + analyze + lower pipeline
    /// without duplicating the boilerplate.
    pub(super) fn test_helpers_lower_source(src: &str) -> Module {
        lower_source(src)
    }

    /// Same-bytes string literals inside one function dedup to a
    /// single idx. Pre-#151 the per-`LowerCtx` counter minted a
    /// fresh idx for each occurrence and the const-pool laid out
    /// three identical `[len][bytes]` records.
    #[test]
    fn intern_dedups_same_literal_in_one_func() {
        // Two `"foo"` literals inside one entry body. Both lower to
        // `Op::ConstString { value: "foo" }` through the same
        // `LowerCtx`.
        let src = "#main() -> String\n\"foo\".concat(\"foo\")";
        let module = lower_source(src);
        let consts = collect_const_strings(&module.funcs);
        let foo_idxs: Vec<u32> = consts
            .iter()
            .filter(|(_, v)| v == "foo")
            .map(|(idx, _)| *idx)
            .collect();
        assert!(
            foo_idxs.len() >= 2,
            "expected at least two `foo` Op::ConstString emissions, got {foo_idxs:?}"
        );
        // Intern contract: every occurrence resolves to the same idx.
        assert!(
            foo_idxs.iter().all(|i| *i == foo_idxs[0]),
            "intern violated — `foo` literals mapped to {foo_idxs:?}"
        );
    }

    /// Distinct literals get distinct idxs (sanity — guards against
    /// a regression that always returns 0).
    #[test]
    fn intern_keeps_distinct_literals_distinct() {
        let src = "#main() -> String\n\"foo\".concat(\"bar\")";
        let module = lower_source(src);
        let consts = collect_const_strings(&module.funcs);
        let foo = consts.iter().find(|(_, v)| v == "foo").map(|(i, _)| *i);
        let bar = consts.iter().find(|(_, v)| v == "bar").map(|(i, _)| *i);
        assert!(foo.is_some(), "missing foo, got {consts:?}");
        assert!(bar.is_some(), "missing bar, got {consts:?}");
        assert_ne!(
            foo, bar,
            "intern collapsed two distinct literals to the same idx"
        );
    }

    /// Module-wide idx-uniqueness across schema-method bodies + the
    /// entry body. Before #151 each func reset `next_string_idx` to
    /// 0, so a method emitting `Op::ConstString { idx: 0, "a" }` and
    /// the entry emitting `Op::ConstString { idx: 0, "b" }` produced
    /// idx collisions the const-pool silently misresolved. The
    /// invariant: every distinct (idx) maps to a single value across
    /// the whole module.
    #[test]
    fn module_wide_idx_uniqueness_across_methods_and_entry() {
        // Schema with a method that returns a string-derived bool
        // (touches a literal), plus an entry body that touches a
        // different literal. The shared intern handle threads through
        // `lower_schema_methods` so both funcs draw idxs from the
        // same allocator.
        let src = "#schema P { String name: * } with {\n\
                     starts_a() -> Bool: self.name.starts_with(\"a\")\n\
                   }\n\
                   #main(P p) -> Bool\n\
                   p.starts_a() ? true : p.name.starts_with(\"b\")";
        let module = lower_source(src);
        let consts = collect_const_strings(&module.funcs);
        // Each idx maps to at most one (value) — collision-free.
        let mut by_idx: HashMap<u32, &String> = HashMap::new();
        for (idx, value) in &consts {
            if let Some(prev) = by_idx.insert(*idx, value) {
                assert_eq!(
                    prev, value,
                    "idx {idx} bound to two values: `{prev}` and `{value}` (module-wide \
                     uniqueness violation)"
                );
            }
        }
        // And we got at least both literals.
        let values: Vec<&String> = consts.iter().map(|(_, v)| v).collect();
        assert!(
            values.iter().any(|v| v.as_str() == "a"),
            "missing `a`, got {values:?}"
        );
        assert!(
            values.iter().any(|v| v.as_str() == "b"),
            "missing `b`, got {values:?}"
        );
    }
}

// =====================================================================
// #165 — `Op::StrConcatN` chain-fold invariants.
//
// End-to-end checks that drive the analyzer + lowering pipeline so the
// fold gate observes the same AST shapes real callers hit. The
// invariants verify both the happy path (a 3+ leaf String chain
// collapses to one `StrConcatN`) and the rejection paths (Dict /
// Schema merge chains and two-operand pair-wise concat keep their
// existing shape).
// =====================================================================

#[cfg(test)]
mod str_concat_chain_tests {
    use super::*;

    /// Walk `funcs` flattening every IR op into a single Vec for
    /// shape-pattern assertions. Recurses into `If` / `Block` / `Loop`
    /// arms so a chain inside a branch still surfaces.
    fn flatten_ops(funcs: &[Func]) -> Vec<Op> {
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
        let mut acc = Vec::new();
        for f in funcs {
            walk(&f.body, &mut acc);
        }
        acc
    }

    /// Four-leaf left-leaning chain `"a" + "b" + "c" + "d"` folds to
    /// one `Op::StrConcatN { operand_count: 4 }` and emits zero
    /// `Op::Add(IrType::String)` in the same function.
    #[test]
    fn four_way_string_chain_folds_to_str_concat_n() {
        let src = "#main() -> String\n\"a\" + \"b\" + \"c\" + \"d\"";
        let module = super::intern_tests::test_helpers_lower_source(src);
        let ops = flatten_ops(&module.funcs);
        let concat_n_args: Vec<u32> = ops
            .iter()
            .filter_map(|op| match op {
                Op::StrConcatN { operand_count } => Some(*operand_count),
                _ => None,
            })
            .collect();
        assert_eq!(concat_n_args, vec![4], "expected one StrConcatN{{4}}");
        // Pair-wise `Op::Add(IrType::String)` must be elided — every
        // String add was absorbed into the chain fold.
        let leftover_str_adds = ops
            .iter()
            .filter(|op| matches!(op, Op::Add(IrType::String)))
            .count();
        assert_eq!(
            leftover_str_adds, 0,
            "fold left behind {leftover_str_adds} pair-wise Op::Add(String) ops"
        );
    }

    /// Three-leaf chain also fires — the minimal shape the fold gate
    /// requires (LHS itself is an Add).
    #[test]
    fn three_way_string_chain_folds_to_str_concat_n() {
        let src = "#main() -> String\n\"a\" + \"b\" + \"c\"";
        let module = super::intern_tests::test_helpers_lower_source(src);
        let ops = flatten_ops(&module.funcs);
        let concat_n_count = ops
            .iter()
            .filter(|op| matches!(op, Op::StrConcatN { operand_count: 3 }))
            .count();
        assert_eq!(concat_n_count, 1, "expected one StrConcatN{{3}}");
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Add(IrType::String)))
                .count(),
            0,
        );
    }

    /// Two-leaf concat keeps the existing `Op::Add(IrType::String)`
    /// shape — the fold gate requires `lhs` to be a Binary(Add), which
    /// a single `"a" + "b"` does not satisfy. Backends that don't yet
    /// support the pair-wise variant still bail to the tree-walker via
    /// the existing fallback envelope.
    #[test]
    fn two_way_string_concat_stays_on_add_string() {
        let src = "#main() -> String\n\"a\" + \"b\"";
        let module = super::intern_tests::test_helpers_lower_source(src);
        let ops = flatten_ops(&module.funcs);
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::StrConcatN { .. }))
                .count(),
            0,
            "two-leaf concat should not fold to StrConcatN"
        );
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Add(IrType::String)))
                .count(),
            1,
            "expected one Op::Add(IrType::String) for the pair-wise concat"
        );
    }
}

// =====================================================================
// Open follow-up #2 — `list.sum(range(...).map(...))` peephole.
//
// Verifies that the extended `try_lower_list_sum_range` recognises the
// `range(...).map((p) => body)` chain and emits a pure-i64 accumulator
// loop with no list allocation. Native benchmark rows rely on this shape
// because the scalar envelope rejects any IR that materialises a `List<Int>`.
// =====================================================================

#[cfg(test)]
mod range_pipeline_tests {
    use super::*;

    /// Drives the same parse + analyze + lower pipeline `intern_tests`
    /// uses, then returns the lowered entry func's flat op stream so
    /// shape assertions stay focussed on the post-desugar IR.
    fn lower_and_flatten(src: &str) -> Vec<Op> {
        let module = intern_tests::test_helpers_lower_source(src);
        let entry_idx = module.entry_func_index.expect("entry");
        let entry = &module.funcs[entry_idx];
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
        let mut acc = Vec::new();
        walk(&entry.body, &mut acc);
        acc
    }

    /// `list.sum(range(n).map((i) => i + 1))` desugars to a pure i64
    /// accumulator loop. No `Op::Call` targeting `list_int_map` or
    /// `list_int_sum` should remain — both would force the bytecode
    /// scalar envelope to bail.
    #[test]
    fn map_sum_chain_desugars_to_pure_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).map((i) => i + 1))";
        let ops = lower_and_flatten(src);
        // No buffer-protocol stdlib indirection.
        let stdlib_list_int_map = stdlib_function_index("list_int_map").unwrap();
        let stdlib_list_int_sum = stdlib_function_index("list_int_sum").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(
                    *fn_index, stdlib_list_int_map,
                    "expected `list_int_map` to be inlined by the peephole"
                );
                assert_ne!(
                    *fn_index, stdlib_list_int_sum,
                    "expected `list_int_sum` to be inlined by the peephole"
                );
            }
        }
        // Block shape: one outer loop-exit block + one inner
        // next-iter block (the latter exists so future `.filter`
        // stages have a short-circuit target). The pipeline emits
        // both unconditionally so the same loop body shape works
        // across all consumer / stage combinations.
        let blocks = ops
            .iter()
            .filter(|op| matches!(op, Op::Block { .. }))
            .count();
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(blocks, 2, "expected outer + inner Block, got {blocks}");
        assert_eq!(loops, 1, "expected one inner Loop, got {loops}");
    }

    /// Chained `.map(...).map(...)` collapses into the same accumulator
    /// loop shape — pipelining stages stay zero-alloc.
    #[test]
    fn chained_map_desugars_to_single_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).map((i) => i + 1).map((j) => j * 2))";
        let ops = lower_and_flatten(src);
        let stdlib_list_int_map = stdlib_function_index("list_int_map").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(*fn_index, stdlib_list_int_map);
            }
        }
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(loops, 1, "expected exactly one fused loop, got {loops}");
    }

    /// Sanity guard: the 0-stage form (`list.sum(range(n))`) still
    /// emits the original loop shape. Regression cover for the
    /// peephole refactor that introduced the chain recogniser.
    #[test]
    fn bare_range_sum_still_desugars() {
        let src = "#import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n))";
        let ops = lower_and_flatten(src);
        let stdlib_list_int_sum = stdlib_function_index("list_int_sum").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(*fn_index, stdlib_list_int_sum);
            }
        }
    }

    /// W4-shape: `range(n).map(c1).filter(c2).len()` desugars to a
    /// pure scalar count accumulator. The buffer-protocol stdlib
    /// `list_int_length` / `list_string_length` / `list_int_filter`
    /// must not show up — every one of them would force the bytecode
    /// scalar envelope to bail.
    #[test]
    fn map_filter_len_chain_desugars_to_count_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n)\n\
                     .map((i) => \"axb\")\n\
                     .filter((s) => s.contains(\"x\"))\n\
                     .len()";
        let ops = lower_and_flatten(src);
        let banned = [
            "list_int_length",
            "list_string_length",
            "list_int_filter",
            "list_int_map",
        ];
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                for name in banned.iter() {
                    if let Some(idx) = stdlib_function_index(name) {
                        assert_ne!(
                            *fn_index, idx,
                            "expected `{name}` to be inlined by the peephole"
                        );
                    }
                }
            }
        }
        // Exactly one Loop (the outer counter), two Block ops
        // (the loop-exit + the inner next-iter block where the
        // filter short-circuits).
        let blocks = ops
            .iter()
            .filter(|op| matches!(op, Op::Block { .. }))
            .count();
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(blocks, 2, "expected outer + inner Block, got {blocks}");
        assert_eq!(loops, 1, "expected one Loop, got {loops}");
    }

    /// `range(n).filter(c).sum()` shape uses the same emitter on the
    /// `SumI64` consumer side. The W-sf shape isn't in cmp_lua but
    /// exercises the filter -> sum path independent of the W4 chain.
    #[test]
    fn filter_sum_chain_uses_pipeline_emitter() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).filter((i) => i % 2 == 0))";
        let ops = lower_and_flatten(src);
        let stdlib_list_int_filter = stdlib_function_index("list_int_filter").unwrap();
        let stdlib_list_int_sum = stdlib_function_index("list_int_sum").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(*fn_index, stdlib_list_int_filter);
                assert_ne!(*fn_index, stdlib_list_int_sum);
            }
        }
    }

    /// AOT-2 — the W19 matmul cell-reduction shape lowers to a
    /// doubly-nested integer accumulator loop with NO list
    /// materialised. None of the buffer-protocol list-builder stdlib
    /// bodies (`list_int_map` / `list_int_sum` / `list_int_fold`) may
    /// survive — every one would force the bytecode scalar envelope to
    /// bail and would keep the shape off the LLVM AOT tier.
    #[test]
    fn nested_range_map_reduce_desugars_to_double_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n).map((i) => range(n).map((j) => (i * n + j) % 100))\n\
                     .reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))";
        let ops = lower_and_flatten(src);
        let banned = ["list_int_map", "list_int_sum", "list_int_fold"];
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                for name in banned.iter() {
                    if let Some(idx) = stdlib_function_index(name) {
                        assert_ne!(
                            *fn_index, idx,
                            "expected `{name}` to be inlined by the nested peephole"
                        );
                    }
                }
            }
        }
        // Two nested loops (outer `i`, inner `j`) and two Block wrappers
        // (one loop-exit guard per loop). No third loop / list builder.
        let blocks = ops
            .iter()
            .filter(|op| matches!(op, Op::Block { .. }))
            .count();
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(loops, 2, "expected two nested Loops, got {loops}");
        assert_eq!(blocks, 2, "expected one Block per loop, got {blocks}");
    }

    /// The `list.sum(row)` inner-fold form lowers to the same
    /// doubly-nested loop with no list materialised.
    #[test]
    fn nested_range_map_list_sum_desugars_to_double_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n).map((i) => range(n).map((j) => (i + j) % 100))\n\
                     .reduce(0, (acc, row) => acc + list.sum(row))";
        let ops = lower_and_flatten(src);
        let banned = ["list_int_map", "list_int_sum", "list_int_fold"];
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                for name in banned.iter() {
                    if let Some(idx) = stdlib_function_index(name) {
                        assert_ne!(*fn_index, idx);
                    }
                }
            }
        }
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(loops, 2, "expected two nested Loops, got {loops}");
    }

    /// CODEGEN-QUALITY (W18 slice): `_len(_list_filter(range(2, n),
    /// (k) => ...))` — where the filtered list is dead (only `_len`
    /// consumes it) — FUSES to a pure i64 counting loop that never
    /// materialises the filtered list. The fused shape emits NO
    /// `list_int_filter` `Op::Call` and NO `AllocScratchDyn` for the
    /// filter output; instead the predicate is inlined under an
    /// `Op::Loop` and a counter is incremented per survivor.
    ///
    /// This is dead-list-elimination / stream fusion — the count is
    /// identical to the materialise-then-`_len` path (same predicate,
    /// same range), only the intermediate `List<Int>` is elided. It is
    /// NOT an algorithm substitution.
    #[test]
    fn len_filter_range_fuses_to_counting_loop_no_materialize() {
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   _len(_list_filter(range(2, n), (k) => k % 2 == 0))";
        let ops = lower_and_flatten(src);

        // No `list_int_filter` `Op::Call` survives — the filter is
        // fused into the counter loop, not dispatched to the bundled
        // stdlib body.
        let filter_idx = stdlib_function_index("list_int_filter").unwrap();
        let filter_calls = ops
            .iter()
            .filter(|op| matches!(op, Op::Call { fn_index, .. } if *fn_index == filter_idx))
            .count();
        assert_eq!(
            filter_calls, 0,
            "fused shape must NOT call list_int_filter, got {filter_calls} calls"
        );

        // No filter-output `AllocScratchDyn` — nothing is materialised.
        // (The eliding range counter loop allocates no scratch record.)
        let alloc_dyn = ops
            .iter()
            .filter(|op| matches!(op, Op::AllocScratchDyn))
            .count();
        assert_eq!(
            alloc_dyn, 0,
            "fused shape must NOT materialise any List<Int> (got {alloc_dyn} AllocScratchDyn)"
        );

        // No per-element arena store fills a materialised payload.
        let store_i64 = ops
            .iter()
            .filter(|op| matches!(op, Op::StoreI64AtAbsolute { .. }))
            .count();
        assert_eq!(
            store_i64, 0,
            "fused shape must NOT store list elements to an arena (got {store_i64})"
        );

        // No `ReadStringLen` survivor-record read — the count comes
        // straight from the loop accumulator.
        let read_len = ops
            .iter()
            .filter(|op| matches!(op, Op::ReadStringLen))
            .count();
        assert_eq!(
            read_len, 0,
            "fused shape reads no length prefix; the counter is the result (got {read_len})"
        );

        // The fusion emits a counting `Op::Loop` with an i64 increment
        // (`Op::Add(I64)` of the accumulator) under the predicate.
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert!(loops >= 1, "expected a counting Op::Loop, got {loops}");
        assert!(
            ops.iter().any(|op| matches!(op, Op::Add(IrType::I64))),
            "expected an i64 counter increment in the fused loop"
        );
    }

    /// CODEGEN-QUALITY: the full W18 production shape — a `where`-bound
    /// recursive `is_prime` helper called from the filter predicate —
    /// also fuses to the counting loop (no `list_int_filter` call, no
    /// materialised list). The predicate body, including the recursive
    /// `is_prime(k, 2)` call, is inlined under the loop.
    #[test]
    fn w18_prime_count_shape_fuses_no_filter_call() {
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
                   where {\n\
                     is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
                   }";
        let ops = lower_and_flatten(src);
        let filter_idx = stdlib_function_index("list_int_filter").unwrap();
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Call { fn_index, .. } if *fn_index == filter_idx))
                .count(),
            0,
            "W18 fused shape must NOT route through list_int_filter"
        );
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::AllocScratchDyn))
                .count(),
            0,
            "W18 fused shape must NOT materialise the filtered list"
        );
        // The counting loop is present.
        assert!(
            ops.iter().any(|op| matches!(op, Op::Loop { .. })),
            "expected the fused counting Op::Loop"
        );
    }

    /// #359 (W20): the softened n-body kernel lowers with a `List<Float>`
    /// accumulator. Pins the envelope additions at the IR level: the
    /// list-literal materialiser (`AllocScratchDyn` + `StoreF64AtAbsolute`
    /// element stores), the `List<Float>` 1D index (`LoadF64AtAbsolute`),
    /// the list-valued reduce carry (the accumulator let rides
    /// `ListFloat`), and the closures lifted to `MakeClosure` (no leftover
    /// stdlib indirection). The exact numeric parity is pinned separately
    /// by the LLVM oracle test `llvm_w20_n_body.rs`.
    #[test]
    fn w20_n_body_lowers_with_list_float_reduce_accumulator() {
        let src = "#unstrict\n\
             #main(Int n) -> Float\n\
             final_state[0] * 1.0 + final_state[1] * 2.0 + final_state[2] * 3.0 + final_state[3] * 4.0\n\
               + final_state[4] * 5.0 + final_state[5] * 6.0 + final_state[6] * 7.0 + final_state[7] * 8.0\n\
             where {\n\
               dt: 0.01,\n\
               soft: 0.1,\n\
               m0: 1.0, m1: 2.0, m2: 0.5, m3: 3.0,\n\
               init: [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2],\n\
               pair_force(s, i, j, mj):\n\
                 i == j ? 0.0 :\n\
                   (s[j] - s[i]) * mj * (1.0 / (((s[j] - s[i]) * (s[j] - s[i]) + soft) * ((s[j] - s[i]) * (s[j] - s[i]) + soft))),\n\
               accel(s, i): pair_force(s, i, 0, m0) + pair_force(s, i, 1, m1) + pair_force(s, i, 2, m2) + pair_force(s, i, 3, m3),\n\
               step(s): [\n\
                 s[0] + s[4] * dt,\n\
                 s[1] + s[5] * dt,\n\
                 s[2] + s[6] * dt,\n\
                 s[3] + s[7] * dt,\n\
                 s[4] + accel(s, 0) * dt,\n\
                 s[5] + accel(s, 1) * dt,\n\
                 s[6] + accel(s, 2) * dt,\n\
                 s[7] + accel(s, 3) * dt\n\
               ],\n\
               final_state: range(n).reduce(init, (s, _step) => step(s))\n\
             }";
        let module = intern_tests::test_helpers_lower_source(src);
        let entry = &module.funcs[module.entry_func_index.expect("entry")];

        // The `init` literal materialises into a scratch arena: an
        // `AllocScratchDyn` + 8 `StoreF64AtAbsolute` element stores
        // appear in the entry body (the `step` body's literal stores
        // live in the lambda func, not the entry).
        let entry_f64_stores = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::StoreF64AtAbsolute { .. }))
            .count();
        assert_eq!(
            entry_f64_stores, 8,
            "expected the `init` 8-element List<Float> literal to emit 8 f64 stores, \
             got {entry_f64_stores}"
        );

        // The reduce body carries the `List<Float>` accumulator: a
        // `LetSet { ty: ListFloat }` appears (the accumulator slot).
        let has_listfloat_let = {
            fn walk(body: &[TaggedOp]) -> bool {
                body.iter().any(|t| match &t.op {
                    Op::LetSet {
                        ty: IrType::ListFloat,
                        ..
                    } => true,
                    Op::Block { body, .. } | Op::Loop { body, .. } => walk(body),
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => walk(then_body) || walk(else_body),
                    _ => false,
                })
            }
            walk(&entry.body)
        };
        assert!(
            has_listfloat_let,
            "expected a ListFloat-typed let (the reduce accumulator carry)"
        );

        // `final_state[k]` indexes the List<Float> -> `LoadF64AtAbsolute`.
        let has_f64_load = {
            fn walk(body: &[TaggedOp]) -> bool {
                body.iter().any(|t| match &t.op {
                    Op::LoadF64AtAbsolute { .. } => true,
                    Op::Block { body, .. } | Op::Loop { body, .. } => walk(body),
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => walk(then_body) || walk(else_body),
                    _ => false,
                })
            }
            walk(&entry.body)
        };
        assert!(
            has_f64_load,
            "expected a LoadF64AtAbsolute for the `final_state[k]` index reads"
        );

        // The where-bound closures `pair_force` / `accel` / `step` lift
        // to lambdas in the closure table (3 entries).
        assert_eq!(
            module.closure_table.len(),
            3,
            "expected pair_force + accel + step lambdas, got {}",
            module.closure_table.len()
        );

        // `step` returns a `List<Float>` handle: its lambda func's
        // declared return type is ListFloat.
        let step_lambda = module
            .funcs
            .iter()
            .find(|f| f.ret == IrType::ListFloat)
            .expect("expected a lambda returning ListFloat (the `step` closure)");
        assert!(
            step_lambda
                .body
                .iter()
                .filter(|t| matches!(t.op, Op::StoreF64AtAbsolute { .. }))
                .count()
                == 8,
            "expected `step`'s body to materialise an 8-element List<Float> via 8 f64 stores"
        );
    }

    /// Symmetric to the W20 Float lowering shape: a COMPUTED `List<Int>`
    /// literal `[n, n+1, n*2, n%3+7]` (each element a non-literal Int
    /// expression over the `#main` arg) must materialise through
    /// `emit_list_int_literal_materialize` — an `AllocScratchDyn` + an
    /// i32 length header (`StoreI32AtAbsolute`) + one
    /// `StoreI64AtAbsolute` PER ELEMENT — and MUST NOT intern as a
    /// `ConstListInt` (which the LLVM AOT envelope cannot materialise).
    /// The where-bound list is passed to a closure so the analyzer's
    /// tuple-index inference doesn't reject it; the exact numeric parity
    /// against the tree-walker is pinned by the LLVM oracle test
    /// `llvm_computed_int_list.rs`.
    #[test]
    fn computed_int_list_literal_lowers_via_scratch_materialize() {
        let src = "#unstrict\n\
             #main(Int n) -> Int\n\
             f(xs) where {\n\
               xs: [n, n + 1, n * 2, n % 3 + 7],\n\
               f(ys): ys[0] + ys[1] + ys[3]\n\
             }";
        let module = intern_tests::test_helpers_lower_source(src);
        let entry = &module.funcs[module.entry_func_index.expect("entry")];

        // The computed `xs` literal materialises in the entry body: at
        // least one `AllocScratchDyn` (the 4-element record) and exactly
        // 4 `StoreI64AtAbsolute` element stores (one per element).
        let alloc_dyn = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::AllocScratchDyn))
            .count();
        assert!(
            alloc_dyn >= 1,
            "expected the computed List<Int> literal to emit an AllocScratchDyn record, \
             got {alloc_dyn}"
        );
        let i64_stores = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::StoreI64AtAbsolute { .. }))
            .count();
        assert_eq!(
            i64_stores, 4,
            "expected the 4-element computed List<Int> literal to emit 4 i64 element stores, \
             got {i64_stores}"
        );
        // The i32 length header is stored.
        let i32_stores = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::StoreI32AtAbsolute { .. }))
            .count();
        assert!(
            i32_stores >= 1,
            "expected an i32 length header store for the materialised record, got {i32_stores}"
        );

        // The whole module must NOT contain a `ConstListInt` for this
        // computed literal — that would mean the const-intern path swallowed
        // it (the AOT envelope cannot materialise an interned const list).
        let has_const_list_int = module.funcs.iter().any(|f| {
            f.body
                .iter()
                .any(|t| matches!(t.op, Op::ConstListInt { .. }))
        });
        assert!(
            !has_const_list_int,
            "computed List<Int> literal must materialise, not intern as ConstListInt"
        );

        // The materialised handle is tagged ListInt: a `LetSet { ty:
        // ListInt }` appears (the where-binding slot for `xs`).
        let has_listint_let = entry.body.iter().any(|t| {
            matches!(
                &t.op,
                Op::LetSet {
                    ty: IrType::ListInt,
                    ..
                }
            )
        });
        assert!(
            has_listint_let,
            "expected a ListInt-typed let (the `xs` where-binding carry)"
        );
    }
}

// =====================================================================
// Phase F.2 — first-class closure value boundary.
//
// The W7 cmp_lua workload (`#main(Int n) -> Dict { #internal fib: (Int k) -> Int =>
// ..., result: fib(n) }`) currently fails `lower_workspace_single` at
// the return-type build step because `-> Dict` has no canonical
// representation. The downstream `Expr::Closure` at a non-higher-order
// site would also reject (see `lower_expr`'s explicit
// `ClosureAcrossBoundary` arm), so even after Phase A's return-type
// work the body would still bail.
//
// These tests pin the *current* diagnostic shape so the Phase B lifting
// surfaces as a test failure (the assertions flip from `Err(...)` to
// `Ok(...)`), giving the future implementer a clean checklist of which
// rejection sites have been lifted. The design doc
// `docs/internal/w7-closure-as-value-design.md` (local-only) captures
// the full plan.
// =====================================================================

#[cfg(test)]
mod w7_closure_boundary_tests {
    use super::*;

    /// Drive parse + analyze + `lower_workspace_single` without the
    /// `.expect("lower")` the `intern_tests::lower_source` helper does
    /// — Phase F.2 needs to observe the failure shape, not panic on it.
    fn try_lower(src: &str) -> Result<Module, LoweringError> {
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        // We intentionally don't `assert!(!analyzed.has_errors())`: the
        // analyzer may surface a soft warning for the closure-typed
        // dict field, but lowering still gets to run. Phase A only
        // cares about the IR-side diagnostic.
        lower_workspace_single(&analyzed, &ast).map(|l| l.module)
    }

    /// Phase C verification: the W7 production source — verbatim copy
    /// of `crates/relon-bench/benches/cmp_lua.rs::w7_relon_src` —
    /// now lowers cleanly through `lower_workspace_single`. The body
    /// produces an anon-Dict-return record with the `result` scalar
    /// field while `fib` is lifted to an internal let-bound closure
    /// handle (it does not appear in the host-visible schema).
    ///
    /// Pre-Phase-C this rejected at the return-schema build step with
    /// `UnsupportedTypeInMain { type_name: "Dict" }`; Phase C lifts
    /// that gap via [`anon_dict_return_plan`] +
    /// [`lower_anon_dict_body`]. Future Phase D scope: backend tier
    /// wiring (`Op::MakeClosure` / `Op::CallClosure`) for bytecode /
    /// trace_jit / LLVM emitters that still reject those ops.
    #[test]
    fn w7_production_source_lowers_via_anon_dict_return_plan() {
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     fib: (Int k) -> Int => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                     result: fib(n)\n\
                   }";
        let module = try_lower(src).expect("Phase C lowers W7 anon-Dict-return source");

        // The synthesised return schema only carries the `result`
        // scalar — `fib` is internal.
        let entry_idx = module
            .entry_func_index
            .expect("Phase C builds an entry func");
        let entry = &module.funcs[entry_idx];
        // Closure table populated with the W7 `fib` lambda.
        assert_eq!(
            module.closure_table.len(),
            1,
            "expected one entry in closure_table for the `fib` lambda"
        );
        // The lambda Func body exists right after the entry func.
        assert!(
            module.funcs.len() >= 2,
            "expected entry + at least one lambda func, got {} funcs",
            module.funcs.len()
        );
        // Entry body emits `MakeClosure` exactly once (for `fib`).
        let make_count = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::MakeClosure { .. }))
            .count();
        assert_eq!(
            make_count, 1,
            "expected the entry to emit MakeClosure once for the `fib` let, got {make_count}"
        );
        // The `result` field's `fib(n)` lowers to `LetGet { Closure
        // } + CallClosure`.
        let call_count = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::CallClosure { .. }))
            .count();
        assert_eq!(
            call_count, 1,
            "expected the entry to emit CallClosure once for `result: fib(n)`, got {call_count}"
        );
    }

    /// A `#internal wrap(v): "<" + v + ">"` field closure's signature is
    /// read from its String-concat body (not defaulted to I64): the
    /// `@wrap()`-decorated field lowers to `Op::CallClosure` with a
    /// `String` return type, and the String-result field is marshalled
    /// onto the return surface. Pre-fix the hardwired `ret_ty = I64`
    /// classified the field as Int and the `String + String` body failed
    /// to lower (`unsupported operator Add`).
    #[test]
    fn anon_dict_string_concat_decorator_returns_string() {
        let src = "#relaxed\n#main(Int n) -> Dict\n\
                   { #internal\n wrap(v): \"<\" + v + \">\",\n \
                   @wrap()\n out: \"hi\" }";
        let module = try_lower(src).expect("String-concat field decorator lowers");
        let entry_idx = module.entry_func_index.expect("entry func");
        let entry = &module.funcs[entry_idx];
        // The decorated `out` field calls `wrap` via a closure whose
        // return type is String (the concat body), not the old I64.
        let string_ret_call = entry.body.iter().any(|t| {
            matches!(
                &t.op,
                Op::CallClosure { ret_ty, param_tys }
                    if *ret_ty == IrType::String
                        && param_tys.as_slice() == [IrType::String]
            )
        });
        assert!(
            string_ret_call,
            "expected a CallClosure {{ [String] -> String }} for the @wrap field, \
             body ops: {:?}",
            entry.body.iter().map(|t| &t.op).collect::<Vec<_>>()
        );
    }

    /// Wave B: the `examples/pricing.relon` Float-valued currency shape
    /// — `currency(symbol, val): symbol + " " + val` decorating a
    /// `Float` field — now lowers. The String-concat body inference
    /// types both params as `String` (concat-coercible), and the
    /// call-site renders the Float value through `Op::FloatToStr`
    /// before the `Op::CallClosure { [String, String] -> String }`,
    /// byte-identical to the tree-walk oracle (which renders the Float
    /// at the `+` inside the body via the same `Display`).
    #[test]
    fn anon_dict_float_string_concat_decorator_lowers() {
        let src = "#relaxed\n#main(Float price) -> Dict\n\
                   { #internal\n currency(symbol, val): symbol + \" \" + val,\n \
                   @currency(\"USD\")\n display: price }";
        let module = try_lower(src).expect("Float-valued String-concat decorator lowers");
        let entry_idx = module.entry_func_index.expect("entry func");
        let entry = &module.funcs[entry_idx];
        let has_float_to_str = entry.body.iter().any(|t| matches!(&t.op, Op::FloatToStr));
        let string_ret_call = entry.body.iter().any(|t| {
            matches!(
                &t.op,
                Op::CallClosure { ret_ty, param_tys }
                    if *ret_ty == IrType::String
                        && param_tys.as_slice() == [IrType::String, IrType::String]
            )
        });
        assert!(
            has_float_to_str && string_ret_call,
            "expected Op::FloatToStr + CallClosure {{ [String, String] -> String }}, \
             body ops: {:?}",
            entry.body.iter().map(|t| &t.op).collect::<Vec<_>>()
        );
    }

    /// Honesty guard for the concat-coercible mask: an explicitly
    /// `String`-annotated closure param is never call-site coerced, so
    /// passing a Float to it still caps loudly.
    #[test]
    fn annotated_string_param_rejects_float_arg() {
        let src = "#relaxed\n#main(Float price) -> Dict\n\
                   { #internal\n tag: (String s) => \"<\" + s + \">\",\n \
                   display: tag(price) }";
        let err = try_lower(src).expect_err("annotated String param must reject Float");
        assert!(
            matches!(err, LoweringError::StdlibArgTypeMismatch { .. }),
            "expected a loud StdlibArgTypeMismatch for the annotated param, got {err:?}"
        );
    }

    /// Phase B foundation check: the canonical schema digest treats
    /// the new [`TypeRepr::Closure`] variant as a structural shape.
    ///
    /// Two closure-typed fields with the same `(params, ret)` shape
    /// must collapse to the same digest, and a shape difference (extra
    /// param, different return) must invalidate the digest so a host
    /// SDK refuses to load a module whose declared closure surface
    /// drifted from its compile-time view.
    ///
    /// The test is gated on the digest plumbing alone — no lowering of
    /// W7-shape user source. The closure-as-value lowering itself
    /// stays Phase C scope; this test only confirms the type-system
    /// hook the future implementation will hang behaviour off.
    #[test]
    fn typerepr_closure_digest_distinguishes_signature_shapes() {
        use relon_abi::schema_canonical::{schema_hash, Field, Schema, TypeRepr};

        let int_to_int = TypeRepr::Closure {
            params: vec![TypeRepr::Int],
            ret: Box::new(TypeRepr::Int),
        };
        // Same shape, different declaration — must collapse.
        let int_to_int_clone = TypeRepr::Closure {
            params: vec![TypeRepr::Int],
            ret: Box::new(TypeRepr::Int),
        };
        // Extra param — must distinguish.
        let int_int_to_int = TypeRepr::Closure {
            params: vec![TypeRepr::Int, TypeRepr::Int],
            ret: Box::new(TypeRepr::Int),
        };
        // Different return — must distinguish.
        let int_to_float = TypeRepr::Closure {
            params: vec![TypeRepr::Int],
            ret: Box::new(TypeRepr::Float),
        };

        let wrap = |ty: TypeRepr| Schema {
            name: "Probe".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![Field {
                name: "f".into(),
                ty,
                default: None,
            }],
        };

        // Structural equality.
        assert_eq!(
            schema_hash(&wrap(int_to_int.clone())),
            schema_hash(&wrap(int_to_int_clone)),
            "two structurally identical closure-typed fields must hash equal"
        );
        // Shape sensitivity.
        assert_ne!(
            schema_hash(&wrap(int_to_int.clone())),
            schema_hash(&wrap(int_int_to_int)),
            "param-arity change must invalidate the digest"
        );
        assert_ne!(
            schema_hash(&wrap(int_to_int)),
            schema_hash(&wrap(int_to_float)),
            "return-type change must invalidate the digest"
        );
    }

    /// Phase B layout-guard check: closure-typed fields must reject at
    /// [`SchemaLayout::offsets_for`] so the binary-handshake builder
    /// can't accidentally lay a non-portable scratch-heap pointer into
    /// a host-visible record. The canonical schema digest already
    /// distinguishes the shape (see the digest test above); the layout
    /// pass is the second line of defence so a hand-built `Schema`
    /// that bypasses the lowering pass still surfaces a typed error
    /// rather than a silent dangle.
    #[test]
    fn closure_field_rejects_at_schema_layout() {
        use relon_abi::layout::{LayoutError, SchemaLayout};
        use relon_abi::schema_canonical::{Field, Schema, TypeRepr};

        let schema = Schema {
            name: "ProbeWithClosure".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![Field {
                name: "fib".into(),
                ty: TypeRepr::Closure {
                    params: vec![TypeRepr::Int],
                    ret: Box::new(TypeRepr::Int),
                },
                default: None,
            }],
        };
        let err = SchemaLayout::offsets_for(&schema)
            .expect_err("closure fields must reject at layout time");
        match err {
            LayoutError::UnsupportedTypeInLayoutV1 { kind, field } => {
                assert_eq!(kind, "Closure", "expected kind tag `Closure`, got {kind}");
                assert_eq!(field, "fib");
            }
            other => panic!(
                "expected LayoutError::UnsupportedTypeInLayoutV1 {{ kind: \"Closure\" }}, got {other:?}"
            ),
        }
    }

    /// W5-P1 verification: a `{str:int}` dict literal sitting on a
    /// `#internal` field of an anon-Dict-return body lowers to a
    /// dict-value capture — `Op::ConstDict` materialising the entry set
    /// followed by `Op::LetSet { ty: IrType::Dict }` into an internal
    /// let-local. The dict field contributes no host-visible record
    /// slot (it is internal, like a lifted closure), so the synthesised
    /// return schema only carries the `result` scalar.
    ///
    /// This is the construction + capture half of the W5 dict-value
    /// surface. The read half (`DictGetByStringKey`) is a P3 follow-up;
    /// this `result` field is a plain scalar so the body stays inside
    /// the P1 envelope (no DictGet).
    #[test]
    fn w5p1_dict_value_field_lowers_to_const_dict_let() {
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     d: { a: 1, b: 2, c: 3 },\n\
                     result: n\n\
                   }";
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        let lowered = lower_workspace_single(&analyzed, &ast)
            .expect("W5-P1 lowers anon-Dict-return with dict-value field");
        let module = &lowered.module;

        let entry_idx = module.entry_func_index.expect("W5-P1 builds an entry func");
        let entry = &module.funcs[entry_idx];

        // Exactly one ConstDict carrying the source-order entries.
        let const_dicts: Vec<&Vec<(String, i64)>> = entry
            .body
            .iter()
            .filter_map(|t| match &t.op {
                Op::ConstDict { entries, .. } => Some(entries),
                _ => None,
            })
            .collect();
        assert_eq!(
            const_dicts.len(),
            1,
            "expected exactly one ConstDict for the `d` dict-value field"
        );
        assert_eq!(
            const_dicts[0],
            &vec![
                ("a".to_string(), 1i64),
                ("b".to_string(), 2i64),
                ("c".to_string(), 3i64),
            ],
            "ConstDict must carry the source-declaration-order entries"
        );

        // The dict pointer is stashed into a Dict-typed let-local.
        let dict_let_sets = entry
            .body
            .iter()
            .filter(|t| {
                matches!(
                    t.op,
                    Op::LetSet {
                        ty: IrType::Dict,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            dict_let_sets, 1,
            "expected one `LetSet {{ ty: Dict }}` capturing the dict value"
        );

        // `d` is internal — the host-visible return schema carries only
        // the `result` scalar field.
        let schema = &lowered.return_schema;
        assert_eq!(
            schema.fields.len(),
            1,
            "dict-value field must be internal — schema carries only `result`"
        );
        assert_eq!(schema.fields[0].name, "result");
    }

    /// W5-P1 honesty edge: a dict field whose value is not a plain Int
    /// literal must reject at lowering (value-type widening is P2/P3),
    /// rather than silently lowering a half-supported shape.
    #[test]
    fn w5p1_non_int_dict_value_rejects() {
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     d: { a: 1, b: \"two\" },\n\
                     result: n\n\
                   }";
        let err = try_lower(src).expect_err("non-Int dict value must reject in P1");
        assert!(
            matches!(err, LoweringError::UnsupportedExpr { .. }),
            "expected UnsupportedExpr for non-Int dict value, got {err:?}"
        );
    }

    /// AOT-3 verification: a W17-shaped where-bound recursive helper
    /// (`bs(lo, hi, t): ...` declared in a `where { ... }` clause and
    /// called from a `reduce` fold) now lowers cleanly. Pre-AOT-3 the
    /// `bs(...)` closure binding hit the `Expr::Closure { .. } =>
    /// ClosureAcrossBoundary` arm of `lower_expr` and the whole source
    /// was rejected at IR lowering, leaving W17 `n/a` on every compiled
    /// backend.
    ///
    /// The lowered entry func must:
    /// * emit `MakeClosure` once for the lifted `bs` let,
    /// * emit `CallClosure` for the recursive self-calls + the fold-site
    ///   call (the W17 body has three `bs(...)` calls: the two recursive
    ///   tails and the `acc + bs(0, n, ...)` fold combine).
    #[test]
    fn w17_where_bound_recursive_helper_lifts_to_closure_let() {
        // W17-shaped binary search: pure recursion over an arithmetic
        // index range, no list materialisation.
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
                   where {\n\
                     bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
                       (lo + hi) / 2 <= t\n\
                         ? bs((lo + hi) / 2, hi, t)\n\
                         : bs(lo, (lo + hi) / 2, t)\n\
                     )\n\
                   }";
        let module = try_lower(src).expect("AOT-3 lowers W17 where-bound recursive helper");

        // The `bs` lambda lands in the closure table.
        assert_eq!(
            module.closure_table.len(),
            1,
            "expected one closure-table entry for the lifted `bs` helper"
        );
        let entry_idx = module.entry_func_index.expect("AOT-3 builds an entry func");
        let entry = &module.funcs[entry_idx];

        // Exactly one MakeClosure for the `bs` let-binding.
        let make_count = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::MakeClosure { .. }))
            .count();
        assert_eq!(
            make_count, 1,
            "expected MakeClosure once for the `bs` where-binding, got {make_count}"
        );

        // Walk the op tree (the reduce fold lowers into an `Op::Loop`,
        // so the fold-site call nests inside the loop body).
        fn count_call_closure(body: &[TaggedOp]) -> usize {
            let mut n = 0;
            for t in body {
                match &t.op {
                    Op::CallClosure { .. } => n += 1,
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        n += count_call_closure(then_body);
                        n += count_call_closure(else_body);
                    }
                    Op::Block { body, .. } | Op::Loop { body, .. } => n += count_call_closure(body),
                    _ => {}
                }
            }
            n
        }
        // The fold-site `bs(0, n, ...)` call lowers to CallClosure in
        // the entry body (nested inside the reduce loop).
        let entry_calls = count_call_closure(&entry.body);
        assert_eq!(
            entry_calls, 1,
            "expected one fold-site CallClosure in the entry body, got {entry_calls}"
        );

        // Walk every non-entry func (the lifted `bs` lambda) and count
        // its recursive CallClosure self-calls — the two bisection
        // tails.
        let lambda_self_calls: usize = module
            .funcs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != entry_idx)
            .map(|(_, f)| count_call_closure(&f.body))
            .sum();
        assert_eq!(
            lambda_self_calls, 2,
            "expected two recursive self-CallClosure inside the `bs` lambda, got {lambda_self_calls}"
        );
    }
}
