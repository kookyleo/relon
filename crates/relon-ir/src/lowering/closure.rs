//! Phase 10-a closure-conversion helpers split out of `lowering`.
//!
//! Pure relocation of the original `lowering.rs` bodies (no behaviour
//! change); back-references into the parent module resolve through `super`.

use super::*;

// =====================================================================
// Phase 10-a: closure-conversion helpers.
// =====================================================================

/// Walk a lambda's body expression and collect identifiers that
/// reference a name not bound by the lambda's own param list. The
/// scan is heuristic — it counts every bare-identifier head segment
/// once and treats it as a potential free variable; spurious entries
/// (names that don't actually resolve in the enclosing scope) are
/// filtered out later by [`resolve_capture`]. The lambda's own params
/// are excluded so they don't pollute the captures list.
fn collect_free_vars(expr: &Expr, lambda_params: &[ClosureParam]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut visit = |s: &str| {
        if lambda_params.iter().any(|p| p.name == s) {
            return;
        }
        if seen.insert(s.to_string()) {
            found.push(s.to_string());
        }
    };
    fn walk_expr(expr: &Expr, lambda_params: &[ClosureParam], visit: &mut dyn FnMut(&str)) {
        match expr {
            Expr::Variable(path) | Expr::Reference { path, .. } => {
                if let Some(TokenKey::String(name, _, _)) = path.first() {
                    visit(name);
                }
            }
            Expr::Binary(_, a, b) => {
                walk_expr(&a.expr, lambda_params, visit);
                walk_expr(&b.expr, lambda_params, visit);
            }
            Expr::Unary(_, n) => walk_expr(&n.expr, lambda_params, visit),
            Expr::Ternary { cond, then, els } => {
                walk_expr(&cond.expr, lambda_params, visit);
                walk_expr(&then.expr, lambda_params, visit);
                walk_expr(&els.expr, lambda_params, visit);
            }
            Expr::List(items) => {
                for n in items {
                    walk_expr(&n.expr, lambda_params, visit);
                }
            }
            Expr::Dict(pairs) => {
                for (_, v) in pairs {
                    walk_expr(&v.expr, lambda_params, visit);
                }
            }
            Expr::Where { expr, bindings } => {
                walk_expr(&bindings.expr, lambda_params, visit);
                walk_expr(&expr.expr, lambda_params, visit);
            }
            Expr::FnCall { path, args } => {
                // Method-call form (`xs.length()`) carries the
                // receiver in the path's leading segments — visit the
                // head as a potential free var. Free-call form
                // (`fib(n)`) also visits the head: in the Phase F.2
                // anon-Dict-return path the head may resolve to a
                // closure-typed let-binding (the lifted dict field);
                // `resolve_capture` filters non-binding names out
                // (stdlib free calls like `range(...)` return
                // `Ok(None)` and never become captures).
                if let Some(TokenKey::String(name, _, _)) = path.first() {
                    visit(name);
                }
                for a in args {
                    walk_expr(&a.value.expr, lambda_params, visit);
                }
            }
            Expr::Closure { body, params, .. } => {
                // Nested lambda: the inner lambda's own params shadow
                // outer ones, but anything else escapes.
                let mut combined: Vec<ClosureParam> = lambda_params.to_vec();
                combined.extend(params.iter().cloned());
                walk_expr(&body.expr, &combined, visit);
            }
            _ => {}
        }
    }
    walk_expr(expr, lambda_params, &mut visit);
    found
}

