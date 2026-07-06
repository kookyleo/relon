//! Lowering sub-module: stdlib call arguments, schema-method receiver
//! dispatch, and closure-signature inference.
//!
//! Owns `lower_stdlib_arg` / `check_stdlib_arg` (buffer-protocol arg
//! marshalling), the method-receiver resolution +
//! `finish_schema_method_call` pair, and the closure-shape inference
//! helpers (`plan_anon_dict_closure_sig`, `infer_closure_body_ret_ty*`,
//! param-usage probes) that pick IR scalar types for closure params.

use super::*;

/// Pop the current vstack head and require it to be `I64`.  Used by
/// the `list.sum(range(...))` desugar to defend against the inner
/// argument exprs lowering to a non-i64 slot — analyzer typing should
/// have caught this earlier, but the desugar emits raw arithmetic so a
/// drift would silently corrupt subsequent ops.
pub(super) fn expect_int_top(ctx: &mut LowerCtx<'_>, range: TokenRange) -> Result<(), LoweringError> {
    match ctx.tstack.last().copied() {
        Some(IrType::I64) => Ok(()),
        Some(other) => Err(cap!(
            "expect_int_top.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "list.sum(range(...)) desugar requires Int args, got {:?}",
                    other
                ),
                range,
            }
        )),
        None => Err(cap!(
            "expect_int_top.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: "list.sum(range(...)) desugar saw empty vstack".to_string(),
                range,
            }
        )),
    }
}

/// Phase 10-a: lower one argument to a stdlib call, routing closure
/// expressions through [`lower_closure_as_value`] when the matching
/// param slot is `IrType::Closure`. Validates the resulting IR slot
/// against the callee's declared param type and surfaces a
/// `StdlibArgTypeMismatch` when the slots disagree.
pub(super) fn lower_stdlib_arg(
    name: &str,
    arg_idx: u32,
    value: &Node,
    param_tys: &[IrType],
    ctx: &mut LowerCtx<'_>,
    call_range: TokenRange,
) -> Result<(), LoweringError> {
    let expected = *param_tys.get(arg_idx as usize).ok_or_else(|| {
        cap!(
            "lower_stdlib_arg.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: name.to_string(),
                arity: param_tys.len() as u32,
                range: call_range,
            }
        )
    })?;
    if expected == IrType::Closure {
        // Closure surface: the value expression must be a literal
        // lambda. Any other shape (a Variable referencing a closure,
        // a stdlib-returned closure) is out of scope for Phase 10-a.
        if let Expr::Closure { .. } = &*value.expr {
            let (param_tys_c, ret_ty_c) =
                stdlib_closure_arg_signature(name, arg_idx).ok_or_else(|| {
                    cap!(
                        "lower_stdlib_arg.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "FnCall(`{}`) arg {} is Closure but no signature side-table entry",
                                name, arg_idx
                            ),
                            range: call_range,
                        }
                    )
                })?;
            lower_closure_as_value(&value.expr, value.range, &param_tys_c, ret_ty_c, ctx)?;
        } else {
            return Err(cap!(
                "lower_stdlib_arg.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "FnCall(`{}`) arg {} expected Closure literal, got `{}`",
                        name,
                        arg_idx,
                        value.expr.kind()
                    ),
                    range: value.range,
                }
            ));
        }
    } else {
        lower_expr(&value.expr, value.range, ctx)?;
    }
    let pushed = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_stdlib_arg.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!("FnCall(arg{}-stack-empty for `{}`)", arg_idx, name),
                range: call_range,
            }
        )
    })?;
    check_stdlib_arg(name, arg_idx, pushed, param_tys, call_range)?;
    ctx.tstack.push(pushed);
    Ok(())
}

