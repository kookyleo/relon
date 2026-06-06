//! W-series peephole / codegen lowering helpers split out of `lowering`.
//!
//! These free functions recognise range-chain / list-sum / list-filter /
//! list-len / nested-range-map-reduce / range-materialisation shapes and
//! lower them directly to fused IR loops. They are a pure relocation of the
//! original `lowering.rs` bodies (no behaviour change); all back-references
//! into the parent module resolve through `super`.

use super::*;

/// review-improvement-160 bytecode M3 phase 2: recognise the
/// `list.sum(range(...))` peephole at the receiver/method/inner-call
/// level and emit an explicit `Op::Loop` accumulator.  Returns
/// `Ok(Some(()))` when the desugar fired (leaves a single `I64` on the
/// vstack), `Ok(None)` when the pattern did not match (caller falls
/// through to the normal lowering path), or `Err` when the inner
/// argument expressions themselves fail to lower.
///
/// Pattern shapes accepted (Open follow-up #2 — IR lowering surface
/// expansion):
///   * `list.sum(range(end))` — `start = 0`.
///   * `list.sum(range(start, end))`.
///   * `list.sum(range(...).map((p) => <body_i64>))` — inlines the
///     closure body per iteration; the body's i64 result is added to
///     the running accumulator. Captures into the body are limited to
///     in-scope let-bindings / `#main` params already reachable from
///     the outer ctx (no heap-style capture frame; the body is
///     emitted directly into the outer ctx's op stream so its
///     captures resolve through the same let-table walk the top-level
///     body uses).
///
/// All inner args must be plain positional (no keyword form); the
/// inner `range` call must use the bare identifier head (no dotted
/// receiver).  Anything else falls through to the default path so a
/// future `list.sum(my_user_fn())` keeps the existing diagnostic.
pub(super) fn try_lower_list_sum_range(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer head must be `list.sum(<single positional arg>)`.
    if path.len() != 2 {
        return Ok(None);
    }
    let outer_head = matches!(&path[0], TokenKey::String(s, _, _) if s == "list");
    let outer_method = matches!(&path[1], TokenKey::String(s, _, _) if s == "sum");
    if !(outer_head && outer_method) {
        return Ok(None);
    }
    if args.len() != 1 || args[0].name.is_some() {
        return Ok(None);
    }

    // Walk the chain rooted at `range(...)`, peeling off
    // recognized `.map((p) => <body>)` / `.filter((p) => <body>)`
    // invocations.  Returns the base range_args plus the per-stage
    // closure adaptor list; otherwise falls through.
    //
    // The *bare* `list.sum(range(...))` subset (no map/filter stages) is
    // ALSO recognised by the shared AST recogniser
    // `relon_parser::rewrite::recognize_fused`, which the tree-walk
    // interpreter now consumes for its materialisation-free fast-path. This
    // IR helper retains its own `match_range_chain` walk because it must
    // additionally lower the map/filter chain forms (the chain handling is
    // the other W-series peephole infrastructure). The two recognisers are
    // kept structurally consistent for the bare-range subset; see the
    // `recognizer_parity` tests at the bottom of this module for the parity
    // guard. The IR helper stays the authority on the IR side (no double
    // rewrite: the interpreter fast-path and IR lowering are mutually
    // exclusive — one runs in the tree-walk backend, the other in the
    // compiled backends).
    let Some(chain) = match_range_chain(&args[0].value.expr) else {
        return Ok(None);
    };

    emit_range_pipeline_loop(&chain, RangeConsumer::SumI64, range, ctx)?;
    Ok(Some(()))
}

/// Open follow-up #2 companion to `try_lower_list_sum_range`: recognise
/// `range(...)[ . map(c) | . filter(c) ]*.len()` and emit a pure i64
/// count accumulator. Returns `Ok(Some(()))` on a successful desugar
/// (vstack carries one `I64` after return), `Ok(None)` when the
/// pattern didn't match (caller falls through to default dispatch),
/// `Err` when an inner expression failed to lower.
///
/// The W4 cmp_lua workload (`range(n).map((i) => "axb")
/// .filter((s) => s.contains("x")).len()`) is the canonical caller.
/// Without this peephole the call would resolve `range` as an unknown
/// stdlib method (`range` is a tree-walker host-fn, not an IR stdlib
/// entry), or — if range were promoted — the filter / map pipeline
/// would materialise a transient `List<String>` the bytecode scalar
/// envelope rejects. The peephole side-steps both by emitting the
/// equivalent scalar loop directly.
pub(super) fn try_lower_range_chain_len(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer call must be `<receiver>.len()` with no args.
    if path.len() != 2 || !args.is_empty() {
        return Ok(None);
    }
    let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
        return Ok(None);
    };
    let TokenKey::String(method_name, _, _) = &path[1] else {
        return Ok(None);
    };
    if method_name.as_str() != "len" {
        return Ok(None);
    }
    let Some(chain) = match_range_chain(&receiver_node.expr) else {
        return Ok(None);
    };
    emit_range_pipeline_loop(&chain, RangeConsumer::Len, range, ctx)?;
    Ok(Some(()))
}

/// Open follow-up #2 companion to `try_lower_list_sum_range` /
/// `try_lower_range_chain_len`: recognise `<chain>.reduce(<init>,
/// (acc, elem) => body)` and emit a per-iteration accumulator update
/// driven by the user's body. Returns `Ok(Some(()))` on a successful
/// desugar (vstack carries the accumulator's type after return),
/// `Ok(None)` when the pattern didn't match, `Err` when an inner
/// expression failed to lower.
///
/// The cmp_lua W3 workload (`range(n).map((i) => "a").reduce("",
/// (acc, s) => acc + s)`) is the canonical caller — its string-concat
/// reduce returns a `String` accumulator the bytecode VM accepts via
/// the B-1 / B-2 string-arena infrastructure.
pub(super) fn try_lower_range_chain_reduce(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer call shape: `<receiver>.reduce(<init>, <closure>)` — two
    // positional args, second is the closure literal.
    if path.len() != 2 || args.len() != 2 {
        return Ok(None);
    }
    if args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
        return Ok(None);
    };
    let TokenKey::String(method_name, _, _) = &path[1] else {
        return Ok(None);
    };
    if method_name.as_str() != "reduce" {
        return Ok(None);
    }
    let init_node = &args[0].value;
    let Expr::Closure {
        params,
        body,
        return_type: _,
    } = &*args[1].value.expr
    else {
        return Ok(None);
    };
    if params.len() != 2 {
        return Ok(None);
    }
    let Some(chain) = match_range_chain(&receiver_node.expr) else {
        return Ok(None);
    };
    emit_range_pipeline_loop(
        &chain,
        RangeConsumer::Reduce {
            init: init_node,
            params: params.as_slice(),
            body,
        },
        range,
        ctx,
    )?;
    Ok(Some(()))
}

/// AOT-4 (W19 slice): `<materialised-list>.reduce(<init>, (acc, elem)
/// => body)` over a where-bound `List<Int>` / `List<List<Int>>` handle
/// (NOT a `range(...)` chain — that's `try_lower_range_chain_reduce`).
/// The canonical caller is the W19 `#main` body
/// `c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc,
/// cell) => cell_acc + cell))`, where `c` is the materialised result
/// matrix (a `List<List<Int>>`) and each `row` is an inner row handle
/// re-reduced cell-by-cell.
///
/// Returns `Ok(Some(()))` on a successful desugar (vstack carries the
/// accumulator type), `Ok(None)` when the receiver is NOT a materialised
/// list handle (so the range-chain / generic paths get a clean shot),
/// `Err` when an inner expression failed to lower.
///
/// The element loop reads the record's `[len]` header for the bound,
/// then loads each i64 element from the payload (`payload + i*8`, the
/// same inline addressing the index path uses). When the reduce body
/// uses `elem` as a list (e.g. `row.reduce(...)`), the element is an
/// inner row handle — it's retagged `ListInt` (`LetSet{ListInt}`
/// truncates the i64 to the i32 handle) so the nested reduce sees a
/// proper list receiver; otherwise `elem` rides as `I64`.
pub(super) fn try_lower_materialized_list_reduce(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    if path.len() != 2 || args.len() != 2 {
        return Ok(None);
    }
    if args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    // The method name is the LAST path segment. The receiver is the
    // FIRST: a bare-variable receiver (`c.reduce(...)`) parses as
    // `[String(recv_name), String("reduce")]`; a complex-expression
    // receiver (`<expr>.reduce(...)`) parses as
    // `[Dynamic(<receiver Node>), String("reduce")]`. We only accept the
    // bare-variable form here — a where-bound `List<Int>` let resolves
    // by name.
    let TokenKey::String(method_name, _, _) = &path[1] else {
        return Ok(None);
    };
    if method_name.as_str() != "reduce" {
        return Ok(None);
    }
    let recv_name = match &path[0] {
        TokenKey::String(s, _, _) => s.clone(),
        // A `Dynamic` receiver is a complex expression (e.g. a range
        // chain), which `try_lower_range_chain_reduce` owns; bail.
        _ => return Ok(None),
    };
    let Expr::Closure {
        params,
        body,
        return_type: _,
    } = &*args[1].value.expr
    else {
        return Ok(None);
    };
    if params.len() != 2 {
        return Ok(None);
    }
    // Only fire when the receiver name resolves to a where-bound
    // `List<Int>` let. We peek the let table WITHOUT lowering so a
    // non-list receiver bails cleanly to the generic path.
    let is_list_let = ctx
        .lets
        .iter()
        .rev()
        .find(|b| b.name == recv_name)
        .map(|b| b.ty == IrType::ListInt)
        .unwrap_or(false);
    if !is_list_let {
        return Ok(None);
    }

    let init_node = &args[0].value;

    // Lower the receiver — emit a `LetGet` of the list handle. (We can't
    // `lower_expr` a `Variable` path slice directly through the FnCall
    // surface, so resolve the let by hand and push its handle.)
    let recv_idx = ctx
        .lets
        .iter()
        .rev()
        .find(|b| b.name == recv_name)
        .map(|b| b.idx)
        .ok_or_else(|| {
            cap!(
                "try_lower_materialized_list_reduce.unresolved_variable",
                LoweringError::UnresolvedVariable {
                    name: recv_name.clone(),
                    range,
                }
            )
        })?;
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: recv_idx,
            ty: IrType::ListInt,
        },
        range,
    });
    ctx.tstack.push(IrType::ListInt);

    // Slot plan.
    let base_i = ctx.next_let_idx;
    let count_i = ctx.next_let_idx + 1;
    let payload_i = ctx.next_let_idx + 2;
    let i_i = ctx.next_let_idx + 3;
    let acc_i = ctx.next_let_idx + 4;
    ctx.next_let_idx += 5;

    // base = receiver handle (i32).
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::ListInt,
        },
        range,
    });
    ctx.tstack.pop();

    // count = i32.load(base) — the record's `[len]` header.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::ListInt,
        },
        range,
    });
    ctx.tstack.push(IrType::ListInt);
    ctx.out.push(TaggedOp {
        op: Op::LoadI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // payload = (base + 4 + 7) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::ListInt,
        },
        range,
    });
    ctx.tstack.push(IrType::ListInt);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // acc = <init>.
    lower_expr(&init_node.expr, init_node.range, ctx)?;
    let acc_ty = ctx.tstack.last().copied().ok_or_else(|| {
        cap!(
            "try_lower_materialized_list_reduce.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "materialised-list reduce: init produced no value".to_string(),
                range: init_node.range,
            }
        )
    })?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.pop();

    // i = 0
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(0),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // The element type: `ListInt` when the body treats `elem` as a list
    // (nested `row.reduce(...)` etc.), else `I64`.
    let elem_ty = if closure_param_used_as_list_int(&params[1].name, &body.expr) {
        IrType::ListInt
    } else {
        IrType::I64
    };
    // Reserve the acc + elem param let slots up front so the body's
    // `Variable` lookups resolve.
    let acc_param_let = ctx.next_let_idx;
    let elem_param_let = ctx.next_let_idx + 1;
    let raw_elem_i = ctx.next_let_idx + 2; // raw i64 element load
    ctx.next_let_idx += 3;

    // Build the loop body in a sub-buffer.
    let saved_outer = std::mem::take(&mut ctx.out);

    // exit when i >= count -> br 1
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Ge(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::BrIf { label_depth: 1 },
        range,
    });

    // raw_elem = i64.load(payload + i*8)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LoadI64AtAbsolute { offset: 0 },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: raw_elem_i,
            ty: IrType::I64,
        },
        range,
    });

    // Bind acc = acc_i contents.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: acc_param_let,
            ty: acc_ty,
        },
        range,
    });
    ctx.lets.push(LetBinding {
        name: params[0].name.clone(),
        idx: acc_param_let,
        ty: acc_ty,
        schema_brand: None,
    });
    // Bind elem. When `elem` is a list handle, `LetSet{ListInt}`
    // truncates the i64 element to the i32 row handle.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: raw_elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: elem_param_let,
            ty: elem_ty,
        },
        range,
    });
    ctx.lets.push(LetBinding {
        name: params[1].name.clone(),
        idx: elem_param_let,
        ty: elem_ty,
        schema_brand: None,
    });

    // Lower the reduce body — leaves the new acc on top.
    lower_expr(&body.expr, body.range, ctx)?;
    let produced = ctx.tstack.last().copied().ok_or_else(|| {
        cap!(
            "try_lower_materialized_list_reduce.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: "materialised-list reduce: body produced no value".to_string(),
                range: body.range,
            }
        )
    })?;
    if produced.wasm_slot() != acc_ty.wasm_slot() {
        ctx.lets.pop();
        ctx.lets.pop();
        let _ = std::mem::replace(&mut ctx.out, saved_outer);
        return Err(cap!(
            "try_lower_materialized_list_reduce.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "materialised-list reduce: body returned {:?}, expected init type {:?}",
                    produced, acc_ty
                ),
                range: body.range,
            }
        ));
    }
    ctx.lets.pop(); // elem
    ctx.lets.pop(); // acc
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.pop();

    // i += 1 ; br 0
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Br { label_depth: 0 },
        range,
    });

    // Wrap under Block { Loop { ... } }.
    let loop_body = std::mem::replace(&mut ctx.out, saved_outer);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: loop_body,
                },
                range,
            }],
        },
        range,
    });

    // Push the final accumulator.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.push(acc_ty);
    Ok(Some(()))
}

