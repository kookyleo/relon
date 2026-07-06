//! Lowering sub-module: `where` bindings, binary operators, f-strings,
//! ternaries, and branch bodies.
//!
//! Owns `lower_where` (let-local scoping / shadowing), `lower_binary`
//! plus the comparison / arithmetic op constructors, the f-string
//! family (`lower_fstring`, value-to-string coercion, the
//! `StrConcatN` chain fold), and ternary / branch lowering in both
//! the plain and typed-slot (`_as_type`) forms.

use super::*;

/// Lower a `<expr> where { name: value, ... }` form by emitting one
/// `LetSet` per binding (in declaration order) and then lowering
/// `expr` with the names in scope.
///
/// Each binding picks up a fresh per-function let-local index;
/// shadowing is supported (a re-declared name uses a new index, and
/// the outer binding becomes unreachable inside the inner expression
/// but stays valid after). We restore the outer scope after the
/// inner body lowers so the trailing `StoreField` sees a clean
/// virtual stack.
pub(super) fn lower_where(
    expr: &Node,
    bindings: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let pairs = match &*bindings.expr {
        Expr::Dict(pairs) => pairs,
        _ => {
            return Err(cap!(
                "lower_where.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("Where(bindings={})", bindings.expr.kind()),
                    range,
                }
            ));
        }
    };
    let saved_lets_len = ctx.lets.len();
    for (key, value) in pairs {
        let name = match key {
            TokenKey::String(s, _, _) => s.clone(),
            _ => {
                return Err(cap!(
                    "lower_where.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: "Where(non-string-binding-key)".to_string(),
                        range,
                    }
                ));
            }
        };
        // AOT-3: a where-binding whose value is a closure (the
        // method-shorthand `name(params): body` desugars to an
        // `Expr::Closure`) is lifted to a closure-typed let-binding,
        // exactly the way the W7 anon-Dict-return path lifts its
        // `#internal fib: (Int k) -> Int => ...` field (see `lower_anon_dict_body`).
        // The closure let-idx + its signature are registered in
        // `ctx.closure_let_signatures` BEFORE the body lowers so a
        // recursive self-call inside the body (W17's `bs((lo+hi)/2, hi,
        // t)`) resolves through `try_lower_local_closure_call` ->
        // `Op::CallClosure`. Pre-Phase-AOT-3 this hit the
        // `Expr::Closure { .. } => ClosureAcrossBoundary` arm of
        // `lower_expr` and the whole W17 source was rejected at lowering.
        if let Expr::Closure {
            params,
            body: closure_body,
            return_type,
        } = &*value.expr
        {
            // Unannotated params / return default to `Int` (I64) — the
            // same convention `anon_dict_return_plan` uses for W7. A
            // `(k: Bool) =>` style annotation is honoured when present.
            //
            // AOT-4 (W16 slice): an unannotated param defaults to
            // `List<Int>` when the body uses it as a list — indexed
            // (`p[i]`) or handed to `_len(p)` / `_list_filter(p, ...)`.
            // The W16 quicksort helper `sum_qs(xs)` takes a `List<Int>`
            // handle with NO annotation (the closure-param grammar does
            // not accept `List<Int>`), so the recursive lift must infer
            // it; an I64 default would mis-tag the recursive list arg
            // and reject lowering. The inference is a cheap structural
            // walk that only fires on the list-shaped uses above.
            let mut param_irts: Vec<IrType> = Vec::with_capacity(params.len());
            for p in params {
                let annotated = p
                    .type_hint
                    .as_ref()
                    .and_then(type_node_to_canonical)
                    .and_then(|r| type_repr_to_ir_type(&r).ok());
                let irt = annotated.unwrap_or_else(|| {
                    // #359 (W20): a param passed positionally into a
                    // sibling where-bound closure adopts that closure's
                    // declared param type (so `accel(s, i)`'s `s` picks up
                    // `ListFloat` from `pair_force`'s first param even
                    // though `accel` never indexes `s` directly). This
                    // runs first because it is a precise propagation, not
                    // a structural guess.
                    if let Some(t) = infer_param_from_sibling_call(&p.name, &closure_body.expr, ctx)
                    {
                        t
                    } else if closure_param_used_as_list_float(&p.name, &closure_body.expr) {
                        // Indexed param whose element flows into Float
                        // arithmetic (W20 `step` / `pair_force` read
                        // `s[k]` then combine with `dt` / `soft` / a
                        // mass) is a `List<Float>`.
                        IrType::ListFloat
                    } else if closure_param_used_as_list_int(&p.name, &closure_body.expr) {
                        IrType::ListInt
                    } else if closure_param_used_as_float(&p.name, &closure_body.expr) {
                        // Scalar param used as a bare operand in Float
                        // arithmetic (W20 `pair_force`'s `mj` mass:
                        // `(s[j] - s[i]) * mj * (1.0 / ..)`) is a `Float`.
                        IrType::F64
                    } else {
                        IrType::I64
                    }
                });
                param_irts.push(irt);
            }
            // Return type: honour an explicit annotation, else infer
            // from the body. AOT-3 hardcoded I64 (W17's `bs` returns
            // `Int`); AOT-4's W18 `is_prime` returns `Bool`, so the
            // self-recursive call inside the ternary must agree with
            // the sibling `true`/`false` literal branches — a fixed
            // I64 default surfaces an `IfBranchTypeMismatch`. The
            // inference is a cheap structural walk (ternary branches /
            // comparison + logical ops -> Bool, otherwise Int).
            //
            // #359 (W20): the ctx-aware variant additionally resolves
            // `F64` (Float literal / Float arith / a call into a sibling
            // closure that returns `F64`) and `ListFloat` (a `[...]`
            // list literal of Float-valued elements) — `pair_force` /
            // `accel` return `F64`, `step` returns a `List<Float>`.
            let ret_ty = return_type
                .as_ref()
                .and_then(type_node_to_canonical)
                .and_then(|r| type_repr_to_ir_type(&r).ok())
                .unwrap_or_else(|| {
                    infer_closure_body_ret_ty_ctx(&closure_body.expr, &param_irts, params, ctx)
                });

            // Pre-allocate the let-idx the closure handle lands in and
            // register both the binding and its signature before the
            // body lowers, so a self-recursive call resolves to this
            // slot.
            let idx = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.lets.push(LetBinding {
                name,
                idx,
                ty: IrType::Closure,
                schema_brand: None,
                type_repr: None,
            });
            ctx.closure_let_signatures
                .insert(idx, (param_irts.clone(), ret_ty));

            // Lower the closure body — pushes `IrType::Closure` and
            // appends the lambda Func to `ctx.lambda_funcs`.
            lower_closure_as_value(&value.expr, value.range, &param_irts, ret_ty, ctx)?;
            let popped = ctx.tstack.pop().ok_or(cap!(
                "lower_where.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: "Where(closure-binding-empty-stack)".to_string(),
                    range: value.range,
                }
            ))?;
            debug_assert_eq!(popped, IrType::Closure);
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx,
                    ty: IrType::Closure,
                },
                range: value.range,
            });
            continue;
        }
        // AOT-4 (W16 slice): a where-binding whose value is a bare
        // `range(a, b)` (or `range(b)`) MUST materialise into a
        // `List<Int>` arena record so a downstream consumer can index
        // it (`xs[0]`) or recurse on a filtered sub-list. The eliding
        // range peepholes only fire for the fusable `.sum` / `.len` /
        // `.reduce` terminals — a where-bound range has no such
        // terminal, so without this it would fall through to the
        // generic FnCall dispatch and reject as
        // `UnknownStdlibMethod { name: "range" }`.
        // AOT-4 (W19 slice): also materialise a where-bound nested
        // `range(a, b).map((p) => <inner>)` — a `List<List<Int>>` (when
        // `<inner>` is itself a materialisable row) or a `List<Int>`
        // (when `<inner>` is `Int`-valued). `emit_list_value_materialize`
        // subsumes the bare-range case, so route both shapes through it.
        if match_bare_range(&value.expr).is_some()
            || match_materializable_outer_map(&value.expr).is_some()
        {
            emit_list_value_materialize(&value.expr, value.range, ctx)?;
            let value_ty = ctx.tstack.pop().ok_or(cap!(
                "lower_where.unsupported_expr.4",
                LoweringError::UnsupportedExpr {
                    kind: "Where(range-materialize-empty-stack)".to_string(),
                    range: value.range,
                }
            ))?;
            debug_assert_eq!(value_ty, IrType::ListInt);
            let idx = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx,
                    ty: IrType::ListInt,
                },
                range: value.range,
            });
            ctx.lets.push(LetBinding {
                name,
                idx,
                ty: IrType::ListInt,
                schema_brand: None,
                type_repr: None,
            });
            continue;
        }
        // #359 (W20): a where-binding whose value is a Float-shaped list
        // literal (the n-body `init: [0.0, 1.0, ..]`) materialises into a
        // scratch `List<Float>` arena record so a downstream consumer (the
        // `range(n).reduce(init, ..)` accumulator) carries a runtime
        // handle, and `init[k]` / `s[k]` index it as `f64`. The bare
        // `Expr::List` arm would otherwise intern a `ConstListFloat` the
        // LLVM AOT envelope cannot materialise; routing the where-bound
        // literal through the scratch materialiser keeps the accumulator
        // a live arena handle (only the LLVM AOT path reaches this — the
        // tree-walker resolves `init` directly).
        if let Expr::List(items) = &*value.expr {
            if !items.is_empty() && list_is_float_shaped(items) {
                emit_list_float_literal_materialize(items, value.range, ctx)?;
                let value_ty = ctx.tstack.pop().ok_or(cap!(
                    "lower_where.unsupported_expr.5",
                    LoweringError::UnsupportedExpr {
                        kind: "Where(float-list-materialize-empty-stack)".to_string(),
                        range: value.range,
                    }
                ))?;
                debug_assert_eq!(value_ty, IrType::ListFloat);
                let idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx,
                        ty: IrType::ListFloat,
                    },
                    range: value.range,
                });
                ctx.lets.push(LetBinding {
                    name,
                    idx,
                    ty: IrType::ListFloat,
                    schema_brand: None,
                    type_repr: None,
                });
                continue;
            }
        }
        // #359 (W20 div-trap unlock): record a where-bound *scalar
        // literal* (`soft: 0.1`, `dt: 0.01`, `m0: 1.0`, ...) as a
        // compile-time constant keyed by its let-idx, so a closure that
        // captures it can inline the constant instead of loading it from
        // the captures struct (see `LowerCtx::const_let_values`). Only
        // bare `Int` / `Float` / `Bool` literals qualify — anything
        // computed still flows through the normal capture path.
        let scalar_const = match &*value.expr {
            Expr::Int(i) => Some(ScalarConst::I64(*i)),
            Expr::Float(f) => Some(ScalarConst::F64(f.into_inner())),
            Expr::Bool(b) => Some(ScalarConst::Bool(*b)),
            _ => None,
        };
        lower_expr(&value.expr, value.range, ctx)?;
        let value_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_where.unsupported_expr.6",
            LoweringError::UnsupportedExpr {
                kind: "Where(binding-empty-stack)".to_string(),
                range: value.range,
            }
        ))?;
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: value_ty },
            range: value.range,
        });
        if let Some(sc) = scalar_const {
            ctx.const_let_values.insert(idx, sc);
        }
        ctx.lets.push(LetBinding {
            name,
            idx,
            ty: value_ty,
            schema_brand: None,
            type_repr: None,
        });
    }
    lower_expr(&expr.expr, expr.range, ctx)?;
    // Pop only the bindings we pushed in this frame — preserves
    // outer-scope lets for sibling expressions.
    ctx.lets.truncate(saved_lets_len);
    Ok(())
}