/// Lower the receiver of a method-call into the top of the virtual
/// stack. Returns the schema brand of the receiver (the schema name
/// the analyzer would dispatch against) when one is statically
/// resolvable; `None` for scalar / String / List<Int> / sub-expression
/// receivers. The caller uses the brand to decide whether to route
/// through the schema-method registry or the stdlib method table.
pub(super) fn lower_method_receiver(
    receiver_segments: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<String>, LoweringError> {
    // Single-segment receivers: bare identifier (`s.length()`) or
    // parenthesised sub-expression (`("hi").length()`).
    if receiver_segments.len() == 1 {
        match &receiver_segments[0] {
            TokenKey::String(name, _, _) => {
                let brand = resolve_receiver_schema_brand(receiver_segments, ctx);
                lower_variable(receiver_segments, range, ctx)?;
                // Source-form check covers static `Schema.method(...)`
                // — when the head names a schema directly, the brand
                // returned above is the schema name; no extra work.
                let _ = name;
                return Ok(brand);
            }
            TokenKey::Dynamic(node, _) => {
                lower_expr(&node.expr, node.range, ctx)?;
                return Ok(None);
            }
            _ => {
                return Err(cap!(
                    "lower_method_receiver.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: "FnCall(unsupported-receiver-key)".to_string(),
                        range,
                    }
                ));
            }
        }
    }
    // Multi-segment receivers (`obj.sub.method()` → segments=[obj, sub]).
    // We route through `lower_variable` so the chained field walk +
    // schema-brand inheritance kick in. The brand of the final field
    // segment is the receiver brand; resolving it stays a static walk
    // so the caller can re-key the registry without extra runtime
    // information.
    if receiver_segments
        .iter()
        .all(|seg| matches!(seg, TokenKey::String(_, _, _)))
    {
        let brand = resolve_receiver_schema_brand(receiver_segments, ctx);
        lower_variable(receiver_segments, range, ctx)?;
        return Ok(brand);
    }
    Err(cap!(
        "lower_method_receiver.unsupported_expr.2",
        LoweringError::UnsupportedExpr {
            kind: format!(
                "FnCall(multi-segment-receiver, segments={})",
                receiver_segments.len()
            ),
            range,
        }
    ))
}

/// Statically inspect the receiver path against the current lowering
/// context and return the schema brand that the receiver's tail
/// segment would carry. Used by [`lower_method_receiver`] to decide
/// whether `obj.method()` should hit the schema-method registry.
/// Returns `None` when the receiver is not statically schema-typed.
pub(super) fn resolve_receiver_schema_brand(
    receiver_segments: &[TokenKey],
    ctx: &LowerCtx<'_>,
) -> Option<String> {
    let head = match receiver_segments.first()? {
        TokenKey::String(s, _, _) => s.as_str(),
        _ => return None,
    };
    // Determine the head's schema shape (if any) from the in-scope
    // bindings: `self`, then let-bindings (innermost first), then
    // method params, then `#main` params, then a static schema name.
    let mut current_schema: Option<Schema> = if let Some(self_b) = ctx.self_binding.as_ref() {
        if head == "self" {
            Some(self_b.schema.clone())
        } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
            b.schema_brand
                .as_deref()
                .and_then(|n| ctx.schema_resolver.resolve(n))
                .and_then(|def| {
                    let mut stack: Vec<&str> = Vec::new();
                    canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, def.range).ok()
                })
        } else if let Some(p) = ctx.method_params.iter().find(|p| p.name == head) {
            p.schema.clone()
        } else {
            None
        }
    } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
        b.schema_brand
            .as_deref()
            .and_then(|n| ctx.schema_resolver.resolve(n))
            .and_then(|def| {
                let mut stack: Vec<&str> = Vec::new();
                canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, def.range).ok()
            })
    } else if let Some(p) = ctx.params.iter().find(|b| b.name == head) {
        p.schema.clone()
    } else {
        // Static `Schema.method(...)` form: the head names a schema
        // directly. The walker treats the schema name itself as the
        // brand; no `self` instance is on the stack so emitting the
        // call from this form would still need an instance — for
        // Phase 5 we leave this surface unsupported and return
        // `None`, falling through to stdlib dispatch.
        return None;
    };
    // Walk any chained field segments to find the brand of the final
    // segment.
    for seg in receiver_segments[1..].iter() {
        let TokenKey::String(name, _, _) = seg else {
            return None;
        };
        let schema = current_schema.take()?;
        let field = schema.fields.iter().find(|f| &f.name == name)?;
        current_schema = match &field.ty {
            TypeRepr::Schema { schema } => Some((**schema).clone()),
            _ => None,
        };
    }
    current_schema.map(|s| s.name)
}