/// AOT-2: a single recognised `range(start, end)[. map((p) => body)]`
/// chain whose final stage produces a *list-valued* row. Distilled
/// from a `match_range_chain` result whose terminal map body is itself
/// a bare inner `range(...).map(...)` chain. Used by
/// [`try_lower_nested_range_map_reduce`] to drive the inner accumulator
/// loop without materialising the row list.
struct NestedRangeShape<'a> {
    /// Outer `range(...)` bounds (the `i` loop).
    outer_range_args: &'a [relon_parser::CallArg],
    /// Outer map closure param (`i`).
    outer_param: &'a ClosureParam,
    /// Inner `range(...)` bounds (the `j` loop). Captures into the
    /// outer counter resolve through the normal let-table walk.
    inner_range_args: &'a [relon_parser::CallArg],
    /// Inner map closure param (`j`).
    inner_param: &'a ClosureParam,
    /// Inner map body — the per-cell expression (`(i * size + j) % 100`
    /// in W19). Must lower to an `I64`.
    cell_body: &'a Node,
}

/// Recognise the outer chain of a doubly-nested
/// `range(...).map((i) => range(...).map((j) => <cell>))` so the
/// caller can fuse it into two integer loops. Returns `None` for any
/// shape outside the single-outer-map / single-inner-map form (the
/// caller falls through to the regular diagnostic).
fn match_nested_range_map(expr: &Expr) -> Option<NestedRangeShape<'_>> {
    // The outer receiver must be a one-stage `range(...).map(<closure>)`.
    let outer_chain = match_range_chain(expr)?;
    if outer_chain.stages.len() != 1 {
        return None;
    }
    let outer_stage = &outer_chain.stages[0];
    if outer_stage.method != "map" || outer_stage.closure_params.len() != 1 {
        return None;
    }
    // The outer map's body must itself be a bare one-stage
    // `range(...).map(<closure>)` — the inner row generator.
    let inner_chain = match_range_chain(&outer_stage.closure_body.expr)?;
    if inner_chain.stages.len() != 1 {
        return None;
    }
    let inner_stage = &inner_chain.stages[0];
    if inner_stage.method != "map" || inner_stage.closure_params.len() != 1 {
        return None;
    }
    Some(NestedRangeShape {
        outer_range_args: outer_chain.range_args,
        outer_param: &outer_stage.closure_params[0],
        inner_range_args: inner_chain.range_args,
        inner_param: &inner_stage.closure_params[0],
        cell_body: inner_stage.closure_body,
    })
}

/// How the outer reduce closure folds each (list-valued) row into the
/// running accumulator. The matmul shape sums every cell of the row,
/// so the inner fold is itself an i64 sum/reduce over the row.
enum RowFold<'a> {
    /// `list.sum(row)` — sum the row's cells.
    Sum,
    /// `row.reduce(<init>, (cell_acc, cell) => cell_acc + cell)` — a
    /// user-supplied i64 reduce over the row. `init` lowers to the
    /// inner accumulator seed; `acc_param` / `cell_param` bind the
    /// closure's two params; `body` updates the accumulator per cell.
    Reduce {
        init: &'a Node,
        acc_param: &'a ClosureParam,
        cell_param: &'a ClosureParam,
        body: &'a Node,
    },
}

/// Decompose the outer reduce body into a combine operator plus the
/// inner row-fold. Recognises `row_acc <op> <fold(row)>` and the
/// commuted `<fold(row)> <op> row_acc`.
struct OuterCombine<'a> {
    /// The binary operator joining the running accumulator with the
    /// row fold (`+` for W19's cell-sum).
    op: Operator,
    /// `true` when the running-accumulator term is the LHS of `op`
    /// (`row_acc + fold`); `false` for the commuted `fold + row_acc`.
    acc_is_lhs: bool,
    /// The inner row fold extracted from the non-accumulator term.
    fold: RowFold<'a>,
}

/// Recognise `list.sum(<row>)` or `<row>.reduce(<init>, (a, c) =>
/// <body>)` where `<row>` is the bare outer reduce param named
/// `row_name`. Returns the inner fold descriptor, or `None` if the
/// expression isn't a recognised single-row fold.
fn match_row_fold<'a>(expr: &'a Expr, row_name: &str) -> Option<RowFold<'a>> {
    let Expr::FnCall { path, args } = expr else {
        return None;
    };
    // `list.sum(row)` — path == [list, sum], single positional arg
    // that is the bare `row` variable.
    if path.len() == 2 {
        let head_is_list = matches!(&path[0], TokenKey::String(s, _, _) if s == "list");
        let tail_is_sum = matches!(&path[1], TokenKey::String(s, _, _) if s == "sum");
        if head_is_list && tail_is_sum && args.len() == 1 && args[0].name.is_none() {
            if expr_is_bare_var(&args[0].value.expr, row_name) {
                return Some(RowFold::Sum);
            }
            return None;
        }
    }
    // `row.reduce(init, (acc, cell) => body)` — method call whose
    // receiver is the bare `row` variable. The parser encodes a
    // bare-identifier receiver as `path[0] = String(row)` (a
    // multi-segment Variable-style path) rather than the
    // `Dynamic(...)` wrapper it uses for sub-expression receivers, so
    // accept both encodings.
    if path.len() == 2 && args.len() == 2 && args.iter().all(|a| a.name.is_none()) {
        let TokenKey::String(method_name, _, _) = &path[1] else {
            return None;
        };
        if method_name.as_str() != "reduce" {
            return None;
        }
        let receiver_is_row = match &path[0] {
            TokenKey::String(s, _, _) => s == row_name,
            TokenKey::Dynamic(receiver_node, _) => expr_is_bare_var(&receiver_node.expr, row_name),
            _ => false,
        };
        if !receiver_is_row {
            return None;
        }
        let Expr::Closure {
            params,
            body,
            return_type: _,
        } = &*args[1].value.expr
        else {
            return None;
        };
        if params.len() != 2 {
            return None;
        }
        return Some(RowFold::Reduce {
            init: &args[0].value,
            acc_param: &params[0],
            cell_param: &params[1],
            body,
        });
    }
    None
}

/// `true` when `expr` is a single-segment `Variable` naming `name`.
fn expr_is_bare_var(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Variable(segs)
        if segs.len() == 1
            && matches!(&segs[0], TokenKey::String(s, _, _) if s == name))
}

/// Decompose `row_acc <op> <fold(row)>` (or commuted). The accumulator
/// term is a bare reference to `acc_name`, and the other term is a
/// recognised [`RowFold`] over `row_name`.
fn match_outer_combine<'a>(
    body: &'a Expr,
    acc_name: &str,
    row_name: &str,
) -> Option<OuterCombine<'a>> {
    let Expr::Binary(op, lhs, rhs) = body else {
        return None;
    };
    // `row_acc + fold(row)`
    if expr_is_bare_var(&lhs.expr, acc_name) {
        let fold = match_row_fold(&rhs.expr, row_name)?;
        return Some(OuterCombine {
            op: *op,
            acc_is_lhs: true,
            fold,
        });
    }
    // `fold(row) + row_acc`
    if expr_is_bare_var(&rhs.expr, acc_name) {
        let fold = match_row_fold(&lhs.expr, row_name)?;
        return Some(OuterCombine {
            op: *op,
            acc_is_lhs: false,
            fold,
        });
    }
    None
}

/// AOT-2: lower a doubly-nested `range.map(range.map(...))` reduced
/// cell-by-cell into a pair of nested i64 accumulator loops with NO
/// intermediate list materialised. The canonical caller is the W19
/// matrix-multiply kernel:
///
/// ```text
/// range(size).map((i) => range(size).map((j) => (i * size + j) % 100))
///   .reduce(0, (row_acc, row) => row_acc + row.reduce(0, (c_acc, c) => c_acc + c))
/// ```
///
/// Returns `Ok(Some(()))` on a successful desugar (vstack carries one
/// `I64` after return), `Ok(None)` when the pattern didn't match
/// (caller falls through to the regular diagnostic), `Err` when an
/// inner expression failed to lower.
pub(super) fn try_lower_nested_range_map_reduce(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer call shape: `<receiver>.reduce(<init>, <closure>)`.
    if path.len() != 2 || args.len() != 2 {
        return Ok(None);
    }
    if args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
        return Ok(None);
    };
    let TokenKey::String(method_name, _, _) = &path[1] else {
        return Ok(None);
    };
    if method_name.as_str() != "reduce" {
        return Ok(None);
    }
    let init_node = &args[0].value;
    let Expr::Closure {
        params,
        body,
        return_type: _,
    } = &*args[1].value.expr
    else {
        return Ok(None);
    };
    if params.len() != 2 {
        return Ok(None);
    }
    let row_acc_name = params[0].name.as_str();
    let row_name = params[1].name.as_str();

    // The receiver must be a doubly-nested `range.map(range.map(...))`.
    let Some(shape) = match_nested_range_map(&receiver_node.expr) else {
        return Ok(None);
    };
    // The reduce body must fold the (list-valued) row cell-by-cell.
    let Some(combine) = match_outer_combine(&body.expr, row_acc_name, row_name) else {
        return Ok(None);
    };

    emit_nested_range_map_reduce(&shape, init_node, &combine, range, ctx)?;
    Ok(Some(()))
}

/// Emit the doubly-nested i64 accumulator loop for the recognised
/// matmul reduction. Pre-condition: the caller matched the outer chain
/// via [`match_nested_range_map`] and the reduce body via
/// [`match_outer_combine`].
///
/// Control-flow shape (mirrors [`emit_range_pipeline_loop`] but with an
/// inner loop nested inside the outer iteration body):
///
/// ```text
/// outer_acc = <init>
/// i = outer_start
/// block (outer-exit) {
///   loop {
///     if i >= outer_end { br 1 }
///     row_acc = <inner-init>
///     j = inner_start
///     block (inner-exit) {
///       loop {
///         if j >= inner_end { br 1 }
///         <cell = cell_body(i, j)>      // I64
///         <row_acc fold-update cell>    // inner fold
///         j += 1
///         br 0
///       }
///     }
///     outer_acc = outer_acc <op> row_acc
///     i += 1
///     br 0
///   }
/// }
/// push outer_acc
/// ```
fn emit_nested_range_map_reduce(
    shape: &NestedRangeShape<'_>,
    init_node: &Node,
    combine: &OuterCombine<'_>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Allocate the outer loop counters + accumulator.
    let outer_start_i = ctx.next_let_idx;
    let outer_end_i = ctx.next_let_idx + 1;
    let outer_acc_i = ctx.next_let_idx + 2;
    ctx.next_let_idx += 3;

    // outer_start
    emit_range_bound(shape.outer_range_args, true, outer_start_i, range, ctx)?;
    // outer_end
    emit_range_bound(shape.outer_range_args, false, outer_end_i, range, ctx)?;

    // outer_acc = <init> (must lower to I64 for the matmul shape).
    lower_expr(&init_node.expr, init_node.range, ctx)?;
    expect_int_top(ctx, init_node.range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: outer_acc_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // ---- outer loop body sub-buffer -------------------------------
    let saved_outer = std::mem::take(&mut ctx.out);

    // if i >= outer_end -> br 1 (exit outer-exit block)
    emit_ge_brif(outer_start_i, outer_end_i, 1, range, ctx);

    // Bind the outer map param (`i`) to the loop counter so the inner
    // range bounds + cell body resolve captures through the walker.
    let outer_param_let = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: outer_start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: outer_param_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.lets.push(LetBinding {
        name: shape.outer_param.name.clone(),
        idx: outer_param_let,
        ty: IrType::I64,
        schema_brand: None,
    });

    // ---- inner row fold: allocate counters + accumulator ----------
    let inner_start_i = ctx.next_let_idx;
    let inner_end_i = ctx.next_let_idx + 1;
    let row_acc_i = ctx.next_let_idx + 2;
    ctx.next_let_idx += 3;

    emit_range_bound(shape.inner_range_args, true, inner_start_i, range, ctx)?;
    emit_range_bound(shape.inner_range_args, false, inner_end_i, range, ctx)?;

    // row_acc = <inner-init>. `Sum` seeds 0; `Reduce` lowers the user
    // init expression (must be I64).
    match &combine.fold {
        RowFold::Sum => {
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(0),
                range,
            });
            ctx.tstack.push(IrType::I64);
        }
        RowFold::Reduce { init, .. } => {
            lower_expr(&init.expr, init.range, ctx)?;
            expect_int_top(ctx, init.range)?;
        }
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: row_acc_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // ---- inner loop body sub-buffer -------------------------------
    let saved_inner = std::mem::take(&mut ctx.out);

    // if j >= inner_end -> br 1 (exit inner-exit block)
    emit_ge_brif(inner_start_i, inner_end_i, 1, range, ctx);

    // Bind the inner map param (`j`) to the inner counter.
    let inner_param_let = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: inner_start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: inner_param_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.lets.push(LetBinding {
        name: shape.inner_param.name.clone(),
        idx: inner_param_let,
        ty: IrType::I64,
        schema_brand: None,
    });

    // cell = <cell_body(i, j)> — must be I64.
    lower_expr(&shape.cell_body.expr, shape.cell_body.range, ctx)?;
    expect_int_top(ctx, shape.cell_body.range)?;
    let cell_let = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: cell_let,
            ty: IrType::I64,
        },
        range: shape.cell_body.range,
    });
    ctx.tstack.pop();
    ctx.lets.pop(); // drop the inner `j` binding before the fold body

    // ---- per-cell fold update ------------------------------------
    match &combine.fold {
        RowFold::Sum => {
            // row_acc += cell
            emit_acc_add_cell(row_acc_i, cell_let, range, ctx);
        }
        RowFold::Reduce {
            acc_param,
            cell_param,
            body,
            ..
        } => {
            // Bind acc / cell params and lower the user fold body; its
            // i64 result becomes the new row_acc.
            let acc_param_let = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: row_acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_param_let,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.lets.push(LetBinding {
                name: acc_param.name.clone(),
                idx: acc_param_let,
                ty: IrType::I64,
                schema_brand: None,
            });
            ctx.lets.push(LetBinding {
                name: cell_param.name.clone(),
                idx: cell_let,
                ty: IrType::I64,
                schema_brand: None,
            });
            lower_expr(&body.expr, body.range, ctx)?;
            expect_int_top(ctx, body.range)?;
            ctx.lets.pop(); // cell
            ctx.lets.pop(); // acc
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: row_acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.pop();
        }
    }

    // j += 1; br 0 (back to inner loop header)
    emit_incr_and_loop(inner_start_i, range, ctx);

    // Pop the inner loop body and wrap in Block { Loop { ... } }.
    let inner_body = std::mem::replace(&mut ctx.out, saved_inner);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: inner_body,
                },
                range,
            }],
        },
        range,
    });

    // ---- combine row_acc into the outer accumulator ---------------
    // outer_acc = outer_acc <op> row_acc (or commuted). Both terms
    // are I64; the result stays I64 for the i64 accumulator slot.
    if combine.acc_is_lhs {
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: outer_acc_i,
                ty: IrType::I64,
            },
            range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: row_acc_i,
                ty: IrType::I64,
            },
            range,
        });
    } else {
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: row_acc_i,
                ty: IrType::I64,
            },
            range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: outer_acc_i,
                ty: IrType::I64,
            },
            range,
        });
    }
    ctx.tstack.push(IrType::I64);
    ctx.tstack.push(IrType::I64);
    let combine_op = combine_operator_to_op(combine.op, range)?;
    ctx.out.push(TaggedOp {
        op: combine_op,
        range,
    });
    ctx.tstack.pop();
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: outer_acc_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // Drop the outer `i` binding now that its body region is emitted.
    ctx.lets.pop();

    // i += 1; br 0 (back to outer loop header)
    emit_incr_and_loop(outer_start_i, range, ctx);

    // Pop the outer loop body and wrap in Block { Loop { ... } }.
    let outer_body = std::mem::replace(&mut ctx.out, saved_outer);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: outer_body,
                },
                range,
            }],
        },
        range,
    });

    // Push the final accumulator so the consumer sees an I64 on top.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: outer_acc_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    Ok(())
}

