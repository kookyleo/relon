//! Lowering sub-module: function-call and comprehension lowering.
//!
//! Owns the call dispatch ladder — local closure-let calls
//! (`CallClosure`), `#native` host-fn calls, and the big
//! `lower_fn_call` stdlib / method / peephole dispatcher — plus list
//! comprehension lowering in both the plain and typed-slot
//! (`_as_type`) forms and the shared `emit_map_with_ret` loop body.

use super::*;

/// Phase F.2 (W7 anon-Dict-return): when a free-call's head names a
/// closure-typed let-binding (a `(name) => ...` value lifted into an
/// internal let by [`lower_anon_dict_body`]), emit the call as
/// `LetGet { idx, Closure }` + per-arg lowering + `Op::CallClosure`.
///
/// Returns `Ok(Some(()))` when the call was lowered, `Ok(None)` when
/// the head doesn't match a closure let (so the caller falls back to
/// the stdlib dispatch / schema-method path). Errors propagate when
/// the arg arity / types don't match the recorded signature.
pub(super) fn try_lower_local_closure_call(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    let [TokenKey::String(name, _, _)] = path else {
        return Ok(None);
    };
    let Some(binding) = ctx.lets.iter().rev().find(|b| b.name == *name).cloned() else {
        return Ok(None);
    };
    if binding.ty != IrType::Closure {
        return Ok(None);
    }
    let (param_tys, ret_ty) = ctx
        .closure_let_signatures
        .get(&binding.idx)
        .cloned()
        .ok_or_else(|| {
            cap!(
                "try_lower_local_closure_call.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "FnCall(local-closure `{}`: signature side-table missing for let_idx {})",
                        name, binding.idx,
                    ),
                    range,
                }
            )
        })?;
    if args.len() != param_tys.len() {
        return Err(cap!(
            "try_lower_local_closure_call.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: name.clone(),
                arity: args.len() as u32,
                range,
            }
        ));
    }
    // Push the closure handle first; `Op::CallClosure` consumes
    // `[handle, arg0, arg1, ...]`.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: binding.idx,
            ty: IrType::Closure,
        },
        range,
    });
    ctx.tstack.push(IrType::Closure);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(cap!(
                "try_lower_local_closure_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(local-closure `{}`: named-arg unsupported)", name),
                    range,
                }
            ));
        }
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let expected = param_tys[i];
        let got = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "try_lower_local_closure_call.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "FnCall(local-closure `{}`: arg {} produced no value)",
                        name, i
                    ),
                    range,
                }
            )
        })?;
        if got.wasm_slot() != expected.wasm_slot() {
            // Wave B: a param typed `String` by the concat-body
            // inference (never by an explicit annotation) is only ever
            // used as a concat leaf inside the body, so rendering a
            // scalar argument to `String` *before* the call is
            // byte-identical to the tree-walk oracle, which renders
            // the value at the `+` inside the body via the same
            // `Display`. This is what admits the
            // `examples/pricing.relon` Float-valued `@currency` shape
            // (`@currency("USD") display: price` ⇒
            // `currency(price, "USD")` with a String-concat body).
            let coercible = expected == IrType::String
                && matches!(got, IrType::I64 | IrType::F64 | IrType::Bool)
                && ctx
                    .closure_concat_coercible
                    .get(&binding.idx)
                    .and_then(|mask| mask.get(i).copied())
                    .unwrap_or(false);
            if coercible {
                // `lower_value_to_string` expects the operand tag
                // already popped (it is — `got` above) and pushes the
                // result `String` tag itself.
                lower_value_to_string(got, call_arg.value.range, ctx)?;
                continue;
            }
            return Err(cap!(
                "try_lower_local_closure_call.stdlib_arg_type_mismatch",
                LoweringError::StdlibArgTypeMismatch {
                    name: name.clone(),
                    arg_idx: i as u32,
                    got,
                    expected,
                    range,
                }
            ));
        }
        ctx.tstack.push(got);
    }
    // Pop args + handle from the virtual stack to match the op's
    // stack effect.
    for _ in 0..param_tys.len() {
        ctx.tstack.pop();
    }
    ctx.tstack.pop(); // the handle
    ctx.out.push(TaggedOp {
        op: Op::CallClosure {
            param_tys: param_tys.clone(),
            ret_ty,
        },
        range,
    });
    ctx.tstack.push(ret_ty);
    Ok(Some(()))
}