/// Find a name in the enclosing scope and return its `(IrType,
/// outer_let_idx)`. If the binding is currently a `#main` /
/// method-param (i.e. not yet in a let-local), the helper materialises
/// a fresh let-local, emits a `LetSet` that captures the value, and
/// returns the new index — so a captured `#main` param participates
/// in the closure capture protocol just like any user-let.
fn resolve_capture(
    name: &str,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<(IrType, u32)>, LoweringError> {
    // Innermost-first: let-bindings shadow params.
    if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == name).cloned() {
        return Ok(Some((b.ty, b.idx)));
    }
    // Method params / `#main` params — lift the value into a fresh
    // let-local so the capture protocol has a uniform source.
    if let Some(p) = ctx.method_params.iter().find(|p| p.name == name).cloned() {
        ctx.out.push(TaggedOp {
            op: Op::LocalGet(p.wasm_local_idx),
            range,
        });
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: p.ty },
            range,
        });
        ctx.lets.push(LetBinding {
            name: name.to_string(),
            idx,
            ty: p.ty,
            schema_brand: None,
            type_repr: None,
        });
        return Ok(Some((p.ty, idx)));
    }
    if let Some(p) = ctx.params.iter().find(|p| p.name == name).cloned() {
        // For scalar / pointer params, emit a `LoadField` + `LetSet`.
        // Schema-typed `#main` params are intentionally NOT captureable
        // by Phase 10-a — closure values cannot carry the analyzer's
        // brand machinery yet.
        if p.schema_brand.is_some() {
            return Err(cap!(
                "resolve_capture.unsupported_closure_capture",
                LoweringError::UnsupportedClosureCapture {
                    name: name.to_string(),
                    ty: p.ty,
                    range,
                }
            ));
        }
        // Use the matching load shape for the param's IR type.
        let load_op = match p.ty {
            IrType::String => Op::LoadStringPtr { offset: p.offset },
            IrType::ListInt => Op::LoadListIntPtr { offset: p.offset },
            IrType::ListFloat => Op::LoadListFloatPtr { offset: p.offset },
            IrType::ListBool => Op::LoadListBoolPtr { offset: p.offset },
            IrType::ListString => Op::LoadListStringPtr { offset: p.offset },
            IrType::ListSchema => Op::LoadListSchemaPtr { offset: p.offset },
            IrType::ListList => Op::LoadListListPtr { offset: p.offset },
            other => Op::LoadField {
                offset: p.offset,
                ty: other,
            },
        };
        ctx.out.push(TaggedOp { op: load_op, range });
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: p.ty },
            range,
        });
        ctx.lets.push(LetBinding {
            name: name.to_string(),
            idx,
            ty: p.ty,
            schema_brand: None,
            type_repr: None,
        });
        return Ok(Some((p.ty, idx)));
    }
    // Not in scope — the name might refer to a parser-level construct
    // (e.g. a stdlib free-call head, a schema name) that doesn't
    // contribute a capture. Let the lambda body's own
    // `lower_variable` handle the diagnostic later.
    Ok(None)
}

/// Lay out a captures struct: place 8-aligned fields first, then 4-/
/// 1-byte fields. Returns the per-field byte offsets in the same
/// order as the input plus the total struct size (rounded up to 8).
fn layout_captures(captures: &[(String, IrType, u32)]) -> (Vec<u32>, u32) {
    // Two passes: 8-byte slots, then everything else. Keeps the
    // total size aligned at 8 without complex packing logic.
    let mut offsets = vec![0u32; captures.len()];
    let mut cursor: u32 = 0;
    for (i, (_, ty, _)) in captures.iter().enumerate() {
        if matches!(ty.wasm_slot(), IrType::I64 | IrType::F64)
            || matches!(ty, IrType::I64 | IrType::F64)
        {
            offsets[i] = cursor;
            cursor += 8;
        }
    }
    for (i, (_, ty, _)) in captures.iter().enumerate() {
        if !matches!(ty, IrType::I64 | IrType::F64) {
            offsets[i] = cursor;
            cursor += 4;
        }
    }
    // Round up the total size to 8 so the next scratch alloc starts
    // at an 8-aligned boundary.
    let total = (cursor + 7) & !7u32;
    (offsets, total)
}