/// Lower one `range(start, end)` bound into a let slot. `is_start`
/// selects the start arg (defaulting to `0` for the single-arg
/// `range(n)` form) or the end arg. The lowered value must be I64.
fn emit_range_bound(
    range_args: &[relon_parser::CallArg],
    is_start: bool,
    target_let: u32,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if is_start {
        if range_args.len() == 2 {
            lower_expr(&range_args[0].value.expr, range_args[0].value.range, ctx)?;
            expect_int_top(ctx, range)?;
        } else {
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(0),
                range,
            });
            ctx.tstack.push(IrType::I64);
        }
    } else {
        let end_arg = &range_args[range_args.len() - 1];
        lower_expr(&end_arg.value.expr, end_arg.value.range, ctx)?;
        expect_int_top(ctx, range)?;
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: target_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();
    Ok(())
}

/// Emit `if let[start] >= let[end] { br <label_depth> }` (the loop-exit
/// guard). The comparison leaves a Bool that `BrIf` consumes.
fn emit_ge_brif(
    start_let: u32,
    end_let: u32,
    label_depth: u32,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) {
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: end_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Ge(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::BrIf { label_depth },
        range,
    });
}

/// Emit `acc += cell` for two i64 let slots, storing back into `acc`.
fn emit_acc_add_cell(acc_let: u32, cell_let: u32, range: TokenRange, ctx: &mut LowerCtx<'_>) {
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: acc_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: cell_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: acc_let,
            ty: IrType::I64,
        },
        range,
    });
}

/// Emit `counter += 1; br 0` — advance the loop counter and branch
/// back to the loop header.
fn emit_incr_and_loop(counter_let: u32, range: TokenRange, ctx: &mut LowerCtx<'_>) {
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: counter_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: counter_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Br { label_depth: 0 },
        range,
    });
}

/// Map the recognised combine `Operator` onto its i64 `Op`. The matmul
/// reduction only uses `+`, but `*` / `-` are accepted for symmetry so
/// the same emitter covers product / difference cell folds.
fn combine_operator_to_op(op: Operator, range: TokenRange) -> Result<Op, LoweringError> {
    match op {
        Operator::Add => Ok(Op::Add(IrType::I64)),
        Operator::Sub => Ok(Op::Sub(IrType::I64)),
        Operator::Mul => Ok(Op::Mul(IrType::I64)),
        other => Err(cap!(
            "combine_operator_to_op.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "nested range.map reduce: unsupported combine operator {:?}",
                    other
                ),
                range,
            }
        )),
    }
}

/// CODEGEN-QUALITY (W18 slice): recognise `_len(_list_filter(range(a,
/// b), (x) => <pred>))` — where the filtered list is *dead* (only
/// `_len` consumes it) — and fuse it to a pure i64 counting loop that
/// never materialises the filtered list:
///
/// ```text
/// count = 0
/// for k in [a, b):
///   if <pred>(k) { count += 1 }
/// push count
/// ```
///
/// This is a dead-list-elimination / stream-fusion rewrite, NOT an
/// algorithm substitution: the algorithm (count the range elements
/// satisfying `<pred>`) is unchanged; only the intermediate
/// `List<Int>` is elided — exactly as `rust_native` counts in
/// registers. The survivor *count* is bit-for-bit identical to the
/// materialise-then-`_len` path (same predicate, same range, same
/// `start >= end` empty-range edge), so the W18 prime-count oracle
/// still matches.
///
/// Mechanism: the predicate closure becomes the single `filter` stage
/// of a [`RangeChain`], and [`emit_range_pipeline_loop`] with
/// [`RangeConsumer::Len`] emits the counter loop — the same battle-
/// tested skeleton the `range(...).filter(...).len()` peephole uses.
/// The predicate body is inlined into the loop (its `is_prime(k, 2)`
/// call lowers as a direct call, devirtualised), so the post-O3 hot
/// loop carries NO `AllocScratchDyn` for the filter output, NO
/// `list_int_filter` `Op::Call`, and NO per-element arena load/store
/// round-trip — just a counter increment under the predicate.
///
/// `_len` and `_list_filter` are the underscore intrinsics the
/// tree-walker registers (`relon-evaluator::stdlib::register_to`);
/// they have no bundled IR stdlib slot keyed under those exact names,
/// so the default `lower_fn_call` dispatch would surface an
/// `UnknownStdlibMethod`. This peephole maps the W18 shape onto the
/// scalar counter loop.
///
/// The fusion fires ONLY for the `_len(_list_filter(range, pred))`
/// shape where the list is dead (its single consumer is the outer
/// `_len`). When the filtered list is fed to another consumer (e.g.
/// indexed, re-filtered, or summed — W16 / W19) the `_list_filter`
/// surfaces under a different parent and this peephole never matches,
/// so those workloads keep their real materialised list.
///
/// Returns `Ok(Some(()))` on a successful fusion (vstack carries one
/// `I64` survivor count after return), `Ok(None)` when the pattern
/// didn't match (caller falls through to the regular dispatch), `Err`
/// when an inner expression failed to lower.
pub(super) fn try_lower_len_filter_range(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer call must be the free-call `_len(<single positional arg>)`.
    if path.len() != 1 || args.len() != 1 || args[0].name.is_some() {
        return Ok(None);
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "_len" || s == "len") {
        return Ok(None);
    }
    // The argument must itself be `_list_filter(range(a, b), closure)`.
    let Expr::FnCall {
        path: inner_path,
        args: inner_args,
    } = &*args[0].value.expr
    else {
        return Ok(None);
    };
    if inner_path.len() != 1 || inner_args.len() != 2 {
        return Ok(None);
    }
    if !matches!(&inner_path[0], TokenKey::String(s, _, _) if s == "_list_filter") {
        return Ok(None);
    }
    if inner_args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    // First inner arg: a bare `range(a, b)` (or `range(b)`). Second:
    // the predicate closure literal.
    let Some(range_args) = match_bare_range(&inner_args[0].value.expr) else {
        return Ok(None);
    };
    let Expr::Closure {
        params,
        body: pred_body,
        ..
    } = &*inner_args[1].value.expr
    else {
        return Ok(None);
    };
    if params.len() != 1 {
        return Ok(None);
    }

    // Build a single-`filter` `RangeChain` from the predicate closure
    // and emit the shared `range(...).filter(...).len()` counter loop.
    // The predicate body must return `Bool`; the pipeline emitter
    // checks that and short-circuits the `count += 1` update when the
    // predicate is false. No `List<Int>` is allocated.
    let chain = RangeChain {
        range_args,
        stages: vec![ChainStage {
            method: "filter",
            closure_params: params.as_slice(),
            closure_body: pred_body,
        }],
    };
    emit_range_pipeline_loop(&chain, RangeConsumer::Len, range, ctx)?;
    Ok(Some(()))
}

/// AOT-4 (W16 slice): general `_len(xs)` / `len(xs)` over an arbitrary
/// `List<Int>`-typed argument (vs the fused
/// `_len(_list_filter(range(...)))` peephole). Lowers the argument
/// speculatively into a scratch op stream; commits only when it
/// produced an `IrType::ListInt` handle, emitting `Op::ReadStringLen`
/// (reads the leading `[len: u32 LE]` prefix the record layout shares
/// with strings) widened to I64 — matching the tree-walker `_len`,
/// which returns the element count as `Int`.
pub(super) fn try_lower_list_len(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    if path.len() != 1 || args.len() != 1 || args[0].name.is_some() {
        return Ok(None);
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "_len" || s == "len") {
        return Ok(None);
    }
    // Lower the argument into a scratch stream so a non-list result can
    // be rolled back without polluting `ctx.out` / `ctx.tstack`.
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    if lower_res.is_err() || produced != Some(IrType::ListInt) {
        // Roll back: discard the scratch stream and restore the outer
        // ctx untouched so the regular dispatch path re-lowers cleanly.
        // Truncate any closure-table slots reserved during the discarded
        // speculative lowering so the slot numbering stays dense (a
        // leaked slot would offset every later `fn_table_idx`).
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return Ok(None);
    }
    // Commit: splice the scratch stream back onto the outer ctx.
    let arg_stream = std::mem::replace(&mut ctx.out, saved_out);
    ctx.out.extend(arg_stream);
    let arg_stack = std::mem::replace(&mut ctx.tstack, saved_stack);
    ctx.tstack.extend(arg_stack);
    ctx.out.push(TaggedOp {
        op: Op::ReadStringLen,
        range,
    });
    ctx.tstack.pop(); // ListInt handle
    ctx.tstack.push(IrType::I64);
    Ok(Some(()))
}

/// AOT-4 (W16 slice): general `_list_filter(xs, (x) => <pred>)` over an
/// arbitrary `List<Int>`-typed first argument (vs the fused
/// `_len(_list_filter(range(...)))` peephole, which only handles a bare
/// `range(...)` source). Lowers the list argument speculatively;
/// commits only when it produced an `IrType::ListInt` handle, then
/// lowers the predicate closure and emits `Op::Call(list_int_filter)`,
/// leaving a fresh `List<Int>` handle on the vstack so the result can
/// be indexed, re-filtered, or recursed on.
pub(super) fn try_lower_list_filter(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    if path.len() != 1 || args.len() != 2 || args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "_list_filter") {
        return Ok(None);
    }
    // Second arg must be a single-param closure literal.
    let Expr::Closure { params, .. } = &*args[1].value.expr else {
        return Ok(None);
    };
    if params.len() != 1 {
        return Ok(None);
    }
    // Speculatively lower the list argument; roll back on a non-list
    // result (or a lowering error) so dispatch can fall through.
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    // Wave R3b: a `List<Float>` source routes to the float filter body
    // via the shared typed-HOF emitter. Roll back the speculative source
    // lowering first (the typed emitter re-lowers it itself), keeping
    // the `List<Int>` path below byte-identical.
    if lower_res.is_ok() && produced == Some(IrType::ListFloat) {
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return emit_list_hof_call(ListHofKind::Filter, args, range, ctx);
    }
    if lower_res.is_err() || produced != Some(IrType::ListInt) {
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return Ok(None);
    }
    let arg_stream = std::mem::replace(&mut ctx.out, saved_out);
    ctx.out.extend(arg_stream);
    let arg_stack = std::mem::replace(&mut ctx.tstack, saved_stack);
    ctx.tstack.extend(arg_stack);

    // Resolve the bundled `list_int_filter` slot.
    let filter_idx = stdlib_function_index("list_int_filter").ok_or_else(|| {
        cap!(
            "try_lower_list_filter.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: "list_int_filter".to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let filter_meta = builtin_stdlib().get(filter_idx as usize).ok_or_else(|| {
        cap!(
            "try_lower_list_filter.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: "list_int_filter".to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let filter_params = filter_meta.params.clone();
    let filter_ret = filter_meta.ret;

    // Lower the predicate closure (`(I64) -> Bool`).
    let (param_tys_c, ret_ty_c) =
        stdlib_closure_arg_signature("list_int_filter", 1).ok_or_else(|| {
            cap!(
                "try_lower_list_filter.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: "list_int_filter closure signature missing".to_string(),
                    range,
                }
            )
        })?;
    lower_closure_as_value(
        &args[1].value.expr,
        args[1].value.range,
        &param_tys_c,
        ret_ty_c,
        ctx,
    )?;
    // Op::Call(list_int_filter): consumes [ListInt, Closure], produces a
    // fresh List<Int> handle.
    ctx.tstack.pop(); // closure
    ctx.tstack.pop(); // source list handle
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index: filter_idx,
            arg_count: 2,
            param_tys: filter_params,
            ret_ty: filter_ret,
        },
        range,
    });
    ctx.tstack.push(filter_ret);
    Ok(Some(()))
}