/// Finish a schema-method `Op::Call` emit: validate / lower
/// non-receiver args, type-check each against the method's
/// signature, then emit the call op with the resolved fn_index.
/// Assumes the receiver is already on top of the vstack.
pub(super) fn finish_schema_method_call(
    fn_index: u32,
    param_tys: Vec<IrType>,
    ret_ty: IrType,
    args: &[relon_parser::CallArg],
    method_name: &str,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let arity = param_tys.len() as u32;
    let expected_args = arity.saturating_sub(1) as usize;
    if args.len() != expected_args {
        return Err(cap!(
            "finish_schema_method_call.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        ));
    }
    // Validate the receiver slot against param[0].
    let pushed_receiver = ctx.tstack.pop().ok_or(cap!(
        "finish_schema_method_call.unsupported_expr.1",
        LoweringError::UnsupportedExpr {
            kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
            range,
        }
    ))?;
    if pushed_receiver.wasm_slot() != param_tys[0].wasm_slot() {
        return Err(cap!(
            "finish_schema_method_call.stdlib_arg_type_mismatch.1",
            LoweringError::StdlibArgTypeMismatch {
                name: method_name.to_string(),
                arg_idx: 0,
                got: pushed_receiver,
                expected: param_tys[0],
                range,
            }
        ));
    }
    ctx.tstack.push(pushed_receiver);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(cap!(
                "finish_schema_method_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{}` for schema method)", method_name),
                    range,
                }
            ));
        }
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let pushed = ctx.tstack.pop().ok_or(cap!(
            "finish_schema_method_call.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!("FnCall(arg{}-stack-empty for `{}`)", i + 1, method_name),
                range,
            }
        ))?;
        let expected = param_tys[i + 1];
        if pushed.wasm_slot() != expected.wasm_slot() {
            return Err(cap!(
                "finish_schema_method_call.stdlib_arg_type_mismatch.2",
                LoweringError::StdlibArgTypeMismatch {
                    name: method_name.to_string(),
                    arg_idx: (i + 1) as u32,
                    got: pushed,
                    expected,
                    range,
                }
            ));
        }
        ctx.tstack.push(pushed);
    }
    for _ in 0..arity {
        ctx.tstack.pop();
    }
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index,
            arg_count: arity,
            param_tys: param_tys.clone(),
            ret_ty,
        },
        range,
    });
    ctx.tstack.push(ret_ty);
    Ok(())
}

/// Validate a single argument's IR type against the stdlib
/// function's declared signature. We compare the **wasm slot** rather
/// than the exact IR type so a `String` argument (i32 slot) doesn't
/// require the caller to disambiguate from other i32-shaped types
/// upstream — the analyzer already enforces stronger typing, and the
/// codegen layer treats slot equivalence as the bytecode-correctness
/// invariant.
pub(super) fn check_stdlib_arg(
    name: &str,
    arg_idx: u32,
    got: IrType,
    param_tys: &[IrType],
    range: TokenRange,
) -> Result<(), LoweringError> {
    let expected = *param_tys.get(arg_idx as usize).ok_or_else(|| {
        cap!(
            "check_stdlib_arg.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: name.to_string(),
                arity: param_tys.len() as u32,
                range,
            }
        )
    })?;
    if got.wasm_slot() != expected.wasm_slot() {
        return Err(cap!(
            "check_stdlib_arg.stdlib_arg_type_mismatch",
            LoweringError::StdlibArgTypeMismatch {
                name: name.to_string(),
                arg_idx,
                got,
                expected,
                range,
            }
        ));
    }
    Ok(())
}