/// Phase 4.a: lower an [`Expr::FnCall`] into a stdlib dispatch.
///
/// Accepts two forms:
///
/// * **Free call**: `path = ["length"]`, `args = [single_value]`. The
///   value is treated as the lone argument; codegen pops it before
///   emitting the wasm `call`.
/// * **Method call**: `path = [<receiver_segments..>, "length"]`,
///   `args = []`. The leading path segments name the receiver — a
///   bare identifier (`s.length()`) carries the segments as
///   `TokenKey::String(name)`, while a parenthesised expression
///   (`("a").length()`) wraps the receiver in a `TokenKey::Dynamic`.
///   Both shapes lower the receiver onto the stack as the call's
///   single argument.
///
/// Any other name / arity surfaces as
/// [`LoweringError::UnknownStdlibMethod`]; argument-type mismatches
/// surface as [`LoweringError::StdlibArgTypeMismatch`].
/// Free-call native-fn lowering. Returns `Ok(true)` when `name`
/// resolved to a host-registered native fn and the guarded call was
/// emitted; `Ok(false)` when the name is not a native fn (so the
/// caller falls through to its stdlib-unknown error).
///
/// Emitted shape, in order:
///   1. one `Op::CheckCap { cap_bit }` per capability bit the fn's
///      gate requires (declaration order) — fires the runtime consult
///      before any argument is evaluated, so a denial traps before the
///      host fn observes state;
///   2. each argument expression, left-to-right;
///   3. `Op::CallNative { import_idx, param_tys, ret_ty, cap_bit:
///      NO_CAPABILITY_BIT }` — the cap guard lives in the CheckCap
///      prologue, so the call itself carries the no-cap sentinel and
///      both backends key the host-fn slot off `import_idx`.
///
/// The analyzer has already arity- and type-checked the call (and the
/// capability reachability check has run against the static grant), so
/// an arity mismatch here means the host registered a signature the
/// analyzer never saw — bail to `Ok(false)` and let the unknown-method
/// error surface rather than emit a malformed call.
pub(super) fn try_lower_native_call(
    name: &str,
    args: &[relon_parser::CallArg],
    arity: u32,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    let entry = match ctx.native_imports.borrow().resolved.get(name) {
        Some(e) => e.clone(),
        None => return Ok(false),
    };
    if entry.param_tys.len() as u32 != arity {
        return Ok(false);
    }
    // Native fns take positional args only; a keyword arg means the
    // call doesn't match the host signature.
    for call_arg in args {
        if call_arg.name.is_some() {
            return Err(cap!(
                "try_lower_native_call.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{name}` for native fn)"),
                    range,
                }
            ));
        }
    }

    // 1. Capability prologue: one CheckCap per required bit, before
    //    any argument side effect.
    for bit in &entry.required_bits {
        ctx.out.push(TaggedOp {
            op: Op::CheckCap { cap_bit: *bit },
            range,
        });
    }

    // 2. Arguments, left-to-right. Each must land on the operand stack
    //    with the IR type the import declares.
    for (i, call_arg) in args.iter().enumerate() {
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let pushed = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "try_lower_native_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(native arg{i}-stack-empty for `{name}`)"),
                    range,
                }
            )
        })?;
        let expected = entry.param_tys[i];
        if pushed != expected {
            return Err(cap!("try_lower_native_call.unsupported_expr.3", LoweringError::UnsupportedExpr {
                kind: format!(
                    "FnCall(`{name}`) native arg {i} type mismatch: expected {expected:?}, got {pushed:?}"
                ),
                range,
            }));
        }
    }

    // 3. Intern the import + emit the call. Sentinel cap_bit: the
    //    guard already fired via the CheckCap prologue.
    let import_idx = ctx.native_imports.borrow_mut().intern(name, &entry);
    ctx.out.push(TaggedOp {
        op: Op::CallNative {
            import_idx,
            param_tys: entry.param_tys.clone(),
            ret_ty: entry.ret_ty,
            cap_bit: NO_CAPABILITY_BIT,
        },
        range,
    });
    ctx.tstack.push(entry.ret_ty);
    Ok(true)
}