/// Speculatively lower the closure literal `node` with the given param
/// types and a *candidate* return type, committing only when its body
/// type-checks against that return slot. Returns `Ok(true)` and leaves
/// the `MakeClosure` op + `Closure` tstack entry in place on success;
/// on a body/return-type mismatch it rolls the ctx back fully (ops,
/// tstack, let counter, reserved lambda slot) and returns `Ok(false)`
/// so the caller can retry with an alternative return type. A genuine
/// lowering error (not a return-type mismatch) propagates.
fn try_lower_closure_with_ret(
    node: &Node,
    param_tys: &[IrType],
    ret_ty: IrType,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    let saved_out_len = ctx.out.len();
    let saved_stack_len = ctx.tstack.len();
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    match lower_closure_as_value(&node.expr, node.range, param_tys, ret_ty, ctx) {
        Ok(()) => Ok(true),
        Err(LoweringError::StdlibArgTypeMismatch { .. }) => {
            // Body produced a different result slot than `ret_ty`. Roll
            // back so the caller can try the other numeric width.
            ctx.out.truncate(saved_out_len);
            ctx.tstack.truncate(saved_stack_len);
            ctx.next_let_idx = saved_next_let;
            ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Single-closure list HOF kind dispatched by [`emit_list_hof_call`].
#[derive(Clone, Copy)]
pub(super) enum ListHofKind {
    Map,
    Filter,
}

/// Wave R3 / R3b shared emitter for the single-closure list HOFs
/// (`map` / `filter`) over `List<Int>` and `List<Float>` sources,
/// including the element-type-changing numeric `map` (Int -> Float /
/// Float -> Int): speculatively lower `args[0]` (the list source) into
/// a scratch op stream, roll back on a non-list result so dispatch can
/// fall through, then resolve the bundled body from the source element
/// type (and, for `map`, the closure's inferred return type), lower
/// the closure arg, and emit `Op::Call(<builtin>)`. Leaves the
/// callee's result list handle on the vstack.
///
/// R3c extends this to `List<String>` sources and String-result `map`
/// (any source whose closure returns a `String`): the result is a
/// `List<String>` pointer-array record (`[count][off_i]…`, 4-byte slots)
/// the bundled `list_*_map_to_string` / `list_string_map` /
/// `list_string_filter` bodies build directly in scratch. Every stored
/// `off_i` is an arena-relative String handle the closure already
/// produced (no relocation needed), so the return ABI / verifier walk the
/// result unchanged.
///
/// The roll-back discipline (out / tstack / next_let_idx / lambda_table
/// truncation) is identical to [`try_lower_list_filter`]'s so a source
/// that doesn't lower to a supported list leaves the ctx pristine for
/// the regular dispatch path.
fn emit_list_hof_call(
    kind: ListHofKind,
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Speculatively lower the source list into a scratch op stream. The
    // ctx's `out` / `tstack` are swapped out so a non-list (or
    // unsupported element) source rolls back cleanly. The source ops
    // are NOT committed until the whole HOF is decided — for `map` the
    // closure's return-type probe runs first, so a String-returning map
    // (which we cap) leaves the ctx pristine for the regular path.
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    let restore = |ctx: &mut LowerCtx<'_>, out: Vec<TaggedOp>, stack: Vec<IrType>| {
        ctx.out = out;
        ctx.tstack = stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
    };
    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    // `List<Int>` / `List<Float>` sources are in the numeric typed-HOF
    // envelope (8-byte element slots); `List<String>` (R3c) is a 4-byte
    // pointer-array source whose closure param is a `String` handle.
    // Everything else rolls back. `src_elem` is the closure's param type
    // for the element.
    let src_elem = match produced {
        Some(IrType::ListInt) => IrType::I64,
        Some(IrType::ListFloat) => IrType::F64,
        Some(IrType::ListString) => IrType::String,
        _ => {
            restore(ctx, saved_out, saved_stack);
            return Ok(None);
        }
    };
    if lower_res.is_err() {
        restore(ctx, saved_out, saved_stack);
        return Ok(None);
    }
    // Detach the committed source op-stream / type-stack; we splice it
    // back (after the original outer stream) only once the closure half
    // is decided. The closure half is lowered into a FRESH empty buffer
    // so a failed map probe leaves nothing behind.
    let src_stream = std::mem::take(&mut ctx.out);
    let src_stack = std::mem::take(&mut ctx.tstack);

    // Helper: commit the final stream as `saved_out ++ src ++ closure`
    // (and the matching type stacks), then emit the call. `ctx.out` /
    // `ctx.tstack` currently hold the closure-only stream.
    fn commit(
        ctx: &mut LowerCtx<'_>,
        saved_out: Vec<TaggedOp>,
        saved_stack: Vec<IrType>,
        src_stream: Vec<TaggedOp>,
        src_stack: Vec<IrType>,
        builtin: &str,
        range: TokenRange,
    ) -> Result<Option<()>, LoweringError> {
        let closure_stream = std::mem::take(&mut ctx.out);
        let closure_stack = std::mem::take(&mut ctx.tstack);
        ctx.out = saved_out;
        ctx.out.extend(src_stream);
        ctx.out.extend(closure_stream);
        ctx.tstack = saved_stack;
        ctx.tstack.extend(src_stack);
        ctx.tstack.extend(closure_stack);
        finish_list_hof(builtin, range, ctx)
    }

    match kind {
        ListHofKind::Filter => {
            let builtin = match src_elem {
                IrType::I64 => "list_int_filter",
                IrType::F64 => "list_float_filter",
                // `List<String>` filter is kept CAPPED in R3c: although the
                // `list_string_filter` body would build a correct result,
                // no `String -> Bool` predicate currently lowers four-way
                // (the analyzer cannot derive the return type of a String-
                // receiver method predicate, and cranelift does not lower
                // String `Eq`/`Ne`), so the shape is not provable byte-equal
                // and rolls back to the loud cap rather than ship an
                // unverified path.
                _ => {
                    restore(ctx, saved_out, saved_stack);
                    return Ok(None);
                }
            };
            // Resolve the predicate closure signature (`(elem) -> Bool`)
            // from the single side-table source of truth.
            let (param_tys_c, ret_ty_c) =
                stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
                    cap!(
                        "emit_list_int_hof_call.unsupported_expr",
                        LoweringError::UnsupportedExpr {
                            kind: format!("{builtin} closure signature missing"),
                            range,
                        }
                    )
                })?;
            lower_closure_as_value(
                &args[1].value.expr,
                args[1].value.range,
                &param_tys_c,
                ret_ty_c,
                ctx,
            )?;
            commit(
                ctx,
                saved_out,
                saved_stack,
                src_stream,
                src_stack,
                builtin,
                range,
            )
        }
        ListHofKind::Map => {
            // Probe the closure body against each candidate result type
            // for this source, in order: the homogeneous (src -> src)
            // shape first, then the cross-type widths. Each candidate is
            // `(closure_return_type, bundled_body_name)`. The candidates
            // are mutually exclusive — the body yields exactly one result
            // slot — so the first probe that accepts the closure wins, and
            // the result element type comes from the matched return type
            // (R3c extends the R3b "result-type-from-closure-return"
            // selection to `String`).
            //
            // R3c adds `String` as a candidate return for every source
            // (`list_int_map_to_string` / `list_float_map_to_string` /
            // homogeneous `list_string_map`), so a String-returning map
            // now lowers four-way instead of capping.
            let candidates: &[(IrType, &str)] = match src_elem {
                IrType::I64 => &[
                    (IrType::I64, "list_int_map"),
                    (IrType::F64, "list_int_map_to_float"),
                    (IrType::String, "list_int_map_to_string"),
                ],
                IrType::F64 => &[
                    (IrType::F64, "list_float_map"),
                    (IrType::I64, "list_float_map_to_int"),
                    (IrType::String, "list_float_map_to_string"),
                ],
                IrType::String => &[(IrType::String, "list_string_map")],
                _ => unreachable!("src_elem is I64, F64, or String"),
            };
            for &(ret_ty, builtin) in candidates {
                if try_lower_closure_with_ret(&args[1].value, &[src_elem], ret_ty, ctx)? {
                    return commit(
                        ctx,
                        saved_out,
                        saved_stack,
                        src_stream,
                        src_stack,
                        builtin,
                        range,
                    );
                }
            }
            // No candidate return matched. Nothing was committed; restore
            // so the regular dispatch path caps it loudly.
            restore(ctx, saved_out, saved_stack);
            Ok(None)
        }
    }
}

/// Wave R3b/R3c method-form (`xs.map(f)` / `xs.filter(f)`) emitter:
/// dispatch a single-closure list HOF when the **receiver is already
/// lowered** onto the vstack (the generic method-dispatch path lowers the
/// receiver before resolving the method). This is the method-form sibling
/// of [`emit_list_hof_call`] (which lowers the source from `args[0]`): it
/// selects the bundled body from the closure's inferred return type — the
/// element-type-changing numeric maps (Int -> Float / Float -> Int) and
/// the R3c String-result maps (`* -> String`) — instead of the single
/// fixed `(elem -> elem)` signature the `stdlib_method_index` table pins.
///
/// `receiver_ty` is the source list's IR type (its handle is on top of the
/// vstack); `closure` is the lone method argument. Returns `Ok(None)` when
/// the receiver isn't a supported list type or no candidate return matches
/// (the caller then falls through to the regular method dispatch, which
/// caps it loudly). On success the receiver handle has been consumed and
/// the result list handle is on the vstack.
pub(super) fn emit_list_hof_method(
    kind: ListHofKind,
    receiver_ty: IrType,
    closure: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Only literal single-param lambdas are a HOF surface.
    let Expr::Closure { params, .. } = &*closure.expr else {
        return Ok(None);
    };
    if params.len() != 1 {
        return Ok(None);
    }
    let src_elem = match receiver_ty {
        IrType::ListInt => IrType::I64,
        IrType::ListFloat => IrType::F64,
        IrType::ListString => IrType::String,
        _ => return Ok(None),
    };
    // Candidate `(closure_return_type, bundled_body_name)` list, ordered
    // homogeneous-first then cross-type — identical selection to the
    // `emit_list_hof_call` map arm.
    let map_candidates: &[(IrType, &str)] = match src_elem {
        IrType::I64 => &[
            (IrType::I64, "list_int_map"),
            (IrType::F64, "list_int_map_to_float"),
            (IrType::String, "list_int_map_to_string"),
        ],
        IrType::F64 => &[
            (IrType::F64, "list_float_map"),
            (IrType::I64, "list_float_map_to_int"),
            (IrType::String, "list_float_map_to_string"),
        ],
        IrType::String => &[(IrType::String, "list_string_map")],
        _ => unreachable!("src_elem is I64, F64, or String"),
    };
    match kind {
        ListHofKind::Map => {
            for &(ret_ty, builtin) in map_candidates {
                if try_lower_closure_with_ret(closure, &[src_elem], ret_ty, ctx)? {
                    // Receiver handle + closure handle are both on the
                    // vstack now; `finish_list_hof` pops both and emits the
                    // call.
                    return finish_list_hof(builtin, range, ctx);
                }
            }
            Ok(None)
        }
        ListHofKind::Filter => {
            let builtin = match src_elem {
                IrType::I64 => "list_int_filter",
                IrType::F64 => "list_float_filter",
                // `List<String>` filter stays capped in R3c — no provable
                // four-way `String -> Bool` predicate (see the matching
                // note in `emit_list_hof_call`). Fall through to the
                // regular dispatch, which caps it loudly.
                _ => return Ok(None),
            };
            let (param_tys_c, ret_ty_c) =
                stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
                    cap!(
                        "emit_list_int_hof_call.unsupported_expr",
                        LoweringError::UnsupportedExpr {
                            kind: format!("{builtin} closure signature missing"),
                            range,
                        }
                    )
                })?;
            // The predicate signature is fixed (`elem -> Bool`); a body
            // that doesn't match is a loud error, not a fall-through, so
            // use the direct lowering (not the rollback probe).
            lower_closure_as_value(&closure.expr, closure.range, &param_tys_c, ret_ty_c, ctx)?;
            finish_list_hof(builtin, range, ctx)
        }
    }
}

/// Finish a single-closure list HOF: pop the closure + source handles,
/// emit `Op::Call(<builtin>)` with the registry-declared param / ret
/// types, and push the result handle.
fn finish_list_hof(
    builtin: &str,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
        cap!(
            "emit_list_int_hof_call.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: builtin.to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
        cap!(
            "emit_list_int_hof_call.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: builtin.to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let params = meta.params.clone();
    let ret = meta.ret;
    ctx.tstack.pop(); // closure
    ctx.tstack.pop(); // source list handle
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index: fn_idx,
            arg_count: 2,
            param_tys: params,
            ret_ty: ret,
        },
        range,
    });
    ctx.tstack.push(ret);
    Ok(Some(()))
}

/// Wave R3 emitter for `_list_reduce(xs, init, f)` /
/// `xs.reduce(init, f)`: speculatively lower the list source (`args[0]`),
/// roll back on a non-list result, lower the `Int` init (`args[1]`) and
/// the `(I64, I64) -> I64` fold closure (`args[2]`), then emit
/// `Op::Call(list_int_fold)` (return `Int`).
fn emit_list_int_fold_call(
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    // Wave R3b: a `List<Float>` source folds through `list_float_fold`
    // with an F64 accumulator. Roll back the speculative source first
    // (the float emitter re-lowers it), keeping the `List<Int>` path
    // below byte-identical.
    if lower_res.is_ok() && produced == Some(IrType::ListFloat) {
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return emit_list_float_fold_call(args, range, ctx);
    }
    if lower_res.is_err() || produced != Some(IrType::ListInt) {
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return Ok(None);
    }
    let arg_stream = std::mem::replace(&mut ctx.out, saved_out);
    ctx.out.extend(arg_stream);
    let arg_stack = std::mem::replace(&mut ctx.tstack, saved_stack);
    ctx.tstack.extend(arg_stack);

    let fn_idx = stdlib_function_index("list_int_fold").ok_or_else(|| {
        cap!(
            "emit_list_int_fold_call.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: "list_int_fold".to_string(),
                arity: 3,
                range,
            }
        )
    })?;
    let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
        cap!(
            "emit_list_int_fold_call.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: "list_int_fold".to_string(),
                arity: 3,
                range,
            }
        )
    })?;
    let params = meta.params.clone();
    let ret = meta.ret;

    // init (arg 1) must be Int (I64).
    lower_expr(&args[1].value.expr, args[1].value.range, ctx)?;
    let init_ty = ctx.tstack.last().copied();
    if init_ty != Some(IrType::I64) {
        return Err(cap!(
            "emit_list_int_fold_call.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!("_list_reduce(init) must be Int, got {:?}", init_ty),
                range: args[1].value.range,
            }
        ));
    }

    // fold closure (arg 2): (I64, I64) -> I64.
    let (param_tys_c, ret_ty_c) =
        stdlib_closure_arg_signature("list_int_fold", 2).ok_or_else(|| {
            cap!(
                "emit_list_int_fold_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "list_int_fold closure signature missing".to_string(),
                    range,
                }
            )
        })?;
    lower_closure_as_value(
        &args[2].value.expr,
        args[2].value.range,
        &param_tys_c,
        ret_ty_c,
        ctx,
    )?;
    ctx.tstack.pop(); // closure
    ctx.tstack.pop(); // init
    ctx.tstack.pop(); // source list handle
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index: fn_idx,
            arg_count: 3,
            param_tys: params,
            ret_ty: ret,
        },
        range,
    });
    ctx.tstack.push(ret);
    Ok(Some(()))
}