/// Phase 10-a: lower one [`Expr::Closure`] literal and emit a
/// `MakeClosure` op leaving an `IrType::Closure` value on top of the
/// vstack. The lambda's body becomes a fresh `Func` appended to
/// `ctx.lambda_funcs`; its wasm-side function index is communicated
/// to `MakeClosure` via the closure-table slot `lambda_funcs.len() - 1`.
///
/// `expected_param_tys` and `expected_ret_ty` describe the surface
/// the consumer requires from the closure body:
///
/// * **Higher-order stdlib arg** (Phase 10-a — the original entry
///   point this helper served). The stdlib side-table
///   ([`stdlib_closure_arg_signature`]) provides the expected
///   signature for `list_int_map` / `filter` / `fold`. Reached from
///   [`lower_stdlib_arg`].
/// * **Closure-as-value at a dict-field / let-binding site**
///   (Phase F.2 / Phase C scope, design doc
///   `docs/internal/w7-closure-as-value-design.md`). The caller
///   supplies the expected signature from the field's declared /
///   inferred [`TypeRepr::Closure`] shape — see the W7 boundary tests
///   in `w7_closure_boundary_tests` for the production source the
///   future Phase C lowering must accept.
///
/// Mismatches between the expected signature and the inferred body
/// type surface as [`LoweringError::StdlibArgTypeMismatch`] (the same
/// diagnostic both surfaces share — the closure body is the "argument"
/// of the consumer's typed slot).
pub(super) fn lower_closure_as_value(
    closure_expr: &Expr,
    closure_range: TokenRange,
    expected_param_tys: &[IrType],
    expected_ret_ty: IrType,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_closure_as_value_with_expected_type(
        closure_expr,
        closure_range,
        expected_param_tys,
        expected_ret_ty,
        None,
        None,
        ctx,
    )
}