/// Determine an anon-Dict-return field closure's `(param_tys, ret_ty)`
/// IR signature, reading the real type rather than defaulting both to
/// I64. The priority is:
///
///   1. **Explicit annotation.** A leading-type field form
///      (`String fmt(s): ...`) or a `-> Ret` arrow stamps the closure's
///      `return_type`; a `(p: T)` param annotation stamps the param's
///      `type_hint`. Both are honoured when present.
///   2. **Conservative body inference (String concat only).** When no
///      explicit return type is given and the body is an unambiguous
///      `String + String + ...` concatenation chain — a left-leaning
///      `+` spine whose leaves are all String literals or this closure's
///      own params, with at least one String-literal leaf to prove the
///      `+` is concat (not arithmetic) — every unannotated param that
///      appears as a leaf is typed `String` and the return type is
///      `String`. The decorated value reaches the body through such a
///      param (value-first desugar), so this is what lets a
///      `@fmt(...)`-style String-result decorator field compile. A
///      numeric `+` (no String leaf, e.g. the W7 `add(v, n): v + n`
///      helper) is NOT matched, so its bytes are unchanged.
///   3. **I64 default.** Anything else keeps the historical behaviour:
///      unannotated params and an unannotated return both default to
///      I64 (the W7 `fib` Int surface).
///
/// The returned `concat_coercible` mask marks the params that were
/// typed `String` **by the concat-body inference** (case 2) rather
/// than by an explicit annotation. The String-concat body shape
/// guarantees such a param is used *only* as a concat leaf, so a
/// call site may render a scalar argument (Int / Bool / Float) to
/// `String` before the call and produce byte-identical output to the
/// tree-walk oracle (which renders the value at the `+` inside the
/// body via the same `Display`). `try_lower_local_closure_call`
/// consults the mask to admit the `examples/pricing.relon`
/// Float-valued `@currency` shape; explicitly-annotated `String`
/// params are never coerced (the mask stays `false` for them).
pub(super) fn plan_anon_dict_closure_sig(
    params: &[ClosureParam],
    return_type: Option<&TypeNode>,
    body: &Expr,
) -> (Vec<IrType>, IrType, Vec<bool>) {
    // Explicit param annotations first (honoured when present; the
    // anon-Dict surface can't yet *write* one, but the read is correct
    // and forward-compatible). Unannotated params start as I64.
    let mut param_irts: Vec<IrType> = params
        .iter()
        .map(|p| {
            p.type_hint
                .as_ref()
                .and_then(type_node_to_canonical)
                .and_then(|r| type_repr_to_ir_type(&r).ok())
                .unwrap_or(IrType::I64)
        })
        .collect();

    let explicit_ret = return_type
        .and_then(type_node_to_canonical)
        .and_then(|r| type_repr_to_ir_type(&r).ok());

    // String-concat detection: collect the leaves of a left-leaning `+`
    // spine and require every leaf to be a String literal or one of this
    // closure's params, with at least one String literal present.
    if is_string_concat_body(body, params) {
        // Type every unannotated param that the body uses as a concat
        // leaf as String; leave annotated params and unused params as
        // they were. The conservative walk only descends `+` nodes, so a
        // param appearing under a numeric subexpression won't reach here
        // (the whole body would already have been rejected).
        let mut concat_coercible = vec![false; params.len()];
        for (i, p) in params.iter().enumerate() {
            if p.type_hint.is_none() && string_concat_uses_param(body, &p.name) {
                param_irts[i] = IrType::String;
                concat_coercible[i] = true;
            }
        }
        // Honour an explicit String annotation; if the annotation
        // disagrees with the inferred String concat, fall back to the
        // explicit annotation (the user wrote it on purpose) — the
        // call-site / body type-check then surfaces any real conflict.
        let ret_ty = explicit_ret.unwrap_or(IrType::String);
        return (param_irts, ret_ty, concat_coercible);
    }

    // Not a String concat: keep the historical I64 default unless an
    // explicit return annotation says otherwise.
    let ret_ty = explicit_ret.unwrap_or(IrType::I64);
    let coercible = vec![false; params.len()];
    (param_irts, ret_ty, coercible)
}

/// True when `expr` is an unambiguous `String + String + ...` concat
/// chain: a left-leaning `Operator::Add` spine whose every leaf is a
/// String literal or a name in `params`, with at least one String
/// literal leaf (so a purely numeric `v + n` is rejected). Any other
/// leaf shape — a numeric literal, an arithmetic subexpression, a
/// non-param variable, a call — disqualifies the whole body.
pub(super) fn is_string_concat_body(expr: &Expr, params: &[ClosureParam]) -> bool {
    let mut saw_string_literal = false;
    if !string_concat_leaves_ok(expr, params, &mut saw_string_literal) {
        return false;
    }
    saw_string_literal
}

/// Recursive leaf check for [`is_string_concat_body`]; sets
/// `saw_string_literal` when a `String` literal leaf is encountered.
pub(super) fn string_concat_leaves_ok(
    expr: &Expr,
    params: &[ClosureParam],
    saw_string_literal: &mut bool,
) -> bool {
    match expr {
        Expr::Binary(Operator::Add, lhs, rhs) => {
            string_concat_leaves_ok(&lhs.expr, params, saw_string_literal)
                && string_concat_leaves_ok(&rhs.expr, params, saw_string_literal)
        }
        Expr::String(_) => {
            *saw_string_literal = true;
            true
        }
        Expr::Variable(path) => {
            matches!(path.as_slice(), [TokenKey::String(name, _, _)]
                if params.iter().any(|p| &p.name == name))
        }
        _ => false,
    }
}