/// Lower one binary expression. Splits the arithmetic + comparison
/// paths so each surface keeps its rejection rules explicit.
pub(super) fn lower_binary(
    op: Operator,
    lhs: &Node,
    rhs: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // #165 — collapse a left-leaning `String + String + ... + String`
    // chain into a single `Op::StrConcatN { operand_count: N }` so
    // every IR-consuming backend routes through a single allocation instead of N - 1
    // pairwise `Op::Add(String)` allocs. The fold is gated on AST
    // shape (the outer node is `Add` and the LHS is itself an `Add`),
    // matches the tree-walker's `try_eval_string_concat_chain` filter,
    // and bails to standard pair-wise lowering when the chain mixes
    // non-String operand types.
    if matches!(op, Operator::Add)
        && matches!(lhs.expr.as_ref(), Expr::Binary(Operator::Add, _, _))
        && try_lower_str_concat_chain(lhs, rhs, range, ctx)?
    {
        return Ok(());
    }
    // Short-circuit Boolean operators. Lowered as a guard-style `Op::If`:
    //
    // * `a && b` → `a ? b : false`
    // * `a || b` → `a ? true  : b`
    //
    // Each branch lowers in its own sub-stream / tstack via
    // [`lower_branch`] so a non-Bool side bails cleanly. Short-circuit
    // semantics fall out of `Op::If`: the unselected branch's ops never
    // execute, matching the tree-walker's eager-eval-of-LHS-then-RHS
    // discipline. Open follow-up #264-cont: this unblocks predicate
    // chains in cmp_lua W10's `allow`-style closures and any future
    // boolean-heavy workload, without disturbing the arithmetic /
    // comparison paths below.
    if matches!(op, Operator::And | Operator::Or) {
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        let lhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.1",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        if lhs_ty != IrType::Bool {
            return Err(cap!(
                "lower_binary.unsupported_operator.2",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        let (then_body, then_ty) = if matches!(op, Operator::And) {
            // a && b: then-branch is `b`, else-branch is `false`.
            lower_branch(rhs, range, ctx)?
        } else {
            // a || b: then-branch is `true`, else-branch is `b`.
            let saved_out = std::mem::take(&mut ctx.out);
            let saved_stack = std::mem::take(&mut ctx.tstack);
            ctx.out.push(TaggedOp {
                op: Op::ConstBool(true),
                range,
            });
            ctx.tstack.push(IrType::Bool);
            let body = std::mem::replace(&mut ctx.out, saved_out);
            let stack = std::mem::replace(&mut ctx.tstack, saved_stack);
            (body, stack[0])
        };
        let (else_body, else_ty) = if matches!(op, Operator::And) {
            // a && b: else-branch is `false`.
            let saved_out = std::mem::take(&mut ctx.out);
            let saved_stack = std::mem::take(&mut ctx.tstack);
            ctx.out.push(TaggedOp {
                op: Op::ConstBool(false),
                range,
            });
            ctx.tstack.push(IrType::Bool);
            let body = std::mem::replace(&mut ctx.out, saved_out);
            let stack = std::mem::replace(&mut ctx.tstack, saved_stack);
            (body, stack[0])
        } else {
            // a || b: else-branch is `b`.
            lower_branch(rhs, range, ctx)?
        };
        if then_ty != IrType::Bool || else_ty != IrType::Bool {
            return Err(cap!(
                "lower_binary.unsupported_operator.3",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        ctx.out.push(TaggedOp {
            op: Op::If {
                result_ty: IrType::Bool,
                then_body,
                else_body,
            },
            range,
        });
        ctx.tstack.push(IrType::Bool);
        return Ok(());
    }
    if let Some(ir_op_ctor) = arithmetic_op_ctor(op) {
        // Lower the LHS into `ctx.out`, then capture the RHS into a
        // detached stream so a mixed `Int`/`Float` pair can splice an
        // `Op::ConvertI64ToF64` promotion onto whichever operand is the
        // `I64` one — the LHS sits buried under the RHS once both are
        // emitted, so we cannot append to it after the fact.
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        let lhs_ty = *ctx.tstack.last().ok_or(cap!(
            "lower_binary.unsupported_operator.4",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        let saved_out = std::mem::take(&mut ctx.out);
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ops = std::mem::replace(&mut ctx.out, saved_out);
        let rhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.5",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        // Pop the LHS too — the result type tag is recomputed below.
        ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.6",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        // Int↔Float promotion (#359 / #362): mirror the tree-walker's
        // `NumericValue::as_f64()` — when one operand is `Int` and the
        // other `Float`, the `Int` operand is widened to `f64` and the
        // binop runs as `F64`, result `Float`. `Add` / `Sub` / `Mul` /
        // `Div` / `Mod` all promote: the tree-walker's
        // `eval_numeric_division` computes `a.as_f64() % b.as_f64()`
        // (Rust f64 `%` = `fmod`, truncated remainder, sign of the
        // dividend) for any non-`Int`/`Int` `%`, so mixed `Int`/`Float`
        // `%` widens here and lowers to `Op::Mod(F64)` below.
        let mixed_promote = matches!(
            (lhs_ty, rhs_ty),
            (IrType::I64, IrType::F64) | (IrType::F64, IrType::I64)
        ) && matches!(
            op,
            Operator::Add | Operator::Sub | Operator::Mul | Operator::Div | Operator::Mod
        );
        if mixed_promote {
            if lhs_ty == IrType::I64 {
                // Promote the LHS (currently on top of `ctx.out`) before
                // emitting the RHS stream.
                ctx.out.push(TaggedOp {
                    op: Op::ConvertI64ToF64,
                    range: lhs.range,
                });
                ctx.out.extend(rhs_ops);
            } else {
                // LHS is already `F64`; emit the RHS then promote it.
                ctx.out.extend(rhs_ops);
                ctx.out.push(TaggedOp {
                    op: Op::ConvertI64ToF64,
                    range: rhs.range,
                });
            }
            ctx.out.push(TaggedOp {
                op: ir_op_ctor(IrType::F64),
                range,
            });
            ctx.tstack.push(IrType::F64);
            return Ok(());
        }
        // `String + non-String` / `non-String + String` coercion concat.
        // The tree-walk oracle (`arithmetic.rs`) renders the non-String
        // operand through `Display` and `format!`-concats it. We mirror
        // that by rendering the non-String operand through the shared
        // value→String skeleton and folding both String operands with a
        // single `StrConcatN { 2 }` (byte-identical to the
        // `String + String` concat path). Only `Add` reaches here for
        // String operands (other arith ops were rejected upstream).
        //
        // Int / Bool / Float non-String operands are supported;
        // composite operands stay capped (the skeleton itself
        // loud-caps them, so any unsupported side returns through the
        // `?` below). The LHS
        // ops still sit at the tail of `ctx.out` and the RHS ops are
        // still detached in `rhs_ops`, so each side is rendered in its
        // own stream before the two are concatenated in source order.
        if lhs_ty != rhs_ty
            && matches!(op, Operator::Add)
            && (lhs_ty == IrType::String || rhs_ty == IrType::String)
        {
            // Render the LHS (ops already at the tail of `ctx.out`) to
            // String. `lower_value_to_string` pushes a tstack tag we do
            // not need here — pop it back off after each call.
            lower_value_to_string(lhs_ty, lhs.range, ctx)?;
            ctx.tstack.pop();
            // Splice in the RHS stream, then render it to String too.
            ctx.out.extend(rhs_ops);
            lower_value_to_string(rhs_ty, rhs.range, ctx)?;
            ctx.tstack.pop();
            // Two String operands in source order → one concat alloc.
            ctx.out.push(TaggedOp {
                op: Op::StrConcatN { operand_count: 2 },
                range,
            });
            ctx.tstack.push(IrType::String);
            return Ok(());
        }
        // Homogeneous path: re-attach the RHS stream verbatim and fall
        // through to the same-type checks below.
        ctx.out.extend(rhs_ops);
        if lhs_ty != rhs_ty {
            return Err(cap!(
                "lower_binary.unsupported_operator.7",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        // F-D7-D: `String + String` lowers to `Op::Add(IrType::String)`.
        // Backends route through their generic string-concat dispatch
        // (Value-level concat in the evaluator, and a host-shim call in
        // cranelift). Only `Operator::Add` is
        // accepted for strings — `String - String` / `String * String`
        // would have been rejected upstream by the analyzer
        // (`infer_binary` returns `None`) so the lhs/rhs types would
        // not both be `String` here for any non-Add arith op.
        if lhs_ty == IrType::String {
            if !matches!(op, Operator::Add) {
                return Err(cap!(
                    "lower_binary.unsupported_operator.8",
                    LoweringError::UnsupportedOperator { op, range }
                ));
            }
            ctx.out.push(TaggedOp {
                op: Op::Add(IrType::String),
                range,
            });
            ctx.tstack.push(IrType::String);
            return Ok(());
        }
        // Only Int / Float pairs support arithmetic.
        if !matches!(lhs_ty, IrType::I64 | IrType::F64) {
            return Err(cap!(
                "lower_binary.unsupported_operator.9",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        // #362: `F64 % F64` lowers to `Op::Mod(F64)` (and so does the
        // promoted-mixed `%` above) to match the tree-walker, which
        // computes `a.as_f64() % b.as_f64()`. Backends that lack a
        // native float remainder (wasm has no `f64.rem`; cranelift has
        // no `frem` and no fmod libcall wired) gracefully reject
        // `Op::Mod(F64)` at codegen — never a panic, never a wrong
        // answer. The LLVM AOT lowers it to `frem` (= `fmod`).
        ctx.out.push(TaggedOp {
            op: ir_op_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(lhs_ty);
        return Ok(());
    }
    if let Some(cmp_ctor) = comparison_op_ctor(op) {
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.10",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        let lhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.11",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        if lhs_ty != rhs_ty {
            return Err(cap!(
                "lower_binary.unsupported_operator.12",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        // Phase 2.c supports comparisons on Int / Float / Bool /
        // Unit. Bool / Unit only support `==` / `!=`; ordering
        // (`<`, `<=`, `>`, `>=`) is rejected at the comparison
        // codegen layer too, but we surface it here as a lowering
        // error so the message stays user-facing.
        match (lhs_ty, op) {
            (IrType::I64 | IrType::F64, _) => {}
            (IrType::Bool, Operator::Eq | Operator::Ne) => {}
            (IrType::Unit, Operator::Eq | Operator::Ne) => {}
            _ => {
                return Err(cap!(
                    "lower_binary.unsupported_operator.13",
                    LoweringError::UnsupportedOperator { op, range }
                ))
            }
        }
        ctx.out.push(TaggedOp {
            op: cmp_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(IrType::Bool);
        return Ok(());
    }
    // Wave R2 — `a | f(...)` pipe operator. A pure static desugar: the
    // tree-walker (eval.rs `Expr::Binary(Operator::Pipe, ..)`) prepends
    // the LHS value as the FIRST positional argument of the right-hand
    // call and dispatches the call. We reproduce that here by building a
    // synthetic argument list `[positional(lhs), ...rhs_args]` and
    // routing it through `lower_fn_call`, so `xs | list.sum`
    // (zero extra args) lowers identically to `list.sum(xs)`, and
    // `xs | _list_reduce(0, f)` lowers identically to
    // `_list_reduce(xs, 0, f)`. No new IR op is needed — the desugar is
    // entirely at the call-shape level, and every downstream peephole
    // (`list.sum(range(..))`, `_len(_list_filter(..))`, ...) fires
    // exactly as it would for the spelled-out call. The tree-walker uses
    // `right.range` for the dispatched call; we mirror that by using the
    // RHS node's range.
    if matches!(op, Operator::Pipe) {
        match rhs.expr.as_ref() {
            // `a | f(extra...)` ≡ `f(a, extra...)`.
            Expr::FnCall { path, args } => {
                let mut piped_args: Vec<relon_parser::CallArg> = Vec::with_capacity(args.len() + 1);
                piped_args.push(relon_parser::CallArg {
                    name: None,
                    value: lhs.clone(),
                });
                piped_args.extend(args.iter().cloned());
                return lower_fn_call(path, &piped_args, rhs.range, ctx);
            }
            // `a | path` (bare, no parens) — `path` parses as a
            // `Variable` naming a function/stdlib entry. The tree-walker
            // resolves it to a closure value and applies `a` as the sole
            // argument, which is byte-equal to the zero-extra-arg call
            // `path(a)`. Route it through the same call lowering.
            Expr::Variable(path) => {
                let piped_args = vec![relon_parser::CallArg {
                    name: None,
                    value: lhs.clone(),
                }];
                return lower_fn_call(path, &piped_args, rhs.range, ctx);
            }
            // `a | <closure-literal>` / `a | <other>` — the tree-walker
            // applies a closure *value* on the RHS. Closures cannot
            // cross the AOT boundary as first-class values (already
            // capped as `lower_expr.closure_across_boundary`), so this
            // residual pipe shape stays capped via the catch-all below.
            _ => {}
        }
    }
    Err(cap!(
        "lower_binary.unsupported_operator.14",
        LoweringError::UnsupportedOperator { op, range }
    ))
}

/// Wave R2 — lower an f-string `f"...${e}..."` into a single `String`
/// IR value. Pure static desugar of the tree-walker's `Expr::FString`
/// arm (eval.rs): the parts are concatenated left-to-right, each
/// interpolated value coerced to a `String` via the same rendering the
/// tree-walker uses (`write!(result, "{}", val)`, i.e. `Display`):
///
///   * a `String`-typed interpolation is used verbatim,
///   * an `Int`-typed interpolation is rendered as its base-10 decimal
///     (`Op::IntToStr`, byte-exact with `i64` `Display`),
///   * literal parts become `Op::ConstString`.
///
/// Each part is reduced to exactly one `String` operand; the operands
/// are then joined with `Op::StrConcatN { operand_count: parts }` (which
/// matches the chained `String + String` lowering byte-for-byte). The
/// 0-part (`f""`) and 1-part degenerate shapes skip the concat. Float /
/// Bool / other interpolation types are not yet byte-proven and stay
/// capped via the catch-all `lower_expr.unsupported_expr.8`.
pub(super) fn lower_fstring(
    parts: &[FStringPart],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // `f""` → the empty string literal (interned like any other).
    if parts.is_empty() {
        let idx = ctx.const_intern.borrow_mut().strings.intern("");
        ctx.out.push(TaggedOp {
            op: Op::ConstString {
                idx,
                value: String::new(),
            },
            range,
        });
        ctx.tstack.push(IrType::String);
        return Ok(());
    }
    // Emit one `String` operand per part, in source order.
    for part in parts {
        lower_fstring_part(part, range, ctx)?;
    }
    // Single part: the lone operand is already the result.
    if parts.len() == 1 {
        return Ok(());
    }
    // Join the N operands with a single concat allocation.
    let operand_count = parts.len() as u32;
    // The N String operands were just pushed; collapse their stack tags
    // into the single String result.
    for _ in 0..operand_count {
        ctx.tstack.pop().ok_or(cap!(
            "lower_expr.unsupported_expr.8",
            LoweringError::UnsupportedExpr {
                kind: "FString(operand stack underflow)".to_string(),
                range,
            }
        ))?;
    }
    ctx.out.push(TaggedOp {
        op: Op::StrConcatN { operand_count },
        range,
    });
    ctx.tstack.push(IrType::String);
    Ok(())
}

/// Unified "value → String" dispatch skeleton.
///
/// The operand's ops are assumed already emitted at the tail of
/// `ctx.out` with its IR type tag `value_ty` (its tstack entry has
/// **already been popped** by the caller). This function appends the
/// conversion ops that turn that operand into a single `String` value
/// and leaves the result tag (`IrType::String`) pushed onto the stack.
///
/// Both f-string interpolation (`f"${x}"`) and `String + non-String`
/// concat route through here so the rendered bytes stay identical
/// across the two surfaces and across every backend.
///
/// Coverage:
/// * `String` → identity (the operand already is the result).
/// * `I64`    → reuse the existing `Op::IntToStr` (byte-exact with
///   `i64` `Display`); routing through here must not change the bytes
///   the previous direct `IntToStr` emission produced.
/// * `Bool`   → render as `b ? "true" : "false"` using the existing
///   `Op::If` + `Op::ConstString` ops (no new op), byte-exact with the
///   tree-walk oracle's `Value::Bool` `Display`.
/// * `F64`    → `Op::FloatToStr` (Wave B). Byte-exact with the
///   oracle's `Value::Float` `Display` because every compiled backend
///   renders through the same Rust leaf helper
///   (`relon_ir::float_str::format_f64_display`).
/// * everything else (List / Dict / Schema / Null / …) → loud cap; we
///   never silently render a wrong byte sequence.
pub(super) fn lower_value_to_string(
    value_ty: IrType,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match value_ty {
        IrType::String => {
            // Already a String — coercion is identity.
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        IrType::I64 => {
            // Int → base-10 decimal, byte-exact with `Display`.
            ctx.out.push(TaggedOp {
                op: Op::IntToStr,
                range,
            });
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        IrType::Bool => {
            // Bool → "true" / "false", byte-exact with the oracle's
            // `Value::Bool` `Display`. Built from the existing `Op::If`
            // (stack effect `[Bool] -> [String]`) selecting one of two
            // interned `Op::ConstString` constants — no new op.
            let true_idx = ctx.const_intern.borrow_mut().strings.intern("true");
            let false_idx = ctx.const_intern.borrow_mut().strings.intern("false");
            let then_body = vec![TaggedOp {
                op: Op::ConstString {
                    idx: true_idx,
                    value: "true".to_string(),
                },
                range,
            }];
            let else_body = vec![TaggedOp {
                op: Op::ConstString {
                    idx: false_idx,
                    value: "false".to_string(),
                },
                range,
            }];
            ctx.out.push(TaggedOp {
                op: Op::If {
                    result_ty: IrType::String,
                    then_body,
                    else_body,
                },
                range,
            });
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        IrType::F64 => {
            // Float → decimal rendering, byte-exact with the oracle's
            // `f64` `Display` (all compiled backends call the same
            // Rust leaf helper — see `Op::FloatToStr`).
            ctx.out.push(TaggedOp {
                op: Op::FloatToStr,
                range,
            });
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        other => Err(cap!(
            "lower_expr.unsupported_expr.8",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "value-to-String coercion of type {other:?} — only String / Int / \
                     Bool / Float have a byte-exact AOT coercion (composite deferred)"
                ),
                range,
            }
        )),
    }
}

/// Lower one f-string part to exactly one `String` operand on the stack.
pub(super) fn lower_fstring_part(
    part: &FStringPart,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match part {
        FStringPart::Literal(s) => {
            let idx = ctx.const_intern.borrow_mut().strings.intern(s);
            ctx.out.push(TaggedOp {
                op: Op::ConstString {
                    idx,
                    value: s.clone(),
                },
                range,
            });
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        FStringPart::Interpolation(node) => {
            lower_expr(&node.expr, node.range, ctx)?;
            let ty = ctx.tstack.pop().ok_or(cap!(
                "lower_expr.unsupported_expr.8",
                LoweringError::UnsupportedExpr {
                    kind: "FString(interpolation operand underflow)".to_string(),
                    range: node.range,
                }
            ))?;
            // Route every interpolation through the shared value→String
            // skeleton so f-string and String-concat render identically.
            lower_value_to_string(ty, node.range, ctx)
        }
    }
}

/// #165 — fold a left-leaning `String + String + ... + String` chain
/// into a single `Op::StrConcatN { operand_count: N }` so every
/// IR-consuming backend allocates once instead of N - 1 times.
///
/// Returns `Ok(true)` when the fold fired (caller skips its standard
/// pair-wise path), `Ok(false)` when the chain mixes non-String
/// operand types (caller falls back). The function side-effects
/// `ctx.out` / `ctx.tstack` only when it commits — a mismatch is
/// detected before any append, so the caller's fall-back path
/// re-lowers from the original `lhs` / `rhs` nodes without seeing
/// stale ops.
///
/// AST shape preconditions (checked by the caller): the outer op is
/// `Operator::Add` and `lhs` is itself an `Expr::Binary(Add, _, _)` —
/// i.e. the chain has at least three leaves.
pub(super) fn try_lower_str_concat_chain(
    lhs: &Node,
    rhs: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    // Walk the LHS spine, peeling off each Add's RHS onto a stack so
    // the deepest leaf becomes `cursor` and `rhs_stack` holds the
    // outer-to-inner right operands. Pop order then yields source
    // order: deepest leaf first, then each RHS in chain order, then
    // the original outer `rhs` last.
    let mut rhs_stack: Vec<&Node> = Vec::with_capacity(4);
    let mut cursor: &Node = lhs;
    while let Expr::Binary(Operator::Add, inner_l, inner_r) = cursor.expr.as_ref() {
        rhs_stack.push(inner_r);
        cursor = inner_l;
    }
    // `cursor` is now the deepest non-Add leaf. Build the leaf list in
    // source order. `leaf_count >= 3` is guaranteed by the caller's
    // shape gate (`lhs` itself is a Binary(Add)).
    let mut leaves: Vec<&Node> = Vec::with_capacity(rhs_stack.len() + 2);
    leaves.push(cursor);
    while let Some(node) = rhs_stack.pop() {
        leaves.push(node);
    }
    leaves.push(rhs);
    debug_assert!(leaves.len() >= 3);
    let leaf_count = leaves.len();
    // Snapshot the emit cursor / type stack so we can roll back if any
    // leaf turns out non-String. We restore both on miss so the
    // caller's standard `lower_arith` path re-runs from the same
    // starting state without observing partial ops.
    let saved_out_len = ctx.out.len();
    let saved_tstack_len = ctx.tstack.len();
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    // Lower each leaf, type-checking after each so we abort early on
    // the first non-String operand (the common rejection — e.g. an
    // outer-Add was actually Schema-merge from a non-Add LHS, which
    // the caller's shape gate already filters).
    for leaf in &leaves {
        lower_expr(&leaf.expr, leaf.range, ctx)?;
        let leaf_ty = ctx.tstack.last().copied();
        if leaf_ty != Some(IrType::String) {
            // Mismatch — restore the snapshot and let the caller fall
            // back to pair-wise lowering. `const_intern` interning is
            // idempotent so any literals we pushed into the intern
            // table are still correct. AOT-4: a leaf may now lower a
            // closure (the W16 `+` chain has `qs(_list_filter(xs, (x)
            // => ...))` operands whose `_list_filter` builds a predicate
            // lambda), so restore `next_let_idx` AND truncate any
            // closure-table slots reserved during the discarded
            // speculative leaf lowering — leaking a slot would offset
            // every later `fn_table_idx` and dispatch the predicate to
            // the wrong lambda at runtime.
            ctx.out.truncate(saved_out_len);
            ctx.tstack.truncate(saved_tstack_len);
            ctx.next_let_idx = saved_next_let;
            ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            return Ok(false);
        }
    }
    // All N leaves are String — commit the StrConcatN op. Pop the N
    // type-stack entries and push the single result.
    ctx.tstack.truncate(saved_tstack_len);
    ctx.tstack.push(IrType::String);
    ctx.out.push(TaggedOp {
        op: Op::StrConcatN {
            operand_count: leaf_count as u32,
        },
        range,
    });
    Ok(true)
}

/// Lower a ternary `cond ? then : els` into `Op::If`. The branches
/// must agree on the IR type they push; the condition must lower to
/// `IrType::Bool`.
pub(super) fn lower_ternary(
    cond: &Node,
    then: &Node,
    els: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Lower the condition in the outer tstack so a body like
    // `(a > 0) ? ... : ...` accurately reports its Bool result.
    lower_expr(&cond.expr, cond.range, ctx)?;
    let cond_ty = ctx.tstack.pop().ok_or(cap!(
        "lower_ternary.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Ternary(cond)".to_string(),
            range,
        }
    ))?;
    if cond_ty != IrType::Bool {
        return Err(cap!(
            "lower_ternary.if_condition_not_bool",
            LoweringError::IfConditionNotBool {
                got: cond_ty,
                range,
            }
        ));
    }
    // Lower each branch into its own sub-stream + isolated tstack so
    // an inner expression spilling extra values onto the stack is
    // caught here rather than leaking into the outer body. The
    // branch sub-ctx inherits the outer `lets` scope + counters so
    // a `let`-bound name remains visible inside the arm and any
    // const-literal index issued by the arm doesn't collide with
    // the outer module.
    let then_ops = lower_branch(then, range, ctx)?;
    let then_ty = then_ops.1;
    let then_body = then_ops.0;

    let else_ops = lower_branch(els, range, ctx)?;
    let else_ty = else_ops.1;
    let else_body = else_ops.0;

    if then_ty != else_ty {
        return Err(cap!(
            "lower_ternary.if_branch_type_mismatch",
            LoweringError::IfBranchTypeMismatch {
                then_ty,
                else_ty,
                range,
            }
        ));
    }
    let result_ty = then_ty;
    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty,
            then_body,
            else_body,
        },
        range,
    });
    ctx.tstack.push(result_ty);
    Ok(())
}

/// Lower a ternary when the surrounding expression already declares the
/// result type. This is required for enum variants such as
/// `flag ? Stat.Up : Stat.Down`, where `Stat.Up` only has meaning once
/// the expected enum type is known.
pub(super) fn lower_ternary_as_type(
    expected: &TypeRepr,
    cond: &Node,
    then: &Node,
    els: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_expr(&cond.expr, cond.range, ctx)?;
    let cond_ty = ctx.tstack.pop().ok_or(cap!(
        "lower_ternary_as_type.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Ternary(cond)".to_string(),
            range,
        }
    ))?;
    if cond_ty != IrType::Bool {
        return Err(cap!(
            "lower_ternary_as_type.if_condition_not_bool",
            LoweringError::IfConditionNotBool {
                got: cond_ty,
                range,
            }
        ));
    }

    let then_ops = lower_branch_as_type(expected, then, range, ctx)?;
    let then_ty = then_ops.1;
    let then_body = then_ops.0;

    let else_ops = lower_branch_as_type(expected, els, range, ctx)?;
    let else_ty = else_ops.1;
    let else_body = else_ops.0;

    if then_ty != else_ty {
        return Err(cap!(
            "lower_ternary_as_type.if_branch_type_mismatch",
            LoweringError::IfBranchTypeMismatch {
                then_ty,
                else_ty,
                range,
            }
        ));
    }

    let expected_ty = type_repr_to_ir_type_dict(expected)?;
    if then_ty != expected_ty {
        return Err(cap!(
            "lower_ternary_as_type.unsupported_expr.branch_type",
            LoweringError::UnsupportedExpr {
                kind: format!("Ternary(branch produced {then_ty:?}, expected {expected_ty:?})"),
                range,
            }
        ));
    }

    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty: then_ty,
            then_body,
            else_body,
        },
        range,
    });
    ctx.tstack.push(then_ty);
    Ok(())
}

/// Lower one branch of a ternary into a self-contained op stream +
/// the type it leaves on top of its private virtual stack.
///
/// The branch shares the outer `LowerCtx`'s let-local counter / const
/// indices so a const literal inside the branch picks up a unique
/// per-module index. Only the `out` stream is rerouted; the `tstack`
/// is replaced with a fresh one so the branch can be checked in
/// isolation.
pub(super) fn lower_branch(
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    lower_expr(&node.expr, node.range, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    if branch_stack.len() != 1 {
        return Err(cap!(
            "lower_branch.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("Ternary(branch-stack={})", branch_stack.len()),
                range,
            }
        ));
    }
    Ok((branch_ops, branch_stack[0]))
}

pub(super) fn lower_branch_as_type(
    expected: &TypeRepr,
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    lower_value_as_type(expected, node, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    if branch_stack.len() != 1 {
        return Err(cap!(
            "lower_branch_as_type.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("Ternary(branch-stack={})", branch_stack.len()),
                range,
            }
        ));
    }
    Ok((branch_ops, branch_stack[0]))
}

/// Map a parser comparison `Operator` to the matching IR op
/// constructor. Returns `None` for non-comparison ops.
pub(super) fn comparison_op_ctor(op: Operator) -> Option<fn(IrType) -> Op> {
    match op {
        Operator::Eq => Some(Op::Eq),
        Operator::Ne => Some(Op::Ne),
        Operator::Lt => Some(Op::Lt),
        Operator::Le => Some(Op::Le),
        Operator::Gt => Some(Op::Gt),
        Operator::Ge => Some(Op::Ge),
        _ => None,
    }
}

/// Map a parser `Operator` to the matching IR op constructor.
pub(super) fn arithmetic_op_ctor(op: Operator) -> Option<fn(IrType) -> Op> {
    match op {
        Operator::Add => Some(Op::Add),
        Operator::Sub => Some(Op::Sub),
        Operator::Mul => Some(Op::Mul),
        Operator::Div => Some(Op::Div),
        Operator::Mod => Some(Op::Mod),
        _ => None,
    }
}