/// Wave R3b emitter for `_list_reduce(xs, init, f)` / `xs.reduce(init,
/// f)` over a `List<Float>` source: folds to an F64 accumulator through
/// the bundled `list_float_fold` body. The source has already been
/// rolled back by the caller, so this re-lowers it. The init expression
/// lowers to F64 (an `Int` literal init like `0` is promoted to F64,
/// mirroring the tree-walk's Int->Float coercion when folding floats);
/// the fold closure is `(F64, F64) -> F64`. `+` inside the body is an
/// IEEE add (no overflow trap) for Float, matching the tree-walker.
fn emit_list_float_fold_call(
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    if lower_res.is_err() || produced != Some(IrType::ListFloat) {
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return Ok(None);
    }
    let arg_stream = std::mem::replace(&mut ctx.out, saved_out);
    ctx.out.extend(arg_stream);
    let arg_stack = std::mem::replace(&mut ctx.tstack, saved_stack);
    ctx.tstack.extend(arg_stack);

    let fn_idx = stdlib_function_index("list_float_fold").ok_or_else(|| {
        cap!(
            "emit_list_int_fold_call.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: "list_float_fold".to_string(),
                arity: 3,
                range,
            }
        )
    })?;
    let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
        cap!(
            "emit_list_int_fold_call.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: "list_float_fold".to_string(),
                arity: 3,
                range,
            }
        )
    })?;
    let params = meta.params.clone();
    let ret = meta.ret;

    // init (arg 1) must lower to F64. An Int literal init is promoted.
    lower_expr(&args[1].value.expr, args[1].value.range, ctx)?;
    match ctx.tstack.last().copied() {
        Some(IrType::F64) => {}
        Some(IrType::I64) => {
            ctx.out.push(TaggedOp {
                op: Op::ConvertI64ToF64,
                range: args[1].value.range,
            });
            ctx.tstack.pop();
            ctx.tstack.push(IrType::F64);
        }
        other => {
            return Err(cap!(
                "emit_list_int_fold_call.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("_list_reduce(init) must be Float, got {:?}", other),
                    range: args[1].value.range,
                }
            ));
        }
    }

    let (param_tys_c, ret_ty_c) =
        stdlib_closure_arg_signature("list_float_fold", 2).ok_or_else(|| {
            cap!(
                "emit_list_int_fold_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "list_float_fold closure signature missing".to_string(),
                    range,
                }
            )
        })?;
    lower_closure_as_value(
        &args[2].value.expr,
        args[2].value.range,
        &param_tys_c,
        ret_ty_c,
        ctx,
    )?;
    ctx.tstack.pop(); // closure
    ctx.tstack.pop(); // init
    ctx.tstack.pop(); // source list handle
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index: fn_idx,
            arg_count: 3,
            param_tys: params,
            ret_ty: ret,
        },
        range,
    });
    ctx.tstack.push(ret);
    Ok(Some(()))
}

/// Wave R3: general `range(a, b)` / `range(b)` as a materialised
/// `List<Int>` *value* (not folded inside an eliding `list.sum` /
/// `_len` consumer). Fires only on a bare `range(...)` free-call — any
/// trailing `.map(...)` / `.filter(...)` is owned by the chain
/// consumers above (which run first) or, for the list-producing value
/// forms, by `try_lower_list_map` / `try_lower_list_filter` whose
/// speculative source lowering re-enters this peephole for the inner
/// `range(...)`.
///
/// Reuses [`emit_range_materialize`] (the AOT-4 W18 slice's runtime
/// range record builder): `[len: u32 LE][pad][i64 elements...]`,
/// payload at `(base + 11) & -8`, with the `start >= end -> []` clamp
/// the tree-walker `range` applies. The result is a real `List<Int>`
/// handle so `range(n)` can be returned, indexed, summed, or piped into
/// `.map` / `.filter` / `reduce`. Returns `Ok(Some(()))` on a match,
/// `Ok(None)` otherwise.
pub(super) fn try_lower_range_value(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Bare `range(...)` head: single path segment `range`, 1 or 2
    // positional args (start defaults to 0), no keyword form. Mirrors
    // `match_bare_range` without constructing a temporary `Expr`.
    if path.len() != 1 || !matches!(&path[0], TokenKey::String(s, _, _) if s == "range") {
        return Ok(None);
    }
    if args.is_empty() || args.len() > 2 || args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    emit_range_materialize(args, range, ctx)?;
    Ok(Some(()))
}

/// Wave R4: static const-fold of the `type(v)` builtin.
///
/// In strict mode the argument's type is statically known after
/// lowering it, so `type(v)` reduces to a constant type-name String —
/// pure static, no runtime value model. The lowered shape is:
///
/// 1. Lower `<arg>` normally, so its value lands on the vstack **and
///    any traps inside it fire** (the tree-walk `type` builtin
///    evaluates its argument before reading `Value::type_name`, so an
///    overflowing sub-expression must trap identically — we never skip
///    evaluating `v`).
/// 2. Map the produced IR type to the canonical type-name string via
///    the single source of truth [`IrType::type_name`] (asserted equal
///    to `Value::type_name` in a `relon-ir` unit test). This COARSENS:
///    every `List*` -> "List", `Dict` (plain or branded) -> "Dict".
/// 3. Discard the evaluated value by storing it into a fresh, never-read
///    let-local (`Op::LetSet`), then push `Op::ConstString(<name>)`.
///
/// Returns `Ok(Some(()))` when the fold fired (leaves a single
/// `IrType::String` on the vstack), `Ok(None)` when the shape does not
/// match or the argument's type is not statically nameable (a wasm
/// handshake `I32` slot) — in the latter case the speculative arg
/// lowering is rolled back and dispatch falls through to the existing
/// `unknown_stdlib_method` cap. `Err` only on an argument that fails to
/// lower.
///
/// Out of scope (kept capped, Wave R6): `type()` over a `#relaxed` /
/// boxed value whose type is not statically determinable — those never
/// reach a concrete `IrType` here and surface through the normal cap.
pub(super) fn try_lower_type_const(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Bare `type(<arg>)`: single path segment `type`, exactly one
    // positional argument, no keyword form.
    if path.len() != 1 || !matches!(&path[0], TokenKey::String(s, _, _) if s == "type") {
        return Ok(None);
    }
    if args.len() != 1 || args[0].name.is_some() {
        return Ok(None);
    }

    // Snapshot so an unnameable argument type rolls the speculative arg
    // lowering back cleanly and dispatch falls through to the cap path.
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();

    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    let type_name = produced.and_then(IrType::type_name);

    let (Ok(()), Some(arg_ty), Some(name)) = (lower_res, produced, type_name) else {
        // Either the argument failed to lower, produced nothing, or
        // produced a non-user-facing slot we refuse to name. Roll back
        // and let the default dispatch raise the existing cap.
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return Ok(None);
    };

    // Re-attach the argument's op stream / vstack (the arg evaluation
    // is KEPT for trap + side-effect-ordering parity).
    let arg_stream = std::mem::replace(&mut ctx.out, saved_out);
    ctx.out.extend(arg_stream);
    let arg_stack = std::mem::replace(&mut ctx.tstack, saved_stack);
    ctx.tstack.extend(arg_stack);

    // Discard the evaluated value: store it into a fresh let-local that
    // is never read. This pops the vstack and keeps the evaluation in
    // the op stream (its traps already fired). The let-local is internal
    // — not registered in `ctx.lets` — so no source name can read it;
    // both backends lazily declare the slot on first `LetSet`.
    let discard_idx = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: discard_idx,
            ty: arg_ty,
        },
        range,
    });
    ctx.tstack.pop(); // the discarded argument value

    // Push the constant type name (interned through the module-wide
    // const-string table, same path as a source `Expr::String`).
    let str_idx = ctx.const_intern.borrow_mut().strings.intern(name);
    ctx.out.push(TaggedOp {
        op: Op::ConstString {
            idx: str_idx,
            value: name.to_string(),
        },
        range,
    });
    ctx.tstack.push(IrType::String);
    Ok(Some(()))
}

/// Wave R3: general `_list_map(xs, (x) => <body>)` over an arbitrary
/// `List<Int>`-typed first argument. Mirror of [`try_lower_list_filter`]
/// — speculatively lowers the list source (so a bare `range(...)`, a
/// where-bound list, a `#main` `List<Int>` param, or a nested
/// `_list_map` / `_list_filter` result all flow through), commits only
/// when it produced an `IrType::ListInt` handle, then lowers the
/// transform closure (`(I64) -> I64`) and emits
/// `Op::Call(list_int_map)`, leaving a fresh `List<Int>` handle.
///
/// The bundled `list_int_map` body dispatches the closure per element
/// via `Op::CallClosure` (the proven four-way closure substrate), so the
/// emitted code applies the same per-element transform in source order —
/// byte-identical to the tree-walk `ListMap` (`results.push(f(item))`).
pub(super) fn try_lower_list_map(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    if path.len() != 1 || args.len() != 2 || args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "_list_map") {
        return Ok(None);
    }
    let Expr::Closure { params, .. } = &*args[1].value.expr else {
        return Ok(None);
    };
    if params.len() != 1 {
        return Ok(None);
    }
    emit_list_hof_call(ListHofKind::Map, args, range, ctx)
}

/// Wave R3: general `_list_reduce(xs, <init>, (acc, x) => <body>)` over
/// an arbitrary `List<Int>`-typed first argument, folding to an `Int`
/// accumulator. Speculatively lowers the list source (same composition
/// envelope as [`try_lower_list_map`] / [`try_lower_list_filter`]),
/// commits only when it produced an `IrType::ListInt` handle, lowers the
/// `Int` init expression and the `(I64, I64) -> I64` fold closure, then
/// emits `Op::Call(list_int_fold)`.
///
/// `list_int_fold`'s body matches the tree-walk `ListReduce` exactly:
/// `acc = init`, then `acc = f(acc, item)` per element in source order,
/// returning `acc`. The fold body lowers normally, so a `+` inside it
/// stays a CHECKED i64 add that traps `NumericOverflow` identically to
/// the tree-walker.
pub(super) fn try_lower_list_reduce(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    if path.len() != 1 || args.len() != 3 || args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "_list_reduce") {
        return Ok(None);
    }
    let Expr::Closure { params, .. } = &*args[2].value.expr else {
        return Ok(None);
    };
    if params.len() != 2 {
        return Ok(None);
    }
    emit_list_int_fold_call(args, range, ctx)
}

/// AOT-4 (W16 slice): general `list.sum(xs)` over an arbitrary
/// `List<Int>`-typed argument (vs the `list.sum(range(...))` eliding
/// peephole, which folds a raw range without materialising). Lowers the
/// argument speculatively; commits only when it produced an
/// `IrType::ListInt` handle, then emits `Op::Call(list_int_sum)` (the
/// bundled i64-accumulator body) — used by the W16 quicksort kernel's
/// `list.sum(_list_filter(xs, (x) => x == xs[0]))` equal-partition fold.
pub(super) fn try_lower_list_sum_value(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    if path.len() != 2 || args.len() != 1 || args[0].name.is_some() {
        return Ok(None);
    }
    let outer_head = matches!(&path[0], TokenKey::String(s, _, _) if s == "list");
    let outer_method = matches!(&path[1], TokenKey::String(s, _, _) if s == "sum");
    if !(outer_head && outer_method) {
        return Ok(None);
    }
    // Speculatively lower the argument; roll back on a non-list result.
    let saved_out = std::mem::take(&mut ctx.out);
    let saved_stack = std::mem::take(&mut ctx.tstack);
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    let lower_res = lower_expr(&args[0].value.expr, args[0].value.range, ctx);
    let produced = ctx.tstack.last().copied();
    if lower_res.is_err() || produced != Some(IrType::ListInt) {
        ctx.out = saved_out;
        ctx.tstack = saved_stack;
        ctx.next_let_idx = saved_next_let;
        ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
        return Ok(None);
    }
    let arg_stream = std::mem::replace(&mut ctx.out, saved_out);
    ctx.out.extend(arg_stream);
    let arg_stack = std::mem::replace(&mut ctx.tstack, saved_stack);
    ctx.tstack.extend(arg_stack);

    let sum_idx = stdlib_function_index("list_int_sum").ok_or_else(|| {
        cap!(
            "try_lower_list_sum_value.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: "list_int_sum".to_string(),
                arity: 1,
                range,
            }
        )
    })?;
    let sum_meta = builtin_stdlib().get(sum_idx as usize).ok_or_else(|| {
        cap!(
            "try_lower_list_sum_value.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: "list_int_sum".to_string(),
                arity: 1,
                range,
            }
        )
    })?;
    let sum_params = sum_meta.params.clone();
    let sum_ret = sum_meta.ret;
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index: sum_idx,
            arg_count: 1,
            param_tys: sum_params,
            ret_ty: sum_ret,
        },
        range,
    });
    ctx.tstack.pop(); // ListInt handle
    ctx.tstack.push(sum_ret);
    Ok(Some(()))
}

/// Match a bare `range(a, b)` / `range(b)` call, returning its arg
/// slice. Unlike [`match_range_chain`] this rejects any trailing
/// `.map(...)` / `.filter(...)` stages — the W18 slice materialises the
/// raw range before handing it to `list_int_filter`.
pub(super) fn match_bare_range(expr: &Expr) -> Option<&[relon_parser::CallArg]> {
    let Expr::FnCall { path, args } = expr else {
        return None;
    };
    if path.len() != 1 || !matches!(&path[0], TokenKey::String(s, _, _) if s == "range") {
        return None;
    }
    if args.is_empty() || args.len() > 2 || args.iter().any(|a| a.name.is_some()) {
        return None;
    }
    Some(args)
}