/// True when `name` appears as a bare-variable leaf inside the `+` spine
/// of `expr`. Used to decide which unannotated params a String-concat
/// body forces to `String`.
pub(super) fn string_concat_uses_param(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Binary(Operator::Add, lhs, rhs) => {
            string_concat_uses_param(&lhs.expr, name) || string_concat_uses_param(&rhs.expr, name)
        }
        Expr::Variable(path) => {
            matches!(path.as_slice(), [TokenKey::String(n, _, _)] if n == name)
        }
        _ => false,
    }
}

/// AOT-4: infer the IR return type of a where-bound helper closure
/// from its body, used when no explicit `: Type` annotation is
/// present. The inference is intentionally narrow — it only needs to
/// distinguish `Bool` (W18 `is_prime`) from the `Int`/I64 default
/// (W17 `bs`):
///
///   * a `Bool` / comparison / logical expression -> `Bool`;
///   * a ternary -> the type of whichever branch is determinable
///     without recursing into the helper itself (so the recursive arm,
///     which has no fixed type yet, doesn't dominate the inference);
///   * everything else -> the `Int` (I64) default.
///
/// This keeps the self-recursive call's return type in agreement with
/// the sibling literal branches, which is what `lower_ternary`'s
/// `IfBranchTypeMismatch` check enforces.
pub(super) fn infer_closure_body_ret_ty(expr: &Expr) -> IrType {
    match expr {
        Expr::Bool(_) => IrType::Bool,
        Expr::Binary(op, _, _) if operator_yields_bool(*op) => IrType::Bool,
        Expr::Unary(Operator::Not, _) => IrType::Bool,
        Expr::Ternary { then, els, .. } => {
            // Prefer the branch that resolves to a concrete scalar
            // type — the recursive arm typically falls through to the
            // I64 default, so a definite Bool on either side wins.
            let then_ty = infer_closure_body_ret_ty(&then.expr);
            let else_ty = infer_closure_body_ret_ty(&els.expr);
            if then_ty == IrType::Bool || else_ty == IrType::Bool {
                IrType::Bool
            } else {
                then_ty
            }
        }
        _ => IrType::I64,
    }
}

/// #359 (W20): ctx-aware closure body return-type inference. Extends
/// [`infer_closure_body_ret_ty`] (Bool / Int) to also resolve `F64`
/// and `ListFloat`:
///
///   * a `[...]` list literal whose first element is Float-valued
///     (a Float literal, a list-typed param index, or Float arith)
///     -> `ListFloat` (W20 `step` returns an 8-element `List<Float>`);
///   * a Float literal / Float-typed where-binding / index into a
///     `ListFloat` param / a call into a sibling closure that returns
///     `F64` / arithmetic over any of those -> `F64` (W20 `pair_force`
///     and `accel`);
///   * everything else falls back to the structural Bool / Int walk.
///
/// `param_irts` / `params` describe THIS closure's own params (so an
/// indexed `s[k]` on a `ListFloat` param resolves to a Float element);
/// `ctx` supplies sibling closure signatures + outer where-bound let
/// types.
pub(super) fn infer_closure_body_ret_ty_ctx(
    expr: &Expr,
    param_irts: &[IrType],
    params: &[ClosureParam],
    ctx: &LowerCtx<'_>,
) -> IrType {
    // List literal -> ListInt / ListFloat based on the first element.
    if let Expr::List(items) = expr {
        if let Some(first) = items.first() {
            return match infer_scalar_expr_ir_ty(&first.expr, param_irts, params, ctx) {
                Some(IrType::F64) => IrType::ListFloat,
                Some(IrType::I64) => IrType::ListInt,
                _ => IrType::ListInt,
            };
        }
        return IrType::ListInt;
    }
    // Scalar / Float resolution first (so a Float ternary like
    // `i == j ? 0.0 : <float arith>` reports F64 rather than the
    // structural Int default).
    if let Some(t) = infer_scalar_expr_ir_ty(expr, param_irts, params, ctx) {
        if t == IrType::F64 {
            return IrType::F64;
        }
    }
    // Fall back to the original structural Bool / Int walk.
    infer_closure_body_ret_ty(expr)
}