pub(super) fn lower_fn_call(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path.is_empty() {
        return Err(cap!(
            "lower_fn_call.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "FnCall(empty-path)".to_string(),
                range,
            }
        ));
    }
    // Phase F.2 (W7 anon-Dict-return): a free-call whose head is a
    // single-segment identifier may resolve to a closure-typed
    // let-binding (the anon-Dict-return path stashes each closure
    // field there). When it does, emit `LetGet { idx, Closure }` +
    // typed arg loads + `Op::CallClosure`. Falls through to the
    // stdlib dispatch when the head doesn't match a let.
    if let Some(()) = try_lower_local_closure_call(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W18 slice): materialize a runtime `range(a, b)` into a
    // `List<Int>` scratch record, run it through the bundled
    // `list_int_filter` body, then read the survivor count via
    // `ReadStringLen`. The canonical caller is the W18 prime-count
    // kernel `_len(_list_filter(range(2, n), (k) => is_prime(k, 2)))`.
    //
    // Unlike the eliding `range(...).len()` / `list.sum(range(...))`
    // peepholes below, the filtered list MUST be materialised: the
    // predicate decides survivor membership per element, and the
    // count cannot be expressed as a closed-form fold over the loop
    // counter (the survivors are a data-dependent subset). The
    // materialise + filter + length path mirrors the bundled stdlib
    // bodies (`list_int_filter` / `list_int_length`) so the record
    // layout flows through unchanged.
    if let Some(()) = try_lower_len_filter_range(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W16 slice): general `_len(xs)` / `_list_filter(xs, f)` on
    // an arbitrary `List<Int>`-typed argument (a where-bound
    // materialised range, a `#main` `List<Int>` param, or a nested
    // `_list_filter(...)` result). These run AFTER the fused
    // `_len(_list_filter(range(...)))` peephole above so that specific
    // shape keeps its single-pass lowering; here the argument is
    // lowered first and the handler only commits when it actually
    // produced a `List<Int>` handle (otherwise the speculative
    // lowering is discarded and dispatch falls through).
    if let Some(()) = try_lower_list_len(path, args, range, ctx)? {
        return Ok(());
    }
    // Stdlib tail wave: `count(xs)` — the oracle's plain element count
    // (`xs.len() as i64`) over ANY list shape; same `ReadStringLen`
    // lowering as `_len` but accepting every list IR type (all records
    // share the `[len: u32 LE]` count prefix). Non-list arguments roll
    // back and cap loudly through the generic dispatch.
    if let Some(()) = try_lower_list_count(path, args, range, ctx)? {
        return Ok(());
    }
    if let Some(()) = try_lower_list_filter(path, args, range, ctx)? {
        return Ok(());
    }
    // Stdlib tail wave: short-circuiting quantifiers `every(xs, p)` /
    // `some(xs, p)` and the `uniqueItems` scan `unique(xs)` over
    // `List<Int>` / `List<Float>` sources. Unsupported sources roll
    // back and cap loudly through the generic dispatch (no bundled
    // body is registered under the surface names).
    if let Some(()) = try_lower_list_pred(path, args, range, ctx)? {
        return Ok(());
    }
    if let Some(()) = try_lower_list_unique(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R3: general `_list_map(xs, f)` — materialise / resolve the
    // list source, then dispatch the transform closure per element
    // through the bundled `list_int_map` body (`Op::CallClosure` over the
    // proven four-way closure substrate). Sits beside `_list_filter`
    // here; the method forms `xs.map(f)` route through `stdlib_method_index`
    // at the bottom of the dispatch (both land on `list_int_map`).
    if let Some(()) = try_lower_list_map(path, args, range, ctx)? {
        return Ok(());
    }
    // review-improvement-160 bytecode M3 phase 2: peephole-desugar
    // `list.sum(range(end))` / `list.sum(range(start, end))` into an
    // explicit `Op::Loop` accumulator before normal lowering kicks in.
    //
    // Why not go through the stdlib path?  `list` is a Relon-source
    // module alias (`#import list from "std/list"`) — the tree-walker
    // resolves it dynamically through the host module loader, but IR
    // lowering has no notion of "user-imported namespace", so the
    // receiver `list` always falls through as `UnresolvedVariable`.
    // Similarly `range` is a tree-walker-only host fn with no IR stdlib
    // entry.  Together this makes the canonical hot-loop shape
    // `list.sum(range(n))` (cmp_lua W1) unreachable to the bytecode +
    // cranelift backends today.
    //
    // The desugar emits the same arithmetic the hand-written
    // `list_int_sum` body computes (i64 accumulator over `start..end`)
    // without materialising the intermediate List<Int> — bytecode does
    // not have list arenas big enough for a 10k element transient
    // anyway, and even the cranelift backend benefits from skipping the
    // record allocation.  Behaviour matches the tree-walker exactly:
    // `range(start, end)` produces the integers `[start, end)` (empty
    // when `start >= end`) and `list.sum([])` returns `0`.
    if let Some(()) = try_lower_list_sum_range(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W16 slice): general `list.sum(<List<Int> handle>)` — the
    // equal-partition fold in the W16 quicksort kernel
    // (`list.sum(_list_filter(xs, (x) => x == xs[0]))`). Runs after the
    // `list.sum(range(...))` eliding peephole so a raw range still folds
    // without materialising; here the argument is a materialised handle.
    if let Some(()) = try_lower_list_sum_value(path, args, range, ctx)? {
        return Ok(());
    }
    // Open follow-up #2: `range(start, end)[. map(...) | . filter(...)]*.len()`
    // desugars to a pure i64 count accumulator. Without this the
    // chain would lower into the buffer-protocol list pipeline and
    // miss the scalar accumulator shape the native backends need, so
    // cmp_lua W4 stays at `n/a`. The desugar fires
    // before regular method dispatch because the receiver path
    // (`range(...)` etc.) is not statically reachable as a known
    // schema in the analyzer's IR-side table — without the peephole
    // `lower_method_receiver` would emit an `UnresolvedVariable`.
    if let Some(()) = try_lower_range_chain_len(path, args, range, ctx)? {
        return Ok(());
    }
    // Open follow-up #2: `<chain>.reduce(<init>, (acc, elem) => body)`.
    // The reduce consumer is structurally similar to sum + len but
    // takes a user-supplied init expression and a 2-arg closure that
    // updates the accumulator per iteration. Covers cmp_lua W3
    // (`range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`)
    // by emitting the same scalar pipeline loop with a string
    // accumulator stashed in a let-binding.
    // AOT-2: doubly-nested `range.map(range.map(...))` reduced
    // cell-by-cell. The canonical caller is the W19 matmul kernel
    // shape — `range(size).map((i) => range(size).map((j) => <cell>))
    // .reduce(0, (row_acc, row) => row_acc + <fold-of-row>)`. The
    // outer map's body is itself a *bare* inner `range(...).map(...)`
    // producing a `List<Int>` row; the outer reduce folds each row
    // cell-by-cell. This MUST run before `try_lower_range_chain_reduce`
    // — that single-level peephole would otherwise match the outer
    // `range(...).map(...).reduce(...)` and then choke lowering the
    // list-valued map body (the inner bare `range(...)` surfaces as an
    // `UnknownStdlibMethod`; `range` is a tree-walker host fn, not an
    // IR stdlib entry). This peephole emits a doubly-nested i64
    // accumulator loop with NO intermediate list materialised.
    if let Some(()) = try_lower_nested_range_map_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    if let Some(()) = try_lower_range_chain_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W19 slice): `<materialised-list>.reduce(init, (acc, elem)
    // => body)` over a where-bound `List<Int>` / `List<List<Int>>`
    // handle — the W19 `#main` outer fold `c.reduce(...)` (with the
    // nested `row.reduce(...)` re-reducing each row handle). Runs after
    // the range-chain reduce so a `range(...)` receiver still elides; it
    // only fires when the receiver is a bare list-typed let.
    if let Some(()) = try_lower_materialized_list_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R3: general `_list_reduce(xs, init, f)` — fold an arbitrary
    // `List<Int>` source to an `Int` accumulator via the bundled
    // `list_int_fold` body (matches the tree-walk `ListReduce` order +
    // checked-add overflow). Runs after the `.reduce` method peepholes so
    // the eliding range-chain / nested-matmul folds keep their scalar
    // loops; this only fires for the underscore free-call form.
    if let Some(()) = try_lower_list_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R3: general `range(a, b)` as a materialised `List<Int>` value
    // (not folded inside an eliding consumer). Runs LAST among the
    // free-call peepholes so the `list.sum(range(...))` /
    // `_len(_list_filter(range(...)))` / `range(...).len()` elisions above
    // keep their allocation-free loops; only a bare `range(...)` whose
    // result is actually used as a list (returned, indexed, `.map`/
    // `.filter`/`reduce`'d) reaches here and gets a real record.
    if let Some(()) = try_lower_range_value(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R4: static const-fold of `type(v)`. In strict mode the
    // argument's type is statically known, so this lowers the argument
    // (for trap parity), discards the value, and pushes the constant
    // canonical type-name String via the shared `IrType::type_name`
    // mapping. Falls through to the cap path when the argument type is
    // not statically nameable (Wave R6 dynamic / boxed `type()`).
    if let Some(()) = try_lower_type_const(path, args, range, ctx)? {
        return Ok(());
    }
    // Final path segment is the method / function name. Earlier
    // segments either form the receiver (method-call form) or are
    // unused (free-call form has exactly one path segment).
    let method_name = match path.last().unwrap() {
        TokenKey::String(name, _, _) => name.as_str(),
        _ => {
            return Err(cap!(
                "lower_fn_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "FnCall(non-string-tail-segment)".to_string(),
                    range,
                }
            ));
        }
    };
    let receiver_segments = &path[..path.len() - 1];
    // Wave R7: scalar-returning Float math (`abs`/`floor`/`ceil`/`round`/
    // `sqrt`) — operand-type-aware dispatch over the new float-intrinsic
    // bundled bodies. Runs before the generic name-keyed dispatch (which
    // caps these for a Float operand because the bundled `abs` is
    // `Int->Int` and the others had no body pre-R7). Falls through on a
    // non-numeric operand / unmatched name, which then caps loudly.
    if let Some(()) =
        peephole::try_lower_scalar_math(method_name, receiver_segments, args, range, ctx)?
    {
        return Ok(());
    }
    // JSON-Schema numeric predicates `multiple_of` / `in_range` (polymorphic
    // numeric domain — same speculative-then-dispatch discipline as the
    // scalar-math peephole). Float `multiple_of` rolls back here and caps
    // loudly through the generic dispatch.
    if let Some(()) =
        peephole::try_lower_predicate_math(method_name, receiver_segments, args, range, ctx)?
    {
        return Ok(());
    }
    // JSON-Schema `size_in_range` over a List / Dict receiver (the element /
    // entry count comes from the shared `[len: u32 LE]` header). A String
    // receiver rolls back and caps loudly (Unicode code-point count needs the
    // UTF-8 decode seam LLVM-native / wasm do not lower).
    if let Some(()) =
        peephole::try_lower_size_in_range(method_name, receiver_segments, args, range, ctx)?
    {
        return Ok(());
    }
    let arity = if receiver_segments.is_empty() {
        // Free-call form. Each `CallArg` value contributes one
        // argument; named args are rejected here (Phase 4.a/4.b
        // stdlib doesn't take keyword args).
        u32::try_from(args.len()).unwrap_or(u32::MAX)
    } else {
        // Method-call form: the receiver becomes argument 0, and any
        // additional `args` slots become the remaining arguments.
        // Phase 4.b method dispatch is limited to single-arg surface
        // methods (`length` / `is_empty`); later phases (concat /
        // substring / ...) will exercise the `1 + args.len()` path.
        1u32 + u32::try_from(args.len()).unwrap_or(u32::MAX)
    };

    // Method-call form (`s.length()`) and free-call form (`length(s)`)
    // take different dispatch paths:
    //
    //   * Method-call: lower the receiver first so we know its IR type,
    //     then resolve `(receiver_ty, method_name)` via
    //     `stdlib_method_index`. This lets the same surface name
    //     (`length`) dispatch to different bodies based on receiver
    //     (`String` -> `length`, `List<Int>` -> `list_int_length`).
    //   * Free-call: resolve directly through `stdlib_function_index`
    //     on the bare name. Free-call form deliberately doesn't go
    //     through `stdlib_method_index` so e.g. `length(xs)` for a
    //     `List<Int>` xs surfaces an `StdlibArgTypeMismatch` rather
    //     than silently routing to `list_int_length` — callers who
    //     want list length must use `xs.length()` or call
    //     `list_int_length(xs)` explicitly.
    if receiver_segments.is_empty() {
        // Free-call form.
        let Some(fn_index) = stdlib_function_index(method_name) else {
            // Not a stdlib builtin — try a host-registered native fn.
            // Stdlib wins on a name clash (builtins stay first-class);
            // a name the host registered that shadows no builtin
            // resolves here into an `Op::CheckCap`-guarded
            // `Op::CallNative`.
            if try_lower_native_call(method_name, args, arity, range, ctx)? {
                return Ok(());
            }
            return Err(cap!(
                "lower_fn_call.unknown_stdlib_method.1",
                LoweringError::UnknownStdlibMethod {
                    name: method_name.to_string(),
                    arity,
                    range,
                }
            ));
        };
        let stdlib_meta = builtin_stdlib().get(fn_index as usize).ok_or_else(|| {
            cap!(
                "lower_fn_call.unknown_stdlib_method.2",
                LoweringError::UnknownStdlibMethod {
                    name: method_name.to_string(),
                    arity,
                    range,
                }
            )
        })?;
        if (stdlib_meta.params.len() as u32) != arity {
            return Err(cap!(
                "lower_fn_call.unknown_stdlib_method.3",
                LoweringError::UnknownStdlibMethod {
                    name: method_name.to_string(),
                    arity,
                    range,
                }
            ));
        }
        for (i, call_arg) in args.iter().enumerate() {
            if call_arg.name.is_some() {
                return Err(cap!(
                    "lower_fn_call.unsupported_expr.3",
                    LoweringError::UnsupportedExpr {
                        kind: format!("FnCall(named-arg `{}` for stdlib)", method_name),
                        range,
                    }
                ));
            }
            lower_stdlib_arg(
                stdlib_meta.name,
                i as u32,
                &call_arg.value,
                &stdlib_meta.params,
                ctx,
                range,
            )?;
        }
        for _ in 0..arity {
            ctx.tstack.pop();
        }
        ctx.out.push(TaggedOp {
            op: Op::Call {
                fn_index,
                arg_count: arity,
                param_tys: stdlib_meta.params.clone(),
                ret_ty: stdlib_meta.ret,
            },
            range,
        });
        ctx.tstack.push(stdlib_meta.ret);
        return Ok(());
    }

    // Method-call form. Lower the receiver first to surface its IR
    // type and schema brand, then route to either schema-rooted
    // dispatch (when the receiver carries a brand) or the stdlib
    // method table.
    let receiver_brand = lower_method_receiver(receiver_segments, range, ctx)?;
    let receiver_ty = *ctx.tstack.last().ok_or(cap!(
        "lower_fn_call.unsupported_expr.4",
        LoweringError::UnsupportedExpr {
            kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
            range,
        }
    ))?;
    // Schema-rooted dispatch beats the stdlib table when both could
    // resolve the call — Phase 5 keeps user methods first-class. The
    // stdlib path stays available for receivers without a brand
    // (`String::length`, `List<Int>::length`).
    if let Some(schema_name) = receiver_brand.as_deref() {
        let key = (schema_name.to_string(), method_name.to_string());
        if let Some((fn_index, param_tys, ret_ty)) = ctx
            .method_registry
            .methods
            .get(&key)
            .map(|(idx, p, r)| (*idx, p.clone(), *r))
        {
            return finish_schema_method_call(
                fn_index,
                param_tys,
                ret_ty,
                args,
                method_name,
                range,
                ctx,
            );
        }
    }
    // Wave R3b/R3c: route method-form `xs.map(f)` / `xs.filter(f)` over a
    // list receiver through the closure-return-type candidate selector
    // (the receiver is already lowered onto the vstack). This resolves the
    // element-type-changing numeric maps and the R3c String-result maps
    // that the fixed `stdlib_method_index` (`(elem -> elem)`) signature
    // cannot. `Ok(None)` falls through to the regular dispatch below (e.g.
    // a non-list receiver, or no candidate return matched), which caps it
    // loudly.
    if matches!(
        receiver_ty,
        IrType::ListInt | IrType::ListFloat | IrType::ListString
    ) && (method_name == "map" || method_name == "filter")
        && args.len() == 1
        && args[0].name.is_none()
    {
        let kind = if method_name == "map" {
            peephole::ListHofKind::Map
        } else {
            peephole::ListHofKind::Filter
        };
        if let Some(()) =
            peephole::emit_list_hof_method(kind, receiver_ty, &args[0].value, range, ctx)?
        {
            return Ok(());
        }
    }
    let Some(fn_index) = stdlib_method_index(receiver_ty, method_name) else {
        return Err(cap!(
            "lower_fn_call.unknown_stdlib_method.4",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        ));
    };
    let stdlib_meta = builtin_stdlib().get(fn_index as usize).ok_or_else(|| {
        cap!(
            "lower_fn_call.unknown_stdlib_method.5",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        )
    })?;
    if (stdlib_meta.params.len() as u32) != arity {
        return Err(cap!(
            "lower_fn_call.unknown_stdlib_method.6",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        ));
    }
    // Wave R15: `s.split(sep)` is only sound to lower when the separator
    // is a provably non-empty String literal. The tree-walk oracle
    // *errors* on an empty separator (`UnsupportedOperator`, never a
    // value), and the byte-level `split` body cannot portably reproduce
    // that error (an empty separator would match at every byte position,
    // diverging from the oracle). Rather than emit a non-portable
    // `Op::Trap`, cap every case we cannot prove non-empty at lowering: a
    // non-literal separator (parameter / load / call) and the empty
    // literal both stay capped. The dominant `","` / `"--"` literal form
    // lowers four-way.
    if method_name == "split"
        && receiver_ty == IrType::String
        && !matches!(
            args.first().map(|a| &*a.value.expr),
            Some(Expr::String(s)) if !s.is_empty()
        )
    {
        return Err(cap!(
            "lower_fn_call.split_empty_separator",
            LoweringError::UnsupportedExpr {
                kind: "String.split with a separator that is not a non-empty string literal — the \
                     tree-walk oracle errors on an empty separator, so only a provably non-empty \
                     literal separator lowers byte-equal four-way"
                    .to_string(),
                range,
            }
        ));
    }
    // The receiver already sits on the vstack — re-check that its
    // slot matches the callee's declared param[0] so a future
    // dispatch entry that mistypes its receiver surfaces at lowering
    // rather than at codegen.
    let pushed_receiver = ctx.tstack.pop().ok_or(cap!(
        "lower_fn_call.unsupported_expr.5",
        LoweringError::UnsupportedExpr {
            kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
            range,
        }
    ))?;
    check_stdlib_arg(method_name, 0, pushed_receiver, &stdlib_meta.params, range)?;
    ctx.tstack.push(pushed_receiver);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(cap!(
                "lower_fn_call.unsupported_expr.6",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{}` for stdlib)", method_name),
                    range,
                }
            ));
        }
        lower_stdlib_arg(
            stdlib_meta.name,
            (i + 1) as u32,
            &call_arg.value,
            &stdlib_meta.params,
            ctx,
            range,
        )?;
    }
    for _ in 0..arity {
        ctx.tstack.pop();
    }
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index,
            arg_count: arity,
            param_tys: stdlib_meta.params.clone(),
            ret_ty: stdlib_meta.ret,
        },
        range,
    });
    ctx.tstack.push(stdlib_meta.ret);
    Ok(())
}

/// Wave R3: lower a list comprehension `[ element for id in iterable
/// (if condition)? ]` by desugaring onto the bundled `list_*_filter`
/// then `list_*_map` higher-order bodies — the same machinery the
/// `xs.filter(...)` / `xs.map(...)` method forms use.
///
/// Semantics (matched byte-exactly to the tree-walk `Expr::Comprehension`
/// driver in `relon-evaluator::eval`): iterate `iterable`'s elements in
/// order; when a `condition` is present, keep only the elements for which
/// `condition` (evaluated with `id` bound to the element) is truthy; emit
/// `element` (evaluated with `id` bound to the element) for each surviving
/// element. Filter-then-map composes to exactly this: the filter body
/// retains the passing source elements (unchanged), then the map body
/// computes `element` from each survivor.
///
/// The loop variable `id` becomes the synthesised closure parameter, so
/// any outer reference inside `condition` / `element` (a `#main` param, a
/// where-bound value) resolves through the closure's free-variable
/// capture path exactly as a hand-written `iterable.filter((id) =>
/// condition).map((id) => element)` would.
///
/// Source-element coverage mirrors the method-form HOF emitter
/// ([`peephole::emit_list_hof_call`]): `List<Int>` and `List<Float>`
/// sources ride the 8-byte-slot numeric bodies; `List<String>` rides the
/// 4-byte pointer-array bodies. The map body is selected from the
/// closure's inferred return type so an element-type-changing map (e.g.
/// `[float(x) for x in ints]` -> `list_int_map_to_float`) lowers four-way
/// when a bundled cross-type body exists. A `List<String>` filter caps
/// (no four-way `String -> Bool` predicate body, matching the method
/// form), as does any source whose element type lacks bundled HOF bodies
/// (e.g. `List<Bool>`).
pub(super) fn lower_comprehension(
    element: &Node,
    id: &str,
    iterable: &Node,
    condition: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // 1. Lower the iterable to a list handle. `range(n)`, a where-bound
    //    list, a `#main` list param, and nested comprehensions / map /
    //    filter results all land here. The element type drives body
    //    selection: `ListInt`/`ListFloat` ride the 8-byte numeric bodies,
    //    `ListString` rides the 4-byte pointer-array bodies.
    lower_expr(&iterable.expr, iterable.range, ctx)?;
    let src_ty = ctx.tstack.last().copied();
    let src_elem = match src_ty {
        Some(IrType::ListInt) => IrType::I64,
        Some(IrType::ListFloat) => IrType::F64,
        Some(IrType::ListString) => IrType::String,
        _ => {
            return Err(cap!(
                "lower_comprehension.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "comprehension iterable must be a List<Int>, List<Float>, or List<String> in the AOT envelope, got {:?}",
                        src_ty
                    ),
                    range: iterable.range,
                }
            ));
        }
    };

    // Helper: synthesise a single-param closure `(id) => body` over the
    // loop variable and emit `Op::Call(<builtin>)` against a list source
    // already sitting on top of the vstack. The bundled body's fixed
    // closure signature is used directly — a body that doesn't match it is
    // a loud error, not a fall-through (used for the filter pass, whose
    // predicate signature is fixed `elem -> Bool`).
    fn emit_hof_with_synthetic_closure(
        builtin: &'static str,
        id: &str,
        body: &Node,
        range: TokenRange,
        ctx: &mut LowerCtx<'_>,
    ) -> Result<(), LoweringError> {
        let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.1",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.2",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let params = meta.params.clone();
        let ret = meta.ret;
        let (param_tys_c, ret_ty_c) =
            stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
                cap!(
                    "lower_comprehension.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: format!("{builtin} closure signature missing"),
                        range,
                    }
                )
            })?;
        let closure_expr = Expr::Closure {
            params: vec![relon_parser::ClosureParam {
                name: id.to_string(),
                type_hint: None,
                range: body.range,
            }],
            return_type: None,
            body: body.clone(),
        };
        lower_closure_as_value(&closure_expr, body.range, &param_tys_c, ret_ty_c, ctx)?;
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
        Ok(())
    }

    // 2. Optional filter pass `(id) => condition`. The filter retains the
    //    passing source elements unchanged, so the body stays at the
    //    source element type. `List<String>` filter caps (no four-way
    //    `String -> Bool` predicate body), matching the method form.
    if let Some(cond) = condition {
        let filter_builtin = match src_elem {
            IrType::I64 => "list_int_filter",
            IrType::F64 => "list_float_filter",
            _ => {
                return Err(cap!(
                    "lower_comprehension.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: "filtered List<String> comprehension is not compiled yet".to_string(),
                        range,
                    }
                ));
            }
        };
        emit_hof_with_synthetic_closure(filter_builtin, id, cond, range, ctx)?;
    }

    // 3. Map pass `(id) => element`. Probe the closure body against each
    //    candidate result type for this source — homogeneous (src -> src)
    //    first, then the cross-type widths — and select the matching
    //    bundled body, exactly as `peephole::emit_list_hof_call`'s map arm
    //    does. The candidates are mutually exclusive (the body yields one
    //    result slot), so the first probe that accepts wins. A body that
    //    matches no candidate caps loudly.
    let map_candidates: &[(IrType, &'static str)] = match src_elem {
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
    let mut last_err: Option<LoweringError> = None;
    for &(ret_ty, builtin) in map_candidates {
        // Roll-back discipline mirrors `peephole::try_lower_closure_with_ret`:
        // a body that produces a different result slot than `ret_ty` rolls
        // back (out / tstack / next_let_idx / lambda_table) so the next
        // candidate width is tried; any other error propagates.
        let saved_out_len = ctx.out.len();
        let saved_stack_len = ctx.tstack.len();
        let saved_next_let = ctx.next_let_idx;
        let saved_lambda_len = ctx.lambda_table.borrow().len();
        match emit_map_with_ret(builtin, id, element, ret_ty, range, ctx) {
            Ok(()) => return Ok(()),
            Err(LoweringError::StdlibArgTypeMismatch { .. }) => {
                ctx.out.truncate(saved_out_len);
                ctx.tstack.truncate(saved_stack_len);
                ctx.next_let_idx = saved_next_let;
                ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            }
            Err(e) => {
                last_err = Some(e);
                ctx.out.truncate(saved_out_len);
                ctx.tstack.truncate(saved_stack_len);
                ctx.next_let_idx = saved_next_let;
                ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            }
        }
    }
    // No candidate matched — cap loudly with the last concrete error (or a
    // generic unsupported-element error when every candidate rolled back).
    Err(last_err.unwrap_or_else(|| {
        cap!(
            "lower_comprehension.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "comprehension element type is not compiled yet for {:?} source",
                    src_ty
                ),
                range,
            }
        )
    }))
}