/// AOT-4 (W18 slice): emit the IR that materialises a runtime
/// `range(a, b)` (or `range(b)`, start defaulting to 0) into a fresh
/// `List<Int>` scratch record and leaves its arena-relative handle on
/// the vstack tagged `IrType::ListInt`.
///
/// Record layout (must match the bundled stdlib contract — see
/// `stdlib::defs::list_int_filter_body`): `[len: u32 LE][pad: u32
/// zero][i64 elements...]`, total `8 + 8*count` bytes, payload aligned
/// at `(base + 4 + 7) & -8`.
///
/// Emitted shape (all address arithmetic in I32, element value I64):
///
/// ```text
/// start = a; end = b
/// count = max(end - start, 0)              ; i32 (truncated)
/// base  = AllocScratchDyn(8 + 8*count)     ; i32 handle
/// i32.store(base, count)                   ; header len prefix
/// payload = (base + 4 + 7) & -8
/// i = 0
/// block { loop {
///   if i >= count { br 1 }
///   i64.store(payload + i*8, start + i)    ; element
///   i = i + 1
///   br 0
/// } }
/// push base                                ; ListInt handle
/// ```
/// #359 (W20): materialise a `List<Float>` literal `[e0, e1, .., eN]`
/// (each `ei` a Float-valued expression) into a fresh scratch arena
/// record and leave its arena-relative handle on the vstack tagged
/// `IrType::ListFloat`. The record layout is byte-identical to the
/// `List<Int>` materialiser (`[len: u32 LE][pad: u32][8-byte
/// elements...]`, payload at `(base + 4 + 7) & -8`) so the shared 1D
/// index path (`lower_list_index_typed`) reads it unchanged — only the
/// element store is an `f64` (the value's bit pattern) rather than an
/// `i64`.
///
/// The element count is a compile-time constant (the literal's
/// length), so the stores are unrolled — no fill loop. This handles
/// both the W20 `init` 8-element literal and the per-step `step(s)`
/// body literal (whose elements are computed Float arithmetic over the
/// previous state's indexed reads). Each element expression is lowered
/// against the live ctx so a closure body's `s[k]` reads + `dt` / mass
/// captures resolve through the normal walker.
pub(super) fn emit_list_float_literal_materialize(
    items: &[Node],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let count = i32::try_from(items.len()).map_err(|_| {
        cap!(
            "emit_list_float_literal_materialize.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "List<Float>(literal too long for i32 count)".to_string(),
                range,
            }
        )
    })?;
    let base_i = ctx.next_let_idx;
    let payload_i = ctx.next_let_idx + 1;
    ctx.next_let_idx += 2;

    // record_size = 8 + 8*count (constant). base = AllocScratchDyn(size).
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8 + 8 * count),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::AllocScratchDyn,
        range,
    });
    // AllocScratchDyn: [i32 size] -> [i32 base]; tstack stays I32.
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // header: i32.store(base, count)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(count),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::StoreI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();

    // payload = (base + 4 + 7) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // Unrolled element stores. For element `i`:
    //   addr = payload + i*8   (i32)
    //   value = <lowered ei>   (F64 bits, an i64 on the operand stack)
    //   f64.store(addr, value)
    // Stack discipline for StoreF64AtAbsolute mirrors the i64 store:
    // [addr(i32), value(F64)].
    for (i, item) in items.iter().enumerate() {
        // addr = payload + i*8
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: payload_i,
                ty: IrType::I32,
            },
            range,
        });
        ctx.tstack.push(IrType::I32);
        let byte_off = i32::try_from(i * 8).map_err(|_| {
            cap!(
                "emit_list_float_literal_materialize.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "List<Float>(element offset overflow)".to_string(),
                    range,
                }
            )
        })?;
        ctx.out.push(TaggedOp {
            op: Op::ConstI32(byte_off),
            range,
        });
        ctx.tstack.push(IrType::I32);
        ctx.out.push(TaggedOp {
            op: Op::Add(IrType::I32),
            range,
        });
        ctx.tstack.pop();
        ctx.tstack.pop();
        ctx.tstack.push(IrType::I32);

        // value = lowered element; coerce Int literals to F64 (mirrors
        // the runtime Int->Float promotion + the const-list arm's
        // `[1, 2.0]` widening).
        lower_expr(&item.expr, item.range, ctx)?;
        let elem_ty = ctx.tstack.last().copied().ok_or_else(|| {
            cap!(
                "emit_list_float_literal_materialize.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: "List<Float>(element produced no value)".to_string(),
                    range: item.range,
                }
            )
        })?;
        match elem_ty {
            IrType::F64 => {}
            IrType::I64 => {
                ctx.out.push(TaggedOp {
                    op: Op::ConvertI64ToF64,
                    range: item.range,
                });
                ctx.tstack.pop();
                ctx.tstack.push(IrType::F64);
            }
            other => {
                return Err(cap!(
                    "emit_list_float_literal_materialize.unsupported_expr.4",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "List<Float>(element #{i} lowered to {other:?}, expected Float)"
                        ),
                        range: item.range,
                    }
                ));
            }
        }
        ctx.out.push(TaggedOp {
            op: Op::StoreF64AtAbsolute { offset: 0 },
            range,
        });
        ctx.tstack.pop(); // value (F64)
        ctx.tstack.pop(); // addr (i32)
    }

    // Push the materialised list handle (base) tagged ListFloat.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::ListFloat);
    Ok(())
}

/// Symmetric to `emit_list_float_literal_materialize`: materialise a
/// computed `List<Int>` literal `[e0, e1, .., eN]` (each `ei` an
/// Int-valued expression that is not a plain literal — so it cannot be
/// interned as a `ConstListInt`) into a fresh scratch arena record and
/// leave its arena-relative handle on the vstack tagged
/// `IrType::ListInt`.
///
/// The record layout is byte-identical to the const / range-map
/// `List<Int>` materialisers (`[len: u32 LE][pad: u32][8-byte
/// elements...]`, payload at `(base + 4 + 7) & -8`, 8-byte stride), so
/// the shared 1D index path (`lower_list_int_index` /
/// `lower_list_index_typed`) and `list.sum` read it unchanged. The only
/// difference from the Float materialiser is the element store op
/// (`StoreI64AtAbsolute` instead of `StoreF64AtAbsolute`) and the
/// element-value coercion (Int stays I64; no I64->F64 conversion).
///
/// The element count is a compile-time constant (the literal's length),
/// so the stores are unrolled — no fill loop. Each element expression is
/// lowered against the live ctx so closure-body indexed reads / captures
/// resolve through the normal walker, mirroring the Float path.
pub(super) fn emit_list_int_literal_materialize(
    items: &[Node],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let count = i32::try_from(items.len()).map_err(|_| {
        cap!(
            "emit_list_int_literal_materialize.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "List<Int>(literal too long for i32 count)".to_string(),
                range,
            }
        )
    })?;
    let base_i = ctx.next_let_idx;
    let payload_i = ctx.next_let_idx + 1;
    ctx.next_let_idx += 2;

    // record_size = 8 + 8*count (constant). base = AllocScratchDyn(size).
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8 + 8 * count),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::AllocScratchDyn,
        range,
    });
    // AllocScratchDyn: [i32 size] -> [i32 base]; tstack stays I32.
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // header: i32.store(base, count)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(count),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::StoreI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();

    // payload = (base + 4 + 7) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // Unrolled element stores. For element `i`:
    //   addr = payload + i*8   (i32)
    //   value = <lowered ei>   (i64 on the operand stack)
    //   i64.store(addr, value)
    // Stack discipline for StoreI64AtAbsolute: [addr(i32), value(I64)].
    for (i, item) in items.iter().enumerate() {
        // addr = payload + i*8
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: payload_i,
                ty: IrType::I32,
            },
            range,
        });
        ctx.tstack.push(IrType::I32);
        let byte_off = i32::try_from(i * 8).map_err(|_| {
            cap!(
                "emit_list_int_literal_materialize.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "List<Int>(element offset overflow)".to_string(),
                    range,
                }
            )
        })?;
        ctx.out.push(TaggedOp {
            op: Op::ConstI32(byte_off),
            range,
        });
        ctx.tstack.push(IrType::I32);
        ctx.out.push(TaggedOp {
            op: Op::Add(IrType::I32),
            range,
        });
        ctx.tstack.pop();
        ctx.tstack.pop();
        ctx.tstack.push(IrType::I32);

        // value = lowered element; require it to be Int-shaped (I64).
        lower_expr(&item.expr, item.range, ctx)?;
        let elem_ty = ctx.tstack.last().copied().ok_or_else(|| {
            cap!(
                "emit_list_int_literal_materialize.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: "List<Int>(element produced no value)".to_string(),
                    range: item.range,
                }
            )
        })?;
        match elem_ty {
            IrType::I64 => {}
            other => {
                return Err(cap!(
                    "emit_list_int_literal_materialize.unsupported_expr.4",
                    LoweringError::UnsupportedExpr {
                        kind: format!("List<Int>(element #{i} lowered to {other:?}, expected Int)"),
                        range: item.range,
                    }
                ));
            }
        }
        ctx.out.push(TaggedOp {
            op: Op::StoreI64AtAbsolute { offset: 0 },
            range,
        });
        ctx.tstack.pop(); // value (I64)
        ctx.tstack.pop(); // addr (i32)
    }

    // Push the materialised list handle (base) tagged ListInt.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::ListInt);
    Ok(())
}

/// `true` when the list literal's elements are NOT all simple scalar
/// literals — i.e. at least one element is a computed expression (the
/// W20 `step(s)` body), so it must be materialised at runtime rather
/// than interned as a `ConstList*`.
///
/// W5-P2: `String` and `Bool` literals are simple constants too —
/// `["a", "b"]` interns straight into a `ConstListString` record, so
/// they belong on the non-computed side. Routing them through the
/// runtime materialiser (which only knows Float / Int) would
/// mis-reject an all-literal `List<String>` / `List<Bool>`.
pub(super) fn list_has_computed_element(items: &[Node]) -> bool {
    items.iter().any(|n| {
        !matches!(
            &*n.expr,
            Expr::Float(_) | Expr::Int(_) | Expr::String(_) | Expr::Bool(_)
        )
    })
}

/// #359 (W20): speculatively lower `node` against the live ctx, read
/// the IR type it leaves on top of the vstack, then roll back every
/// side effect (emitted ops, vstack entries, let-table pushes, the let
/// counter) so the caller can re-lower from a clean state. Used to
/// classify a computed list literal's element shape before committing
/// to the matching materialiser.
pub(super) fn probe_expr_ir_ty(
    node: &Node,
    ctx: &mut LowerCtx<'_>,
) -> Result<IrType, LoweringError> {
    let out_len = ctx.out.len();
    let tstack_len = ctx.tstack.len();
    let lets_len = ctx.lets.len();
    let next_let = ctx.next_let_idx;
    lower_expr(&node.expr, node.range, ctx)?;
    let ty = ctx.tstack.last().copied().ok_or_else(|| {
        cap!(
            "probe_expr_ir_ty.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: "List(computed element produced no value during type probe)".to_string(),
                range: node.range,
            }
        )
    })?;
    // Roll back all side effects so the real materialiser re-lowers the
    // element from scratch (no duplicate ops, no leaked let slots).
    ctx.out.truncate(out_len);
    ctx.tstack.truncate(tstack_len);
    ctx.lets.truncate(lets_len);
    ctx.next_let_idx = next_let;
    Ok(ty)
}

/// `true` when the list literal is Float-shaped: at least one element
/// is a Float literal, or (for an all-computed list) the first element
/// is a Float-producing expression shape. Conservative — only used to
/// route a *computed* list literal to the Float materialiser; an
/// all-Int-literal list still flows through the `ConstListInt` arm.
pub(super) fn list_is_float_shaped(items: &[Node]) -> bool {
    items.iter().any(|n| matches!(&*n.expr, Expr::Float(_)))
        || items
            .first()
            .is_some_and(|n| expr_looks_float_valued(&n.expr))
}

/// Structural "does this expression look Float-valued?" check for the
/// computed-list-literal router. Recognises Float literals, Float
/// arithmetic, and ternaries with a Float arm. Used only to disambiguate
/// a computed list's element shape (`step(s)`'s `s[k] + s[k]*dt` is
/// Float); it never has to be exhaustive — a wrong guess simply falls
/// through to the existing diagnostic.
fn expr_looks_float_valued(expr: &Expr) -> bool {
    match expr {
        Expr::Float(_) => true,
        Expr::Binary(op, l, r) => {
            !operator_yields_bool(*op)
                && (expr_looks_float_valued(&l.expr) || expr_looks_float_valued(&r.expr))
        }
        Expr::Unary(_, n) => expr_looks_float_valued(&n.expr),
        Expr::Ternary { then, els, .. } => {
            expr_looks_float_valued(&then.expr) || expr_looks_float_valued(&els.expr)
        }
        _ => false,
    }
}

fn emit_range_materialize(
    range_args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Reserve let slots. `start` / `span` / `elem` ride as I64;
    // `count`, `base`, `payload`, `i` are all I32 (address arithmetic
    // + the loop counter), mirroring `list_int_filter_body`. Each slot
    // is single-typed for its whole lifetime — the LLVM emitter
    // rejects a let-slot reused under two IR types (`ensure_let_slot`
    // aliasing guard). `elem` is the running i64 element value (start
    // + i); carried in a dedicated I64 slot so the I32 loop counter
    // `i` is never read back widened (which would alias its slot).
    let start_i = ctx.next_let_idx;
    let span_i = ctx.next_let_idx + 1;
    let count_i = ctx.next_let_idx + 2;
    let base_i = ctx.next_let_idx + 3;
    let payload_i = ctx.next_let_idx + 4;
    let i_i = ctx.next_let_idx + 5;
    let elem_i = ctx.next_let_idx + 6;
    ctx.next_let_idx += 7;

    // start = a (or 0 for the 1-arg `range(b)` form).
    if range_args.len() == 2 {
        lower_expr(&range_args[0].value.expr, range_args[0].value.range, ctx)?;
        expect_int_top(ctx, range)?;
    } else {
        ctx.out.push(TaggedOp {
            op: Op::ConstI64(0),
            range,
        });
        ctx.tstack.push(IrType::I64);
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // span = end - start. Stashed in a dedicated I64 slot so the
    // clamp below can re-read it without juggling the operand stack.
    let end_arg = &range_args[range_args.len() - 1];
    lower_expr(&end_arg.value.expr, end_arg.value.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::Sub(IrType::I64),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: span_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // count = span > 0 ? span : 0, then truncate into the I32 `count`
    // slot. The clamp guards `range(b, a)` with `b > a` so it yields
    // an empty list (matches the tree-walker `range`, which produces
    // `[]` when start >= end) and keeps the `AllocScratchDyn` size
    // non-negative. An `Op::If` is used rather than `Op::Select`
    // because the LLVM emitter lowers `If` (both arms leave an I64)
    // but has no `Select` arm.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: span_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(0),
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::Gt(IrType::I64),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::Bool);
    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty: IrType::I64,
            then_body: vec![TaggedOp {
                op: Op::LetGet {
                    idx: span_i,
                    ty: IrType::I64,
                },
                range,
            }],
            else_body: vec![TaggedOp {
                op: Op::ConstI64(0),
                range,
            }],
        },
        range,
    });
    ctx.tstack.pop(); // the Bool predicate
    ctx.tstack.push(IrType::I64); // the If's I64 result
                                  // Truncate the clamped span into the I32 `count` slot.
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // record_size = 8 + 8*count  (i32 arithmetic; matches
    // list_int_filter_body's `16 + 8*n` header sizing minus the
    // filter's extra slack — we size exactly to `count`).
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::AllocScratchDyn,
        range,
    });
    // AllocScratchDyn: [i32 size] -> [i32 base]. tstack stays I32.
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // header: i32.store(base, count)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::StoreI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();

    // payload = (base + 4 + 7) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // i = 0
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(0),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // elem = start (the first element value; advanced by 1 per iter)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // Fill loop: redirect ctx.out into a sub-buffer for the loop body
    // (mirrors `emit_range_pipeline_loop`'s splice dance).
    let saved_outer = std::mem::take(&mut ctx.out);

    // exit when i >= count -> br 1 (out of the loop-exit block)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Ge(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::BrIf { label_depth: 1 },
        range,
    });

    // addr = payload + i*8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    // value = elem (the running i64 element value start + i). Read
    // from the dedicated I64 slot so the I32 loop counter is never
    // read back widened (which would alias its let-slot).
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    // i64.store(addr, value) — Stack discipline: [addr(i32), value(i64)].
    ctx.out.push(TaggedOp {
        op: Op::StoreI64AtAbsolute { offset: 0 },
        range,
    });

    // i = i + 1
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    // elem = elem + 1 (keeps the i64 element value in lock-step with i)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Br { label_depth: 0 },
        range,
    });

    // Wrap the loop body under Block { Loop { ... } }.
    let loop_body = std::mem::replace(&mut ctx.out, saved_outer);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: loop_body,
                },
                range,
            }],
        },
        range,
    });

    // Push the materialised list handle (base) tagged ListInt.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::ListInt);
    Ok(())
}