pub(super) fn lower_closure_as_value_with_expected_type(
    closure_expr: &Expr,
    closure_range: TokenRange,
    expected_param_tys: &[IrType],
    expected_ret_ty: IrType,
    expected_param_reprs: Option<&[Option<&TypeRepr>]>,
    expected_ret_repr: Option<&TypeRepr>,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let Expr::Closure {
        params: lambda_params,
        body: lambda_body,
        ..
    } = closure_expr
    else {
        return Err(cap!(
            "lower_closure_as_value.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "lower_closure_as_value(non-closure `{}`)",
                    closure_expr.kind()
                ),
                range: closure_range,
            }
        ));
    };
    if lambda_params.len() != expected_param_tys.len() {
        return Err(cap!(
            "lower_closure_as_value.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Closure(arity-mismatch: expected {}, got {})",
                    expected_param_tys.len(),
                    lambda_params.len()
                ),
                range: closure_range,
            }
        ));
    }

    // -----------------------------------------------------------------
    // Free-var analysis + capture resolution.
    // -----------------------------------------------------------------
    let free_vars = collect_free_vars(&lambda_body.expr, lambda_params);
    let mut resolved: Vec<(String, IrType, u32)> = Vec::new();
    // #359 (W20 container perf): free vars that resolve to a where-bound
    // scalar literal (`soft` / `dt` / masses) are INLINED into the lambda
    // body as `Op::Const*` rather than captured through the arena captures
    // struct (an opaque load LLVM's `-O3` cannot fold). This keeps
    // `dx*dx + soft` a compile-time-constant expression inside
    // `pair_force` so the optimizer folds the inner-loop arithmetic. The
    // constant is the exact source literal, so the body computes a
    // bit-identical value.
    let mut inlined_consts: Vec<(String, IrType, ScalarConst)> = Vec::new();
    for name in free_vars {
        if let Some((ty, outer_idx)) = resolve_capture(&name, lambda_body.range, ctx)? {
            if let Some(sc) = ctx.const_let_values.get(&outer_idx).copied() {
                inlined_consts.push((name, ty, sc));
            } else {
                resolved.push((name, ty, outer_idx));
            }
        }
    }
    let (offsets, captures_size) = layout_captures(&resolved);
    let captures: Vec<ClosureCapture> = resolved
        .iter()
        .zip(offsets.iter())
        .map(|((_, ty, let_idx), offset)| ClosureCapture {
            let_idx: *let_idx,
            ty: *ty,
            offset: *offset,
        })
        .collect();

    // -----------------------------------------------------------------
    // Build the lambda Func.
    //
    // Signature: `(captures_ptr: i32, ...lambda_params) -> ret_ty`.
    // Body prologue: for each capture, emit `LocalGet(0);
    // LoadXxxAtAbsolute { offset }; LetSet { fresh_idx, ty }`. The
    // lambda body's lowering ctx sees each capture as a let-binding
    // under its source-level name; the body lowers normally.
    // -----------------------------------------------------------------
    let mut lambda_param_tys: Vec<IrType> = Vec::with_capacity(1 + expected_param_tys.len());
    lambda_param_tys.push(IrType::I32);
    lambda_param_tys.extend(expected_param_tys.iter().copied());

    // AOT-4 fix: reserve THIS lambda's global closure-table slot BEFORE
    // lowering its body, so any lambda created inside the body (e.g. the
    // W16 filter predicate built inside the recursive `sum_qs` helper)
    // reserves a strictly-later slot. The reserved slot is the
    // `fn_table_idx` the outer `MakeClosure` will reference; the body is
    // lowered next, then the built Func is dropped into the slot.
    let fn_table_idx = ctx.reserve_lambda_slot();

    // Use a fresh LowerCtx — captures + lambda params become its let
    // bindings. Cloning the schema resolver / method registry is a
    // cheap re-use of the outer-side shared maps; the inner walk
    // never mutates them. #151 — share the outer ctx's intern handle
    // so any literal lowered inside the lambda body participates in
    // the module-wide dedup table. AOT-4 — share the module-wide lambda
    // slot table so a nested lambda's `fn_table_idx` is a global slot.
    const EMPTY_PARAMS: &[LocalBinding] = &[];
    let mut inner = LowerCtx::new(
        EMPTY_PARAMS,
        ctx.schema_resolver.clone(),
        ctx.method_registry.clone(),
        ctx.intern_handle(),
        ctx.native_imports_handle(),
    );
    inner.lambda_table = ctx.lambda_table_handle();
    inner.variant_records_in_scratch = true;

    // Prologue: load each capture into a fresh inner let-local.
    let mut inner_let_idx: u32 = 0;
    for ((name, ty, outer_idx), offset) in resolved.iter().zip(offsets.iter()) {
        // Push captures_ptr (local 0), then emit the type-driven load
        // followed by a LetSet under the source-level name.
        inner.out.push(TaggedOp {
            op: Op::LocalGet(0),
            range: lambda_body.range,
        });
        match ty {
            IrType::I64 => inner.out.push(TaggedOp {
                op: Op::LoadI64AtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
            IrType::F64 => inner.out.push(TaggedOp {
                op: Op::LoadF64AtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
            IrType::Bool => inner.out.push(TaggedOp {
                op: Op::LoadI8UAtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
            IrType::I32
            | IrType::Unit
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::ListList
            | IrType::Closure
            | IrType::Dict => inner.out.push(TaggedOp {
                op: Op::LoadI32AtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
        }
        inner.out.push(TaggedOp {
            op: Op::LetSet {
                idx: inner_let_idx,
                ty: *ty,
            },
            range: lambda_body.range,
        });
        inner.lets.push(LetBinding {
            name: name.clone(),
            idx: inner_let_idx,
            ty: *ty,
            schema_brand: None,
            type_repr: None,
        });
        // Phase F.2 (W7 anon-Dict-return): when a captured value is
        // a closure handle, propagate its signature into the inner
        // ctx's side-table so a recursive self-call inside the body
        // (which sees the capture at `inner_let_idx`) resolves
        // through `try_lower_local_closure_call`. Without this hop
        // the inner ctx would see an `IrType::Closure` let but no
        // signature, leading to a missing-side-table error.
        if matches!(*ty, IrType::Closure) {
            if let Some(sig) = ctx.closure_let_signatures.get(outer_idx).cloned() {
                inner.closure_let_signatures.insert(inner_let_idx, sig);
            }
        }
        inner_let_idx += 1;
    }
    // #359 (W20 container perf): inlined scalar constants. Rather than
    // capturing the value through the arena captures struct (an opaque
    // load LLVM can't fold), register a fresh inner let-local that maps
    // to the compile-time constant in `inner.const_let_values`. The body
    // never emits a `LetGet` for it — `lower_variable`'s bare-const path
    // resolves each reference straight to an `Op::Const*`, keeping the
    // value a compile-time constant the optimizer can fold into the
    // inner-loop arithmetic. The let-binding exists only so the name
    // resolves to this idx; no `LetSet` / load is emitted.
    for (name, ty, sc) in inlined_consts.iter() {
        inner.lets.push(LetBinding {
            name: name.clone(),
            idx: inner_let_idx,
            ty: *ty,
            schema_brand: None,
            type_repr: None,
        });
        inner.const_let_values.insert(inner_let_idx, *sc);
        inner_let_idx += 1;
    }
    // Lambda's own params: stash each wasm local into a let-local
    // bound under the source-level name so the body's
    // `lower_variable` lookup just works.
    for (i, lp) in lambda_params.iter().enumerate() {
        let wasm_local_idx = (i + 1) as u32;
        let ty = expected_param_tys[i];
        inner.out.push(TaggedOp {
            op: Op::LocalGet(wasm_local_idx),
            range: lambda_body.range,
        });
        inner.out.push(TaggedOp {
            op: Op::LetSet {
                idx: inner_let_idx,
                ty,
            },
            range: lambda_body.range,
        });
        let type_repr = expected_param_reprs
            .and_then(|reprs| reprs.get(i).copied().flatten())
            .cloned();
        inner.lets.push(LetBinding {
            name: lp.name.clone(),
            idx: inner_let_idx,
            ty,
            schema_brand: None,
            type_repr,
        });
        inner_let_idx += 1;
    }
    inner.next_let_idx = inner_let_idx;

    // Body lowering. Enum-like values need the declared return type so
    // constructors such as `Stat.Up` / `Some(x)` can resolve inside HOFs.
    if let Some(expected) = expected_ret_repr {
        lower_value_as_type(expected, lambda_body, &mut inner)?;
    } else {
        lower_expr(&lambda_body.expr, lambda_body.range, &mut inner)?;
    }
    let body_ty = inner.tstack.last().copied().ok_or_else(|| {
        cap!(
            "lower_closure_as_value.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: "Closure(empty-body-stack)".to_string(),
                range: lambda_body.range,
            }
        )
    })?;
    if body_ty.wasm_slot() != expected_ret_ty.wasm_slot() {
        return Err(cap!(
            "lower_closure_as_value.stdlib_arg_type_mismatch",
            LoweringError::StdlibArgTypeMismatch {
                name: "closure-return".to_string(),
                arg_idx: 0,
                got: body_ty,
                expected: expected_ret_ty,
                range: lambda_body.range,
            }
        ));
    }
    inner.out.push(TaggedOp {
        op: Op::Return,
        range: lambda_body.range,
    });

    // -----------------------------------------------------------------
    // Outer-side: emit the MakeClosure op. The closure-table slot was
    // RESERVED before the body lowered (`fn_table_idx` above) on the
    // module-wide shared `lambda_table`; nested lambdas created during
    // the body took strictly-later slots through the same shared table.
    // Drop the built Func into its reserved slot. The final closure
    // table is assembled (in slot order) by the entry assembler.
    // -----------------------------------------------------------------
    let lambda_func = Func {
        name: format!("__closure_{}", fn_table_idx),
        params: lambda_param_tys,
        ret: expected_ret_ty,
        body: inner.out,
        range: closure_range,
    };
    ctx.set_lambda_slot(fn_table_idx, lambda_func);

    ctx.out.push(TaggedOp {
        op: Op::MakeClosure {
            fn_table_idx,
            captures,
            captures_size,
        },
        range: closure_range,
    });
    ctx.tstack.push(IrType::Closure);
    Ok(())
}