/// #359 (W20): best-effort scalar IR-type inference for a closure body
/// sub-expression, resolving `F64` vs `I64` (other shapes return
/// `None`). Used by the ctx-aware return-type / list-literal inference.
/// Conservative: only commits when the shape is unambiguous.
pub(super) fn infer_scalar_expr_ir_ty(
    expr: &Expr,
    param_irts: &[IrType],
    params: &[ClosureParam],
    ctx: &LowerCtx<'_>,
) -> Option<IrType> {
    match expr {
        Expr::Float(_) => Some(IrType::F64),
        Expr::Int(_) => Some(IrType::I64),
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            // Bare name: a where-bound let or a sibling param.
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(b) = ctx.lets.iter().rev().find(|b| &b.name == name) {
                    return scalar_of(b.ty);
                }
                if let Some(pos) = params.iter().position(|p| &p.name == name) {
                    return scalar_of(param_irts[pos]);
                }
                return None;
            }
            // `s[k]` index on a list-typed param -> element scalar type.
            if path.len() == 2 {
                if let (TokenKey::String(name, _, _), TokenKey::Dynamic(_, _)) =
                    (&path[0], &path[1])
                {
                    if let Some(pos) = params.iter().position(|p| &p.name == name) {
                        return match param_irts[pos] {
                            IrType::ListFloat => Some(IrType::F64),
                            IrType::ListInt => Some(IrType::I64),
                            _ => None,
                        };
                    }
                }
            }
            None
        }
        Expr::FnCall { path, .. } => {
            // A call into a sibling where-bound closure adopts its
            // declared return type.
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(b) = ctx.lets.iter().rev().find(|b| &b.name == name) {
                    if b.ty == IrType::Closure {
                        if let Some((_, ret)) = ctx.closure_let_signatures.get(&b.idx) {
                            return scalar_of(*ret);
                        }
                    }
                }
            }
            None
        }
        Expr::Binary(op, l, r) => {
            if operator_yields_bool(*op) {
                return Some(IrType::Bool);
            }
            let lt = infer_scalar_expr_ir_ty(&l.expr, param_irts, params, ctx);
            let rt = infer_scalar_expr_ir_ty(&r.expr, param_irts, params, ctx);
            match (lt, rt) {
                // Float dominates (mirrors the runtime Int->Float
                // promotion the Part A `ConvertI64ToF64` op implements).
                (Some(IrType::F64), _) | (_, Some(IrType::F64)) => Some(IrType::F64),
                (Some(IrType::I64), Some(IrType::I64)) => Some(IrType::I64),
                _ => None,
            }
        }
        Expr::Ternary { then, els, .. } => {
            let tt = infer_scalar_expr_ir_ty(&then.expr, param_irts, params, ctx);
            let et = infer_scalar_expr_ir_ty(&els.expr, param_irts, params, ctx);
            match (tt, et) {
                (Some(IrType::F64), _) | (_, Some(IrType::F64)) => Some(IrType::F64),
                (Some(IrType::I64), Some(IrType::I64)) => Some(IrType::I64),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Narrow an [`IrType`] to its scalar `F64` / `I64` form for the
/// inference helpers; other shapes are `None` (not scalar-relevant).
pub(super) fn scalar_of(t: IrType) -> Option<IrType> {
    match t {
        IrType::F64 => Some(IrType::F64),
        IrType::I64 => Some(IrType::I64),
        _ => None,
    }
}

/// #359 (W20): infer a closure param's IR type from how a sibling
/// where-bound closure call uses it. When the body contains a call
/// `f(.., name, ..)` where `f` is a previously-bound closure whose
/// declared param at that position is a List* / scalar type, the
/// param `name` adopts that type. Drives `accel(s, i)`'s `s` to
/// `ListFloat` (it passes `s` as `pair_force`'s first arg without ever
/// indexing it directly). Returns `None` when no such call pins it.
pub(super) fn infer_param_from_sibling_call(name: &str, body: &Expr, ctx: &LowerCtx<'_>) -> Option<IrType> {
    fn walk(name: &str, expr: &Expr, ctx: &LowerCtx<'_>) -> Option<IrType> {
        match expr {
            Expr::FnCall { path, args } => {
                if let [TokenKey::String(callee, _, _)] = path.as_slice() {
                    if let Some(b) = ctx.lets.iter().rev().find(|b| &b.name == callee) {
                        if b.ty == IrType::Closure {
                            if let Some((param_tys, _)) = ctx.closure_let_signatures.get(&b.idx) {
                                for (i, a) in args.iter().enumerate() {
                                    if a.name.is_none() && expr_is_bare_named(&a.value.expr, name) {
                                        if let Some(t) = param_tys.get(i) {
                                            return Some(*t);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Recurse into args for nested calls.
                args.iter().find_map(|a| walk(name, &a.value.expr, ctx))
            }
            Expr::Binary(_, l, r) => walk(name, &l.expr, ctx).or_else(|| walk(name, &r.expr, ctx)),
            Expr::Unary(_, n) => walk(name, &n.expr, ctx),
            Expr::Ternary { cond, then, els } => walk(name, &cond.expr, ctx)
                .or_else(|| walk(name, &then.expr, ctx))
                .or_else(|| walk(name, &els.expr, ctx)),
            Expr::List(items) => items.iter().find_map(|n| walk(name, &n.expr, ctx)),
            _ => None,
        }
    }
    walk(name, body, ctx)
}

/// `true` when `expr` is the bare variable `name` (a single-segment
/// `Variable([String(name)])`).
pub(super) fn expr_is_bare_named(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Variable(path)
        if path.len() == 1
            && matches!(&path[0], TokenKey::String(s, _, _) if s == name))
}

/// #359 (W20): infer that a closure param named `name` is a
/// `List<Float>` from how the body uses it: an index access
/// `name[...]` whose result flows into Float arithmetic, OR the body
/// otherwise pairs the index with a Float literal / Float where-
/// binding. Conservative — only fires when the body contains a Float
/// literal alongside an index on `name`, so a `List<Int>` param (whose
/// body is pure-Int) is never mis-inferred. This runs only after
/// [`infer_param_from_sibling_call`] declines, so the precise
/// propagation always wins.
pub(super) fn closure_param_used_as_list_float(name: &str, expr: &Expr) -> bool {
    closure_param_used_as_list_int(name, expr) && expr_contains_float_literal(expr)
}

/// #359 (W20): infer that a scalar closure param named `name` is a
/// `Float` from its use as a bare operand in a Float-shaped arithmetic
/// expression. Drives `pair_force`'s `mj` mass param (used in
/// `(s[j] - s[i]) * mj * (1.0 / ..)`) to `F64`. Conservative: fires only
/// when `name` appears as a direct (non-bool) Binary operand AND the
/// closure body somewhere carries a Float literal — a pure-Int scalar
/// param (e.g. `pair_force`'s `i` / `j` index args, which only appear
/// inside `s[..]` brackets and `i == j` comparisons, never as a bare
/// arithmetic operand) is never mis-tagged.
pub(super) fn closure_param_used_as_float(name: &str, body: &Expr) -> bool {
    expr_contains_float_literal(body) && param_is_bare_arith_operand(name, body)
}

/// `true` when `name` appears as a direct operand of a non-bool Binary
/// (or Unary) arithmetic op anywhere in `expr`.
pub(super) fn param_is_bare_arith_operand(name: &str, expr: &Expr) -> bool {
    match expr {
        Expr::Binary(op, l, r) => {
            if !operator_yields_bool(*op)
                && (expr_is_bare_named(&l.expr, name) || expr_is_bare_named(&r.expr, name))
            {
                return true;
            }
            param_is_bare_arith_operand(name, &l.expr) || param_is_bare_arith_operand(name, &r.expr)
        }
        Expr::Unary(op, n) => {
            (!matches!(op, Operator::Not) && expr_is_bare_named(&n.expr, name))
                || param_is_bare_arith_operand(name, &n.expr)
        }
        Expr::Ternary { cond, then, els } => {
            param_is_bare_arith_operand(name, &cond.expr)
                || param_is_bare_arith_operand(name, &then.expr)
                || param_is_bare_arith_operand(name, &els.expr)
        }
        Expr::List(items) => items
            .iter()
            .any(|n| param_is_bare_arith_operand(name, &n.expr)),
        Expr::FnCall { args, .. } => args
            .iter()
            .any(|a| param_is_bare_arith_operand(name, &a.value.expr)),
        _ => false,
    }
}

/// `true` when `expr` contains a Float literal anywhere in its tree.
pub(super) fn expr_contains_float_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Float(_) => true,
        Expr::Binary(_, l, r) => {
            expr_contains_float_literal(&l.expr) || expr_contains_float_literal(&r.expr)
        }
        Expr::Unary(_, n) => expr_contains_float_literal(&n.expr),
        Expr::Ternary { cond, then, els } => {
            expr_contains_float_literal(&cond.expr)
                || expr_contains_float_literal(&then.expr)
                || expr_contains_float_literal(&els.expr)
        }
        Expr::List(items) => items.iter().any(|n| expr_contains_float_literal(&n.expr)),
        Expr::FnCall { args, .. } => args
            .iter()
            .any(|a| expr_contains_float_literal(&a.value.expr)),
        _ => false,
    }
}

/// AOT-4 (W16 slice): infer that a closure param named `name` is a
/// `List<Int>` from how the body uses it. Returns `true` when the body
/// contains an index access `name[...]`, a `_len(name)` / `len(name)`,
/// or a `_list_filter(name, ...)` whose first argument is the bare
/// param. The walk is purely structural (no type-checking) and
/// conservative — it only fires on uses that are unambiguously
/// list-shaped, so a scalar param is never mis-inferred.
pub(super) fn closure_param_used_as_list_int(name: &str, expr: &Expr) -> bool {
    fn expr_is_bare_param(expr: &Expr, name: &str) -> bool {
        matches!(expr, Expr::Variable(path)
            if path.len() == 1
                && matches!(&path[0], TokenKey::String(s, _, _) if s == name))
    }
    match expr {
        // `name[...]` -> `Variable([String(name), Dynamic(idx)])`.
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            path.len() == 2
                && matches!(&path[0], TokenKey::String(s, _, _) if s == name)
                && matches!(&path[1], TokenKey::Dynamic(_, _))
        }
        Expr::FnCall { path, args } => {
            // `_len(name)` / `len(name)` / `_list_filter(name, ...)`.
            let head_list_intrinsic = path.len() == 1
                && matches!(&path[0], TokenKey::String(s, _, _)
                    if s == "_len" || s == "len" || s == "_list_filter");
            if head_list_intrinsic
                && args
                    .first()
                    .is_some_and(|a| expr_is_bare_param(&a.value.expr, name))
            {
                return true;
            }
            // AOT-4 (W19 slice): a method call whose receiver is the bare
            // param — `name.reduce(...)` / `name.sum()` / `name.map(...)`
            // / `name.fold(...)` / `name.length()` — is a list use. A
            // bare-variable receiver parses as `[String(name),
            // String(method)]`; a complex-expression receiver parses as
            // `[Dynamic(<receiver>), String(method)]`. Cover both.
            if path.len() == 2 {
                if let TokenKey::String(m, _, _) = &path[1] {
                    let is_list_method = matches!(
                        m.as_str(),
                        "reduce" | "sum" | "map" | "fold" | "filter" | "length" | "max"
                    );
                    let recv_is_param = match &path[0] {
                        TokenKey::String(s, _, _) => s == name,
                        TokenKey::Dynamic(recv, _) => expr_is_bare_param(&recv.expr, name),
                        _ => false,
                    };
                    if is_list_method && recv_is_param {
                        return true;
                    }
                }
            }
            args.iter()
                .any(|a| closure_param_used_as_list_int(name, &a.value.expr))
        }
        Expr::Binary(_, l, r) => {
            closure_param_used_as_list_int(name, &l.expr)
                || closure_param_used_as_list_int(name, &r.expr)
        }
        Expr::Unary(_, n) => closure_param_used_as_list_int(name, &n.expr),
        Expr::Ternary { cond, then, els } => {
            closure_param_used_as_list_int(name, &cond.expr)
                || closure_param_used_as_list_int(name, &then.expr)
                || closure_param_used_as_list_int(name, &els.expr)
        }
        _ => false,
    }
}

/// `true` for the comparison + logical operators whose IR lowering
/// leaves a `Bool` on the operand stack.
pub(super) fn operator_yields_bool(op: Operator) -> bool {
    matches!(
        op,
        Operator::Eq
            | Operator::Ne
            | Operator::Lt
            | Operator::Gt
            | Operator::Le
            | Operator::Ge
            | Operator::And
            | Operator::Or
    )
}