/// Recognise a where-bound list value the AOT-4 materialiser can build:
/// either a bare `range(a, b)` (1D `List<Int>`) or a single-stage
/// `range(a, b).map((p) => <inner>)` whose `<inner>` is itself a
/// materialisable list (nested -> `List<List<Int>>`) or an `Int`-valued
/// scalar (`List<Int>`). Returns the outer range args + the map closure
/// when the `.map` form matches; `None` for the bare-range / unmatched
/// forms (the caller handles bare range via `match_bare_range`).
pub(super) fn match_materializable_outer_map(
    expr: &Expr,
) -> Option<(&[relon_parser::CallArg], &ClosureParam, &Node)> {
    let chain = match_range_chain(expr)?;
    // Exactly one `map` stage (a 2D row builder). `filter` stages and
    // multi-stage pipelines are out of scope for the where-bound
    // materialiser — those flow through the eliding consumers.
    if chain.stages.len() != 1 || chain.stages[0].method != "map" {
        return None;
    }
    let stage = &chain.stages[0];
    if stage.closure_params.len() != 1 {
        return None;
    }
    Some((
        chain.range_args,
        &stage.closure_params[0],
        stage.closure_body,
    ))
}

/// AOT-4 (W19 slice): materialise a where-bound list value into an arena
/// record, leaving its `ListInt` handle on the vstack. Dispatches on the
/// value shape:
///
///   * bare `range(a, b)` / `range(b)` -> [`emit_range_materialize`]
///     (1D `List<Int>`);
///   * `range(a, b).map((p) => <inner>)` -> an outer `List<Int>` record
///     whose i-th i64 element is either the materialised inner row's i32
///     arena handle (when `<inner>` is itself a materialisable list, so
///     the outer record is a `List<List<Int>>`) or the `<inner>` scalar
///     cell value directly (when `<inner>` is `Int`-valued).
///
/// The outer fill-loop binds the map closure param `p` to the running
/// element value (`start + i`), lowers `<inner>` inline against the
/// outer ctx (so captures like a prior where-bound `a` / `b` resolve
/// through the normal let table), widens an inner `ListInt` handle to
/// the i64 element slot (`LetSet{I64}` zero-extends an i32), and stores
/// it at `payload + i*8`. Same record layout + payload alignment as
/// [`emit_range_materialize`] so the result flows through the bundled
/// `list_int_*` bodies and the inline index path unchanged.
pub(super) fn emit_list_value_materialize(
    value_expr: &Expr,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Bare range -> reuse the 1D materialiser.
    if let Some(range_args) = match_bare_range(value_expr) {
        return emit_range_materialize(range_args, range, ctx);
    }
    let Some((range_args, param, inner_body)) = match_materializable_outer_map(value_expr) else {
        return Err(cap!(
            "emit_list_value_materialize.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind:
                    "where-bound list value is neither a bare range nor a range().map((p) => ...)"
                        .to_string(),
                range,
            }
        ));
    };

    // Slot plan (all distinct, single-typed for their lifetime — the
    // LLVM emitter's `ensure_let_slot` aliasing guard requires it, so
    // the I64 `span` and the I32 `count` are SEPARATE slots).
    let start_i = ctx.next_let_idx;
    let span_i = ctx.next_let_idx + 1; // I64 clamped element count
    let count_i = ctx.next_let_idx + 2; // I32 element count
    let base_i = ctx.next_let_idx + 3;
    let payload_i = ctx.next_let_idx + 4;
    let i_i = ctx.next_let_idx + 5;
    let elem_i = ctx.next_let_idx + 6; // running i64 element (start + i), bound to `p`
    let val_i = ctx.next_let_idx + 7; // i64 element value to store
    ctx.next_let_idx += 8;

    // start = a (or 0 for `range(b)`).
    if range_args.len() == 2 {
        lower_expr(&range_args[0].value.expr, range_args[0].value.range, ctx)?;
        expect_int_top(ctx, range)?;
    } else {
        ctx.out.push(TaggedOp {
            op: Op::ConstI64(0),
            range,
        });
        ctx.tstack.push(IrType::I64);
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // count = clamp(end - start, 0), truncated to i32. `Op::If` (not
    // `Select`, which the LLVM emitter has no arm for) yields the
    // clamped i64; the trailing `LetSet{I32}` truncates.
    let end_arg = &range_args[range_args.len() - 1];
    lower_expr(&end_arg.value.expr, end_arg.value.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::Sub(IrType::I64),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I64);
    // span on top: clamp to >= 0 via If.
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: span_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: span_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(0),
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::Gt(IrType::I64),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::Bool);
    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty: IrType::I64,
            then_body: vec![TaggedOp {
                op: Op::LetGet {
                    idx: span_i,
                    ty: IrType::I64,
                },
                range,
            }],
            else_body: vec![TaggedOp {
                op: Op::ConstI64(0),
                range,
            }],
        },
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // record_size = 8 + 8*count
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::AllocScratchDyn,
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // header: i32.store(base, count)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::StoreI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();

    // payload = (base + 4 + 7) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // i = 0
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(0),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // elem = start
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // Bind the map closure param `p` to `elem` for the inner body.
    // Source value is read live each iteration (the loop body re-reads
    // the `elem` slot), so the binding is just a name->slot alias.
    let param_let = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.lets.push(LetBinding {
        name: param.name.clone(),
        idx: param_let,
        ty: IrType::I64,
        schema_brand: None,
    });

    // Fill loop: redirect ctx.out into a sub-buffer for the body.
    let saved_outer = std::mem::take(&mut ctx.out);

    // exit when i >= count -> br 1
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: count_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Ge(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::BrIf { label_depth: 1 },
        range,
    });

    // p = elem (the running i64 element value).
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: param_let,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();

    // Lower the inner element value. A nested materialisable list ->
    // recurse (produces a `ListInt` i32 handle); otherwise lower the
    // scalar `Int` cell expression directly (produces `I64`). Either
    // way the result is normalised into the i64 `val` slot:
    // `LetSet{I64}` zero-extends an i32 handle and is a no-op width
    // match for an i64 cell.
    let inner_is_list = match_bare_range(&inner_body.expr).is_some()
        || match_materializable_outer_map(&inner_body.expr).is_some();
    if inner_is_list {
        emit_list_value_materialize(&inner_body.expr, inner_body.range, ctx)?;
        let produced = ctx.tstack.pop().ok_or(cap!(
            "emit_list_value_materialize.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: "where-bound 2D materialise: inner row produced no value".to_string(),
                range: inner_body.range,
            }
        ))?;
        debug_assert_eq!(produced, IrType::ListInt);
        // Widen the i32 row handle into the i64 element slot.
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: val_i,
                ty: IrType::I64,
            },
            range,
        });
    } else {
        lower_expr(&inner_body.expr, inner_body.range, ctx)?;
        let produced = ctx.tstack.pop().ok_or(cap!(
            "emit_list_value_materialize.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: "where-bound list materialise: map body produced no value".to_string(),
                range: inner_body.range,
            }
        ))?;
        if produced != IrType::I64 {
            // Restore the op stream before surfacing the diagnostic.
            ctx.lets.pop();
            let _ = std::mem::replace(&mut ctx.out, saved_outer);
            return Err(cap!(
                "emit_list_value_materialize.unsupported_expr.4",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                    "where-bound list materialise: map body must be Int- or list-valued, got {:?}",
                    produced
                ),
                    range: inner_body.range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: val_i,
                ty: IrType::I64,
            },
            range,
        });
    }

    // addr = payload + i*8 ; i64.store(addr, val)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: payload_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: val_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::StoreI64AtAbsolute { offset: 0 },
        range,
    });

    // i += 1 ; elem += 1 ; br 0
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: i_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: elem_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Br { label_depth: 0 },
        range,
    });

    // Wrap under Block { Loop { ... } }.
    let loop_body = std::mem::replace(&mut ctx.out, saved_outer);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: loop_body,
                },
                range,
            }],
        },
        range,
    });

    // Pop the `p` binding now the body is closed.
    ctx.lets.pop();

    // Push the outer list handle (base) tagged ListInt.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::ListInt);
    Ok(())
}

/// One stage in a `range(...).chain(...)` pipeline. Each stage takes
/// the running per-iteration value (initially the loop counter `i`)
/// and produces a new value of the recorded `result_ty`.
#[derive(Debug, Clone)]
struct ChainStage<'a> {
    /// Surface method name (`map` / `filter` for now). Pinned so
    /// downstream loop emitters know what control-flow shape to
    /// produce.
    method: &'static str,
    /// Closure literal supplied as the method's single positional arg.
    /// Borrowed straight off the parser AST so the loop emitter can
    /// inline it into the outer ctx's op stream.
    closure_params: &'a [ClosureParam],
    closure_body: &'a Node,
}

/// Decomposition of a `range(...)[. <method>(<closure>)]*` chain. The
/// `stages` vec walks innermost-to-outermost — `range(...).map(f).map(g)`
/// produces stages `[(map, f), (map, g)]` so the loop emitter can
/// pipeline them in source order.
#[derive(Debug)]
struct RangeChain<'a> {
    range_args: &'a [relon_parser::CallArg],
    stages: Vec<ChainStage<'a>>,
}

/// Recognise a `range(...)[ . map((p) => body) ]*` pipeline.
///
/// Parser shape (cf. `relon-parser` token AST):
///   `range(n).map(f).filter(g)` →
///     `FnCall {
///        path: [Dynamic(FnCall {
///                 path: [Dynamic(FnCall { path: [String("range")], args: [n] }),
///                        String("map")],
///                 args: [f] }),
///               String("filter")],
///        args: [g] }`
///
/// Each method call wraps its receiver as a `TokenKey::Dynamic(Node)`
/// at the head of the `path` slice, with the remaining segment being
/// the method name. This function walks that nesting innermost-out,
/// peeling each recognised method off until it hits the bare
/// `range(...)` call (terminal). Anything outside the recognised
/// shape returns `None` so the caller falls through to the regular
/// lowering path.
fn match_range_chain(expr: &Expr) -> Option<RangeChain<'_>> {
    let mut stages: Vec<ChainStage<'_>> = Vec::new();
    let mut current: &Expr = expr;
    loop {
        let Expr::FnCall { path, args } = current else {
            return None;
        };
        // Bare `range(...)` — terminal case. Validate arity + reject
        // keyword args; the parent `try_lower_list_sum_range` does the
        // same for the 0-stage form, but we repeat the check here so
        // a multi-stage chain bottoms out cleanly.
        if path.len() == 1 && matches!(&path[0], TokenKey::String(s, _, _) if s == "range") {
            if args.is_empty() || args.len() > 2 {
                return None;
            }
            if args.iter().any(|a| a.name.is_some()) {
                return None;
            }
            // Stages were pushed outermost-first; reverse so the
            // emitter walks innermost-first (source order).
            stages.reverse();
            return Some(RangeChain {
                range_args: args,
                stages,
            });
        }
        // Recognised chain step has exactly 2 path segments:
        //   [Dynamic(receiver_call), String("<method_name>")]
        // and exactly one positional closure arg.
        if path.len() != 2 {
            return None;
        }
        let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
            return None;
        };
        let TokenKey::String(method_name, _, _) = &path[1] else {
            return None;
        };
        let method: &'static str = match method_name.as_str() {
            "map" => "map",
            // Open follow-up #2: `filter` lets the W4-shape
            // (`range(n).map(c1).filter(c2).len()`) collapse onto the
            // same accumulator-loop skeleton. The filter's predicate
            // body lowers to a Bool atop the vstack; the emitter walks
            // an inner `block { ... }` so a false predicate jumps past
            // the consumer's accumulator update.
            "filter" => "filter",
            _ => return None,
        };
        if args.len() != 1 || args[0].name.is_some() {
            return None;
        }
        let Expr::Closure {
            params,
            body,
            return_type: _,
        } = &*args[0].value.expr
        else {
            return None;
        };
        if params.len() != 1 {
            return None;
        }
        stages.push(ChainStage {
            method,
            closure_params: params.as_slice(),
            closure_body: body,
        });
        // Descend into the receiver (the inner FnCall wrapped in
        // Dynamic) and continue peeling.
        current = &receiver_node.expr;
    }
}

/// Consumer terminating a `range(...)` pipeline. Selects how the
/// per-iteration value (after the final map/filter stage) folds into
/// the loop's accumulator.
#[derive(Debug, Clone, Copy)]
enum RangeConsumer<'a> {
    /// `list.sum(<chain>)` — accumulator += element (element must be
    /// `I64`).
    SumI64,
    /// `<chain>.len()` — accumulator += 1 per surviving iteration
    /// (element type is irrelevant; only the filter outcome matters).
    Len,
    /// `<chain>.reduce(<init>, (acc, elem) => body)` — init the
    /// accumulator from a lowered expression, then update it per
    /// iteration via the supplied 2-arg closure body.
    Reduce {
        /// Init expression's AST node — lowered against the outer
        /// ctx before the loop opens so its captures resolve through
        /// the normal walker.
        init: &'a Node,
        /// Closure params: [acc_name, elem_name].
        params: &'a [ClosureParam],
        /// Closure body expression.
        body: &'a Node,
    },
}