/// Map-pass helper for [`lower_comprehension`]: synthesise the
/// single-param closure `(id) => element` with the closure return pinned
/// to `ret_ty` (so the cross-type bundled bodies resolve), then emit
/// `Op::Call(<builtin>)` against the source list already on the vstack.
/// Returns `Err(StdlibArgTypeMismatch)` when the body produces a result
/// slot other than `ret_ty`, which the caller treats as a probe miss.
pub(super) fn emit_map_with_ret(
    builtin: &'static str,
    id: &str,
    body: &Node,
    ret_ty: IrType,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
        cap!(
            "lower_comprehension.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: builtin.to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
        cap!(
            "lower_comprehension.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: builtin.to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let params = meta.params.clone();
    let ret = meta.ret;
    let (param_tys_c, _ret_ty_c) = stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
        cap!(
            "lower_comprehension.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: format!("{builtin} closure signature missing"),
                range,
            }
        )
    })?;
    let closure_expr = Expr::Closure {
        params: vec![relon_parser::ClosureParam {
            name: id.to_string(),
            type_hint: None,
            range: body.range,
        }],
        return_type: None,
        body: body.clone(),
    };
    // Pin the closure return to the candidate `ret_ty` so a body whose
    // result slot differs trips `StdlibArgTypeMismatch` (the probe miss).
    lower_closure_as_value(&closure_expr, body.range, &param_tys_c, ret_ty, ctx)?;
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
    Ok(())
}