/// Emit the pure-i64 accumulator loop that implements one of the
/// recognised `range(start, end)[. map(...) | . filter(...)]*` chain
/// consumers. Pre-condition: the caller has already matched the
/// chain via [`match_range_chain`]. The emitter walks the stages
/// in source order, lowering each closure body inline against the
/// outer ctx so captures resolve through the normal walker.
///
/// Control-flow shape (the inner block lets `filter` short-circuit
/// the consumer update without breaking out of the loop):
///
/// ```text
/// block (loop-exit) {
///   loop {
///     if start >= end { br 1 }      // exit the loop-exit block
///     block (next-iter) {
///       <stage 0 emit>
///       <stage 1 emit>              // filter stages emit `br 0` on
///       ...                         // false to skip the consumer
///       <consumer update>
///     }
///     start += 1
///     br 0                          // back to loop header
///   }
/// }
/// push acc
/// ```
///
/// Label depths (counted from innermost out):
///   * inside `next-iter` block: 0 → next-iter, 1 → loop, 2 → loop-exit
///   * inside `loop` body but outside `next-iter`: 0 → loop, 1 → loop-exit
fn emit_range_pipeline_loop(
    chain: &RangeChain<'_>,
    consumer: RangeConsumer<'_>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let range_args = chain.range_args;
    // The accumulator type depends on the consumer: SumI64 / Len use
    // i64; Reduce derives it from the init expression's lowered type.
    // Stash the determined type so we can re-emit `LetGet` ops later
    // and also so the post-loop push matches.
    let start_i = ctx.next_let_idx;
    let end_i = ctx.next_let_idx + 1;
    let acc_i = ctx.next_let_idx + 2;
    ctx.next_let_idx += 3;

    // start
    if range_args.len() == 2 {
        lower_expr(&range_args[0].value.expr, range_args[0].value.range, ctx)?;
        expect_int_top(ctx, range)?;
    } else {
        ctx.out.push(TaggedOp {
            op: Op::ConstI64(0),
            range,
        });
        ctx.tstack.push(IrType::I64);
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();
    // end
    let end_arg = &range_args[range_args.len() - 1];
    lower_expr(&end_arg.value.expr, end_arg.value.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: end_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();
    // acc = <init>. SumI64 / Len init to i64 zero; Reduce lowers the
    // user-supplied init expression and inherits its type.
    let acc_ty = match consumer {
        RangeConsumer::SumI64 | RangeConsumer::Len => {
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(0),
                range,
            });
            ctx.tstack.push(IrType::I64);
            IrType::I64
        }
        RangeConsumer::Reduce { init, .. } => {
            lower_expr(&init.expr, init.range, ctx)?;
            ctx.tstack.last().copied().ok_or_else(|| {
                cap!(
                    "emit_range_pipeline_loop.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: "range-chain reduce: init expression produced no value".to_string(),
                        range: init.range,
                    }
                )
            })?
        }
    };
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.pop();

    // -----------------------------------------------------------------
    // Build the inner loop body.
    //
    // Strategy: temporarily redirect `ctx.out` into a sub-buffer per
    // nested control-flow region so we can splice the resulting vec
    // under the matching Block/Loop op. The op-stream redirect
    // preserves all other ctx fields (let-table, vstack,
    // next_let_idx, intern handle); only `out` is swapped so the
    // closure body's lowering goes into the right region.
    //
    // The sub-buffer dance is needed because the body lowering walks
    // arbitrary user IR — we don't want to drop a hand-rolled
    // Vec<TaggedOp> in the middle of `ctx.out` only to re-shuffle it.
    // -----------------------------------------------------------------

    // Outer loop body sub-buffer.
    let saved_outer = std::mem::take(&mut ctx.out);

    // br_if (start_i >= end_i) -> label_depth 1 (outer loop-exit block)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: end_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Ge(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::BrIf { label_depth: 1 },
        range,
    });

    // ----- "next-iter" block (filter short-circuit target) ----------
    let saved_iter = std::mem::take(&mut ctx.out);

    // Walk the stages. `current_value` flows from the loop counter
    // into each successive map's output; filter stages don't change
    // it (they only branch out on false).
    let mut current_value_idx = start_i;
    let mut current_value_ty = IrType::I64;
    for stage in chain.stages.iter() {
        let param = &stage.closure_params[0];
        // Allocate a fresh let-binding under the closure
        // parameter's name so `Variable(p)` inside the body
        // resolves to current_value via the normal walker.
        let param_let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: current_value_idx,
                ty: current_value_ty,
            },
            range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: param_let_idx,
                ty: current_value_ty,
            },
            range,
        });
        ctx.lets.push(LetBinding {
            name: param.name.clone(),
            idx: param_let_idx,
            ty: current_value_ty,
            schema_brand: None,
        });
        let body_node = stage.closure_body;
        lower_expr(&body_node.expr, body_node.range, ctx)?;
        let produced_ty = ctx.tstack.last().copied().ok_or_else(|| {
            cap!(
                "emit_range_pipeline_loop.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "range-chain {}: closure body produced no value",
                        stage.method
                    ),
                    range: body_node.range,
                }
            )
        })?;
        ctx.lets.pop();
        match stage.method {
            "map" => {
                // Stash the body result into a fresh let so further
                // stages (and the consumer) can pick it up.
                let result_let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: result_let_idx,
                        ty: produced_ty,
                    },
                    range: body_node.range,
                });
                ctx.tstack.pop();
                current_value_idx = result_let_idx;
                current_value_ty = produced_ty;
            }
            "filter" => {
                if produced_ty != IrType::Bool {
                    // Restore ctx.out to keep the caller's diagnostic
                    // surface from seeing a corrupt op stream.
                    let _ = std::mem::take(&mut ctx.out);
                    ctx.out = saved_iter;
                    let _ = std::mem::take(&mut ctx.out);
                    ctx.out = saved_outer;
                    return Err(cap!(
                        "emit_range_pipeline_loop.unsupported_expr.3",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "range-chain filter predicate must return Bool, got {:?}",
                                produced_ty
                            ),
                            range,
                        }
                    ));
                }
                // The predicate left a Bool on top. Branch to the
                // "next-iter" block on FALSE so the consumer update
                // is skipped. Wasm's `br_if` branches on non-zero —
                // so we invert first via Op::Sub or Op::Eq with 0.
                // Simpler: push 0 and `Eq` to get "predicate==0", then
                // br_if.
                ctx.out.push(TaggedOp {
                    op: Op::ConstBool(false),
                    range,
                });
                ctx.tstack.push(IrType::Bool);
                ctx.out.push(TaggedOp {
                    op: Op::Eq(IrType::Bool),
                    range,
                });
                ctx.tstack.pop();
                ctx.tstack.pop();
                ctx.tstack.push(IrType::Bool);
                ctx.out.push(TaggedOp {
                    op: Op::BrIf { label_depth: 0 },
                    range,
                });
                ctx.tstack.pop();
                // current_value passes through unchanged.
            }
            other => {
                return Err(cap!(
                    "emit_range_pipeline_loop.unsupported_expr.4",
                    LoweringError::UnsupportedExpr {
                        kind: format!("range-chain: unsupported method `{}`", other),
                        range,
                    }
                ));
            }
        }
    }

    // Consumer update.
    match consumer {
        RangeConsumer::SumI64 => {
            // Push the current element and require it i64.
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: current_value_idx,
                    ty: current_value_ty,
                },
                range,
            });
            ctx.tstack.push(current_value_ty);
            if current_value_ty != IrType::I64 {
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_iter;
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_outer;
                return Err(cap!(
                    "emit_range_pipeline_loop.unsupported_expr.5",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "list.sum(range(...).map(...)) requires Int-valued element; got {:?}",
                            current_value_ty
                        ),
                        range,
                    }
                ));
            }
            // acc_i += element
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::Add(IrType::I64),
                range,
            });
            ctx.tstack.pop();
            ctx.tstack.pop();
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.pop();
        }
        RangeConsumer::Len => {
            // acc_i += 1
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(1),
                range,
            });
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::Add(IrType::I64),
                range,
            });
            ctx.tstack.pop();
            ctx.tstack.pop();
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.pop();
            // Silence the unused-let warning when no map stage flows.
            let _ = current_value_idx;
        }
        RangeConsumer::Reduce { params, body, .. } => {
            // The reduce closure takes (acc, elem). Bind both as
            // transient let-bindings under the closure's parameter
            // names so the body's `Variable(...)` lookups resolve
            // through the normal walker.
            if params.len() != 2 {
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_iter;
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_outer;
                return Err(cap!(
                    "emit_range_pipeline_loop.unsupported_expr.6",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "range-chain reduce requires 2-arg closure (acc, elem); got {}",
                            params.len()
                        ),
                        range,
                    }
                ));
            }
            // Bind acc into a fresh let under params[0].name. Source
            // value: current acc_i contents.
            let acc_param_let = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: acc_i,
                    ty: acc_ty,
                },
                range,
            });
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_param_let,
                    ty: acc_ty,
                },
                range,
            });
            ctx.lets.push(LetBinding {
                name: params[0].name.clone(),
                idx: acc_param_let,
                ty: acc_ty,
                schema_brand: None,
            });
            // Bind elem into a fresh let under params[1].name. Source
            // value: current_value_idx.
            let elem_param_let = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: current_value_idx,
                    ty: current_value_ty,
                },
                range,
            });
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: elem_param_let,
                    ty: current_value_ty,
                },
                range,
            });
            ctx.lets.push(LetBinding {
                name: params[1].name.clone(),
                idx: elem_param_let,
                ty: current_value_ty,
                schema_brand: None,
            });
            // Lower the closure body — leaves the new acc value on
            // top of the vstack.
            lower_expr(&body.expr, body.range, ctx)?;
            let produced = ctx.tstack.last().copied().ok_or_else(|| {
                cap!(
                    "emit_range_pipeline_loop.unsupported_expr.7",
                    LoweringError::UnsupportedExpr {
                        kind: "range-chain reduce: body produced no value".to_string(),
                        range: body.range,
                    }
                )
            })?;
            if produced.wasm_slot() != acc_ty.wasm_slot() {
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_iter;
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_outer;
                return Err(cap!(
                    "emit_range_pipeline_loop.unsupported_expr.8",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "range-chain reduce: body returned {:?}, expected init type {:?}",
                            produced, acc_ty
                        ),
                        range: body.range,
                    }
                ));
            }
            ctx.lets.pop(); // elem
            ctx.lets.pop(); // acc
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_i,
                    ty: acc_ty,
                },
                range,
            });
            ctx.tstack.pop();
        }
    }

    // Pop the "next-iter" sub-buffer and splice it under Op::Block.
    let iter_body = std::mem::replace(&mut ctx.out, saved_iter);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: iter_body,
        },
        range,
    });

    // start_i += 1; br 0 (back to loop)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Br { label_depth: 0 },
        range,
    });

    // Pop the outer loop body and wrap under Block { Loop { ... } }.
    let outer_body = std::mem::replace(&mut ctx.out, saved_outer);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: outer_body,
                },
                range,
            }],
        },
        range,
    });
    // Push the accumulator so the consumer sees its final value on
    // top — matches the corresponding `list_int_sum` / `list_int_length`
    // / `list_int_fold` return shape depending on the consumer.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.push(acc_ty);
    Ok(())
}

#[cfg(test)]
mod recognizer_parity {
    //! Parity guard: the shared AST recogniser
    //! `relon_parser::rewrite::recognize_fused` (consumed by the tree-walk
    //! interpreter's materialisation-free fast-path) must agree with this
    //! module's IR-side `match_range_chain` on the *bare* `list.sum(range(...))`
    //! subset (no map/filter stages). Keeps the two recognisers from drifting.

    use super::*;
    use relon_parser::parse_document;
    use relon_parser::rewrite::{recognize_fused, FusedPattern};

    fn list_sum_call(src: &str) -> Node {
        let doc = parse_document(src).expect("parse");
        fn find(node: &Node) -> Option<Node> {
            if let Expr::FnCall { path, .. } = node.expr.as_ref() {
                let is_list_sum = path.len() == 2
                    && matches!(&path[0], TokenKey::String(s, _, _) if s == "list")
                    && matches!(&path[1], TokenKey::String(s, _, _) if s == "sum");
                if is_list_sum {
                    return Some(node.clone());
                }
            }
            for child in relon_parser::child_nodes(node) {
                if let Some(found) = find(child) {
                    return Some(found);
                }
            }
            None
        }
        find(&doc).expect("list.sum call")
    }

    /// On the bare-range subset both recognisers fire and agree on the
    /// presence/absence of an explicit `start` argument.
    #[test]
    fn bare_range_subset_agrees() {
        for (src, has_start) in [
            ("#main(Int n) -> Int\nlist.sum(range(n))", false),
            ("#main(Int n) -> Int\nlist.sum(range(5, n))", true),
        ] {
            let call = list_sum_call(src);
            let Expr::FnCall { args, .. } = call.expr.as_ref() else {
                unreachable!()
            };
            // IR side.
            let chain = match_range_chain(&args[0].value.expr).expect("ir match");
            assert!(chain.stages.is_empty(), "bare range has no stages: {src}");
            let ir_has_start = chain.range_args.len() == 2;
            // Shared AST recogniser.
            let pat = recognize_fused(call.expr.as_ref()).expect("ast match");
            let FusedPattern::RangeSum { start, .. } = pat;
            assert_eq!(start.is_some(), has_start, "ast start mismatch: {src}");
            assert_eq!(
                ir_has_start,
                start.is_some(),
                "ir/ast disagree on start arg: {src}"
            );
        }
    }

    /// The map/filter chain forms are owned by the IR peephole only; the
    /// shared AST recogniser deliberately does NOT fire on them (so the
    /// interpreter falls through to the stdlib path).
    #[test]
    fn chain_form_is_ir_only() {
        let call = list_sum_call("#main(Int n) -> Int\nlist.sum(range(n).map((i) => i))");
        let chain = match_range_chain(match call.expr.as_ref() {
            Expr::FnCall { args, .. } => &args[0].value.expr,
            _ => unreachable!(),
        })
        .expect("ir match");
        assert_eq!(chain.stages.len(), 1, "one map stage");
        assert!(
            recognize_fused(call.expr.as_ref()).is_none(),
            "shared recogniser must not claim chain forms"
        );
    }
}