pub(super) fn lower_comprehension_as_type(
    expected_element: &TypeRepr,
    element: &Node,
    id: &str,
    iterable: &Node,
    condition: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_expr(&iterable.expr, iterable.range, ctx)?;
    let src_ty = ctx.tstack.last().copied();
    let (filter_builtin, map_builtin) = match src_ty {
        Some(IrType::ListInt) => (Some("list_int_filter"), "list_int_map_to_variant_list"),
        Some(IrType::ListFloat) => (Some("list_float_filter"), "list_float_map_to_variant_list"),
        Some(IrType::ListString) => (None, "list_string_map_to_variant_list"),
        _ => {
            return Err(cap!(
                "lower_comprehension.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "comprehension iterable must be List<Int>, List<Float>, or List<String> for variant-list output, got {:?}",
                        src_ty
                    ),
                    range: iterable.range,
                }
            ))
        }
    };

    fn emit_hof_with_synthetic_closure_as_type(
        builtin: &'static str,
        id: &str,
        body: &Node,
        expected_body_ty: Option<&TypeRepr>,
        range: TokenRange,
        ctx: &mut LowerCtx<'_>,
    ) -> Result<(), LoweringError> {
        let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.1",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.2",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let params = meta.params.clone();
        let ret = meta.ret;
        let (param_tys_c, ret_ty_c) =
            stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
                cap!(
                    "lower_comprehension.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: format!("{builtin} closure signature missing"),
                        range,
                    }
                )
            })?;
        let closure_expr = Expr::Closure {
            params: vec![relon_parser::ClosureParam {
                name: id.to_string(),
                type_hint: None,
                range: body.range,
            }],
            return_type: None,
            body: body.clone(),
        };
        if let Some(expected) = expected_body_ty {
            lower_closure_as_value_with_expected_type(
                &closure_expr,
                body.range,
                &param_tys_c,
                ret_ty_c,
                None,
                Some(expected),
                ctx,
            )?;
        } else {
            lower_closure_as_value(&closure_expr, body.range, &param_tys_c, ret_ty_c, ctx)?;
        }
        ctx.tstack.pop();
        ctx.tstack.pop();
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
        Ok(())
    }

    if let Some(cond) = condition {
        let Some(filter_builtin) = filter_builtin else {
            return Err(cap!(
                "lower_comprehension.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "filtered List<String> comprehension is not compiled yet".to_string(),
                    range,
                }
            ));
        };
        emit_hof_with_synthetic_closure_as_type(filter_builtin, id, cond, None, range, ctx)?;
    }
    emit_hof_with_synthetic_closure_as_type(
        map_builtin,
        id,
        element,
        Some(expected_element),
        range,
        ctx,
    )?;
    Ok(())
}
