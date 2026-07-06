//! Lowering sub-module: reference (`&sibling` / `&root`), variable
//! path, and enum payload path lowering.
//!
//! Covers backward-static reference desugaring against the
//! source-ordered field-let graph, `lower_variable`'s let / param /
//! schema-field path walk, and the direct enum payload load path
//! (`x.Variant.field`) with its base validation.

use super::*;

/// R10: lower a *backward static* `&sibling.<name>` / `&root.<name>`
/// reference on the compiled path.
///
/// At the entry-level dict (the `#main -> Dict` anon-Dict-return body)
/// the entry dict IS the document root, so `&sibling.<name>` and
/// `&root.<name>` resolve to the very same field — both bases are
/// handled here. The runtime contract for a `&sibling`/`&root` whose
/// single trailing segment names an earlier field in the *same* dict is
/// identical to a bare let reference, so this reuses the source-ordered
/// field-let graph: each host-visible scalar field is registered as a
/// `LetBinding` (see [`lower_anon_dict_body`]) before later fields
/// lower, exactly as `lower_where` / the closure / dict / list-string
/// fields already do. We resolve `<name>` to that let-idx and emit the
/// same `LetGet` (or scalar-const inline) `lower_variable` would.
///
/// Everything outside that narrow shape is a loud cap, NOT a silent
/// fallback:
///
/// * Positional / runtime / grandparent bases — `&uncle` / `&prev` /
///   `&next` / `&index` / `&this` — need loop-carried or cross-dict
///   state the compiled entry body does not model, so they cap.
/// * A forward reference (the name is not yet bound — declared later in
///   source order) is not in `ctx.lets` and caps via the unresolved
///   path; the backward-only contract is what keeps the value
///   well-defined at lowering time.
/// * Dynamic-key segments and multi-segment paths (`&sibling.x.y`) cap;
///   only a single static `String` segment is lowered. `#internal`
///   sibling fields are dropped from the compiled plan entirely, so a
///   reference to one never resolves and caps — this also sidesteps the
///   `&sibling.<priv>`-allowed vs `&root.<priv>`-blocked privacy split,
///   since neither form can reach a private field here.
pub(super) fn lower_reference(
    base: RefBase,
    path: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Only the entry-level-equivalent bases. Positional/runtime bases
    // are honestly out of the compiled path's reach.
    if !matches!(base, RefBase::Sibling | RefBase::Root) {
        return Err(cap!(
            "lower_reference.positional_base",
            LoweringError::UnsupportedExpr {
                kind: format!("Reference(positional base {base:?} not supported on compiled path)"),
                range,
            }
        ));
    }
    // Exactly one static String segment — no dynamic keys, no chaining.
    let name = match path {
        [TokenKey::String(name, _, _)] => name.as_str(),
        _ => {
            return Err(cap!(
                "lower_reference.unsupported_path_shape",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Reference(only a single static field segment is supported, got {path:?})"
                    ),
                    range,
                }
            ));
        }
    };
    // R13: the named field must be bound as a let. The body walker emits
    // anon-Dict fields in topological order over their reference edges,
    // so a `&sibling` / `&root` reference's target field — declared
    // earlier (backward) *or* later (forward) in source — is already
    // registered as a let by the time this reference lowers. Inline a
    // scalar-const let to the literal exactly as `lower_variable` does so
    // all backends fold an identical compile-time value; otherwise emit
    // the `LetGet`. A reference cycle never reaches here: it is rejected
    // up front in `anon_dict_emit_order`.
    if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == name).cloned() {
        if let Some(sc) = ctx.const_let_values.get(&b.idx).copied() {
            let (op, ty) = match sc {
                ScalarConst::I64(i) => (Op::ConstI64(i), IrType::I64),
                ScalarConst::F64(f) => (Op::ConstF64(OrderedFloat::from(f)), IrType::F64),
                ScalarConst::Bool(b) => (Op::ConstBool(b), IrType::Bool),
            };
            ctx.out.push(TaggedOp { op, range });
            ctx.tstack.push(ty);
            return Ok(());
        }
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: b.idx,
                ty: b.ty,
            },
            range,
        });
        ctx.tstack.push(b.ty);
        return Ok(());
    }
    // The name binds to no in-scope let. On this surface that means a
    // reference to an `#internal` sibling (dropped from the compiled
    // plan) or to a name that is not a host-visible sibling field at all.
    Err(cap!(
        "lower_reference.unresolved_field",
        LoweringError::UnresolvedVariable {
            name: name.to_string(),
            range,
        }
    ))
}

// NOTE (orphaned doc, stranded by a refactor — kept for reference):
// Lower a bare-identifier reference. Phase 3.a checks the user-let
// scope first (innermost shadow wins) and falls back to the `#main`
// parameter index. The let-binding hit emits an `Op::LetGet`; the
// param hit emits a typed `Op::LoadField` reading from the `in_buf`.
// Phase 5 extends the surface in two ways:
// * `self` (when the lowering context owns a `self_binding`) lowers
//   to the wasm-local that holds the schema instance's absolute address.
// * Multi-segment paths whose head resolves to a schema-typed binding
//   chase field offsets through the schema's layout chain, emitting
//   `Op::LoadFieldAtAbsolute` per segment.

pub(super) fn enum_payload_field_name(
    segment: &TokenKey,
    variant: &CanonicalEnumVariant,
) -> Option<String> {
    match segment {
        TokenKey::String(name, _, optional) if !*optional => Some(name.clone()),
        TokenKey::Index(index, optional) if !*optional && variant.is_tuple => {
            Some(index.to_string())
        }
        _ => None,
    }
}

pub(super) fn direct_payload_load_op(ty: IrType, offset: u32) -> Op {
    match ty {
        IrType::I64 => Op::LoadI64AtAbsolute { offset },
        IrType::F64 => Op::LoadF64AtAbsolute { offset },
        IrType::Bool => Op::LoadI8UAtAbsolute { offset },
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
        | IrType::Dict => Op::LoadI32AtAbsolute { offset },
    }
}

pub(super) fn validate_enum_payload_base(
    ctx: &mut LowerCtx<'_>,
    range: TokenRange,
) -> Result<(), LoweringError> {
    let base_ty = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_variable.unsupported_expr.enum_payload_stack",
            LoweringError::UnsupportedExpr {
                kind: "Enum(payload access without a variant pointer)".to_string(),
                range,
            }
        )
    })?;
    if base_ty != IrType::I32 {
        return Err(cap!(
            "lower_variable.unsupported_expr.enum_payload_stack_type",
            LoweringError::UnsupportedExpr {
                kind: format!("Enum(payload access expected I32 variant pointer, got {base_ty:?}"),
                range,
            }
        ));
    }
    Ok(())
}

pub(super) fn lower_enum_payload_path(
    path_tail: &[TokenKey],
    narrowing: &EnumVariantNarrowing,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path_tail.len() != 1 {
        return Err(cap!(
            "lower_variable.unsupported_expr.enum_payload_path",
            LoweringError::UnsupportedExpr {
                kind: "Enum(payload access with more than one segment)".to_string(),
                range,
            }
        ));
    }
    let Some(field_name) = enum_payload_field_name(&path_tail[0], &narrowing.variant) else {
        return Err(cap!(
            "lower_variable.unsupported_expr.enum_payload_segment",
            LoweringError::UnsupportedExpr {
                kind: "Enum(payload access expects a field name or tuple index)".to_string(),
                range,
            }
        ));
    };

    if let Some(payload) = &narrowing.direct_payload {
        if field_name != payload.field_name {
            return Err(cap!(
                "lower_variable.unsupported_expr.enum_payload_unknown_field",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            ));
        }
        let payload_slot_offset = variant_payload_offset_for_lowering(&payload.ty)? as u32;
        let field_ir = type_repr_to_ir_type_dict(&payload.ty)?;
        ctx.out.push(TaggedOp {
            op: direct_payload_load_op(field_ir, payload_slot_offset),
            range,
        });
        validate_enum_payload_base(ctx, range)?;
        ctx.tstack.push(field_ir);
        return Ok(());
    }

    let payload_schema = narrowing
        .variant
        .payload_schema(&narrowing.enum_name)
        .ok_or_else(|| {
            cap!(
                "lower_variable.unsupported_expr.enum_unit_payload_access",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(unit variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })?;
    let field_meta = payload_schema
        .fields
        .iter()
        .find(|field| field.name == field_name)
        .ok_or_else(|| {
            cap!(
                "lower_variable.unsupported_expr.enum_payload_unknown_field",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })?;
    let payload_ty = TypeRepr::Schema {
        schema: Box::new(payload_schema.clone()),
    };
    let payload_slot_offset = variant_payload_offset_for_lowering(&payload_ty)? as u32;
    let layout = SchemaLayout::offsets_for(&payload_schema)?;
    let field_slot = layout
        .fields
        .iter()
        .find(|slot| slot.name == field_meta.name)
        .ok_or_else(|| {
            cap!(
                "lower_variable.unsupported_expr.enum_payload_missing_layout",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` payload field `{}` has no layout slot)",
                        narrowing.variant.name, field_meta.name
                    ),
                    range,
                }
            )
        })?;
    let field_ir = type_repr_to_ir_type_dict(&field_meta.ty)?;
    ctx.out.push(TaggedOp {
        op: Op::LoadI32AtAbsolute {
            offset: payload_slot_offset,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LoadFieldAtAbsolute {
            offset: field_slot.offset as u32,
            ty: field_ir,
        },
        range,
    });
    validate_enum_payload_base(ctx, range)?;
    ctx.tstack.push(field_ir);
    Ok(())
}

pub(super) fn lower_variable(
    path: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path.is_empty() {
        return Err(cap!(
            "lower_variable.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "Variable(empty-path)".to_string(),
                range,
            }
        ));
    }
    let head = match &path[0] {
        TokenKey::String(s, _, _) => s.as_str(),
        TokenKey::Index(_, _) | TokenKey::Dummy | TokenKey::Spread(_) | TokenKey::Dynamic(_, _) => {
            return Err(cap!(
                "lower_variable.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "Variable(non-string-key)".to_string(),
                    range,
                }
            ));
        }
    };
    let enum_narrowing = ctx.enum_variant_narrowing.get(head).cloned();
    // #359 (W20 container perf): a bare reference to a where-bound
    // SCALAR CONSTANT let (`soft` / `dt` / a mass) lowers to the literal
    // `Op::Const*` directly instead of a `LetGet` (an alloca load
    // pre-mem2reg, or — when captured by a closure — an opaque load from
    // the arena captures struct). Folding it to a compile-time constant
    // lets LLVM's `-O3` value-range / arithmetic simplification see the
    // real value (`dx*dx + 0.1`) instead of an opaque load, recovering
    // the scalar half of the W20 inner-loop overhead (2.14x -> ~1.69x on
    // s90). Restricted to single-segment paths (`path.len() == 1` — no
    // field/index chaining) and to lets recorded in `const_let_values`;
    // the inlined literal is the exact source value, so all backends
    // compute a bit-identical result. Scalar-let shadowing is respected
    // because `const_let_values` is keyed by the same let-idx the
    // innermost binding resolves to.
    if path.len() == 1 {
        if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
            if let Some(sc) = ctx.const_let_values.get(&b.idx).copied() {
                let (op, ty) = match sc {
                    ScalarConst::I64(i) => (Op::ConstI64(i), IrType::I64),
                    ScalarConst::F64(f) => (Op::ConstF64(OrderedFloat::from(f)), IrType::F64),
                    ScalarConst::Bool(b) => (Op::ConstBool(b), IrType::Bool),
                };
                ctx.out.push(TaggedOp { op, range });
                ctx.tstack.push(ty);
                return Ok(());
            }
        }
    }
    // The walker pushes a value onto the vstack representing the
    // root binding's IR type plus, for schema-typed roots, the
    // canonical schema shape and brand so chained field offsets can
    // be resolved deeper down the path.
    let mut current_schema: Option<Schema>;
    if let Some(self_b) = ctx.self_binding.clone() {
        if head == "self" {
            ctx.out.push(TaggedOp {
                op: Op::LocalGet(self_b.wasm_local_idx),
                range,
            });
            ctx.tstack.push(IrType::I32);
            current_schema = Some(self_b.schema.clone());
        } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head).cloned() {
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: b.idx,
                    ty: b.ty,
                },
                range,
            });
            ctx.tstack.push(b.ty);
            current_schema = b
                .schema_brand
                .as_deref()
                .and_then(|n| ctx.schema_resolver.resolve(n))
                .and_then(|def| {
                    let mut stack: Vec<&str> = Vec::new();
                    canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, range).ok()
                });
        } else if let Some(p) = ctx.method_params.iter().find(|p| p.name == head).cloned() {
            ctx.out.push(TaggedOp {
                op: Op::LocalGet(p.wasm_local_idx),
                range,
            });
            ctx.tstack.push(p.ty);
            current_schema = p.schema;
        } else {
            return Err(cap!(
                "lower_variable.unresolved_variable.1",
                LoweringError::UnresolvedVariable {
                    name: head.to_string(),
                    range,
                }
            ));
        }
    } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head).cloned() {
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: b.idx,
                ty: b.ty,
            },
            range,
        });
        ctx.tstack.push(b.ty);
        current_schema = b
            .schema_brand
            .as_deref()
            .and_then(|n| ctx.schema_resolver.resolve(n))
            .and_then(|def| {
                let mut stack: Vec<&str> = Vec::new();
                canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, range).ok()
            });
    } else {
        let binding = ctx
            .params
            .iter()
            .find(|b| b.name == head)
            .cloned()
            .ok_or_else(|| {
                cap!(
                    "lower_variable.unresolved_variable.2",
                    LoweringError::UnresolvedVariable {
                        name: head.to_string(),
                        range,
                    }
                )
            })?;
        // Pointer-indirect leaves (`String` / `ListInt`) get their own
        // op tag so a later phase can hang String / List operations
        // off them without re-deriving the type from the slot. Schema
        // params lift the buffer-relative pointer to an absolute
        // address via `LoadSchemaPtr`.
        let op = match (binding.ty, binding.schema_brand.as_deref()) {
            (IrType::I32, Some(_)) => Op::LoadSchemaPtr {
                offset: binding.offset,
            },
            (IrType::String, _) => Op::LoadStringPtr {
                offset: binding.offset,
            },
            (IrType::ListInt, _) => Op::LoadListIntPtr {
                offset: binding.offset,
            },
            (IrType::ListFloat, _) => Op::LoadListFloatPtr {
                offset: binding.offset,
            },
            (IrType::ListBool, _) => Op::LoadListBoolPtr {
                offset: binding.offset,
            },
            (IrType::ListString, _) => Op::LoadListStringPtr {
                offset: binding.offset,
            },
            (IrType::ListSchema, _) => Op::LoadListSchemaPtr {
                offset: binding.offset,
            },
            (IrType::ListList, _) => Op::LoadListListPtr {
                offset: binding.offset,
            },
            _ => Op::LoadField {
                offset: binding.offset,
                ty: binding.ty,
            },
        };
        ctx.out.push(TaggedOp { op, range });
        ctx.tstack.push(binding.ty);
        current_schema = binding.schema.clone();
    }

    // Walk any remaining segments against the schema layout chain.
    // Each segment pops the i32 absolute address, computes the
    // field's offset + IR type from the canonical schema, and emits a
    // matching `LoadFieldAtAbsolute`. The pushed type adopts the
    // field's IR shape; schema-typed fields preserve the brand for
    // further chained access.
    if path.len() == 1 {
        return Ok(());
    }
    if let Some(narrowing) = enum_narrowing.as_ref() {
        return lower_enum_payload_path(&path[1..], narrowing, range, ctx);
    }
    // AOT-4 (W16 slice): 1D `xs[i]` index on a materialised `List<Int>`
    // receiver. The parser lowers the bracket form to a single trailing
    // `TokenKey::Dynamic(<index Node>)` segment after the root name (a
    // dotted `xs.0` would arrive as `TokenKey::Index`, which we do NOT
    // accept here — the materialised-list index path is bracket-only).
    // The head pushed an `IrType::ListInt` arena handle (i32); the index
    // is read with inline payload addressing that mirrors the record
    // layout the bundled `list_int_*` bodies write
    // (`stdlib::defs::list_filter_body_typed`): `[len: u32 LE][pad: u32]
    // [i64 elements...]`, payload at `(base + 4 + 7) & -8`, element `i`
    // at `payload + i*8`. The load is emitted WITHOUT a bounds branch —
    // every caller in scope (the W16 quicksort kernel) guards
    // `_len(xs) <= 1` before reaching `xs[0]`, so the index is provably
    // in-bounds on the hot path. A shape we cannot prove in-bounds is
    // DECLINED (falls through to the generic non-string-segment
    // diagnostic) rather than emitting a possibly-wrong load. We do NOT
    // emit `Op::ListGetByIntIdx` (that op is trace-recorder-only; static
    // codegen rejects it).
    // AOT-4 (W19 slice): generalise to a CHAIN of trailing `Dynamic`
    // index segments so 2D `a[i][k]` (and any N-D `xs[i][j]...`) on a
    // materialised `List<List<Int>>` lowers. A `List<List<Int>>` is the
    // outer `List<Int>` record whose i64 elements are i32 arena offsets
    // of inner `List<Int>` rows (the materialiser writes the handle
    // truncated into the i64 element slot — see
    // `emit_list_value_materialize`). An outer index `a[i]` therefore
    // loads an i64 whose low 32 bits ARE the inner row's arena handle;
    // to index it again the i64 is round-tripped through a ListInt
    // let-slot (`LetSet{ListInt}` truncates i64->i32) so the next
    // `lower_list_int_index` sees a properly tagged `ListInt` receiver.
    // The FINAL segment loads the i64 cell value. Inline payload
    // addressing throughout (NO `Op::ListGetByIntIdx`, NO bounds branch
    // — every W19 index is provably within `range(size)`).
    if path.len() >= 2
        && path[1..]
            .iter()
            .all(|s| matches!(s, TokenKey::Dynamic(_, _)))
    {
        let receiver_ty = ctx.tstack.last().copied();
        if receiver_ty == Some(IrType::ListInt) {
            let last = path.len() - 1;
            for (off, seg) in path[1..].iter().enumerate() {
                let TokenKey::Dynamic(index_node, optional) = seg else {
                    unreachable!("guarded by the all-Dynamic check above");
                };
                // Optional indexing (`xs[i]?`) needs an Option.None-or-value
                // result the i64 element path can't represent; decline.
                if *optional {
                    return Err(cap!(
                        "lower_variable.unsupported_expr.3",
                        LoweringError::UnsupportedExpr {
                            kind: "Variable(optional-list-index unsupported)".to_string(),
                            range,
                        }
                    ));
                }
                // Pops the `ListInt` receiver, pushes the i64 element.
                lower_list_int_index(index_node, range, ctx)?;
                // Not the last segment: the loaded i64 is an inner row
                // handle — retag it as `ListInt` for the next index step.
                if 1 + off != last {
                    let handle_i = ctx.next_let_idx;
                    ctx.next_let_idx += 1;
                    ctx.out.push(TaggedOp {
                        op: Op::LetSet {
                            idx: handle_i,
                            ty: IrType::ListInt,
                        },
                        range,
                    });
                    ctx.tstack.pop(); // i64 element
                    ctx.out.push(TaggedOp {
                        op: Op::LetGet {
                            idx: handle_i,
                            ty: IrType::ListInt,
                        },
                        range,
                    });
                    ctx.tstack.push(IrType::ListInt);
                }
            }
            return Ok(());
        }
        // #359 (W20): 1D `s[k]` index on a `List<Float>` receiver — the
        // n-body state list (`init` / `final_state` / the reducer's `s`
        // param). The record layout is identical to `List<Int>` (8-byte
        // elements); only the element load is `f64` and the result rides
        // as `F64`. A `List<Float>`-of-`List<Float>` does not occur in
        // W20, so only the single trailing-index form is accepted here.
        if receiver_ty == Some(IrType::ListFloat) && path.len() == 2 {
            let TokenKey::Dynamic(index_node, optional) = &path[1] else {
                unreachable!("guarded by the all-Dynamic check above");
            };
            if *optional {
                return Err(cap!(
                    "lower_variable.unsupported_expr.4",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-list-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            // Pops the `ListFloat` receiver, pushes the f64 element.
            lower_list_index_typed(index_node, IrType::ListFloat, range, ctx)?;
            return Ok(());
        }
        // W5-P2: 1D `keys[i]` index on a `List<String>` receiver — the
        // dict-probe `keys[i % 10]` workload, plus the standalone
        // `["a", .., "j"][i]` form. A `List<String>` record is a
        // *pointer array*: `[len: u32][off_0: u32]...[off_{N-1}: u32]`
        // header whose `off_i` is the arena-relative byte offset of the
        // i-th String record (`[slen: u32][utf8]`). Indexing it loads
        // the `u32` slot — which IS a `String` handle (the same i32
        // arena offset `ConstString` pushes) — so the result rides on
        // the vstack tagged `String` and any downstream consumer (the
        // String-return tail-record copy) sees a normal String value.
        // Only the single trailing-index form is accepted (no
        // `List<List<String>>` in scope).
        if receiver_ty == Some(IrType::ListString) && path.len() == 2 {
            let TokenKey::Dynamic(index_node, optional) = &path[1] else {
                unreachable!("guarded by the all-Dynamic check above");
            };
            if *optional {
                return Err(cap!(
                    "lower_variable.unsupported_expr.5",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-list-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            // Pops the `ListString` receiver, pushes the String handle.
            lower_list_string_index(index_node, range, ctx)?;
            return Ok(());
        }
        // W5-P3: 1D `d[k]` index on a materialised `{String -> Int}`
        // dict receiver — the dict-probe workload. `d` is an
        // `IrType::Dict` arena handle (pushed by `Op::ConstDict` /
        // `LetGet{Dict}`); the bracket index `k` lowers to a runtime
        // `String` handle (a `keys[i]` element or a `ConstString`). The
        // probe is a fully IR-lowered linear scan + byte compare over
        // the arena entry table, so native + wasm32 need no new runtime
        // import. Only the single trailing-index form is accepted (no
        // nested dict-of-dict in scope).
        if receiver_ty == Some(IrType::Dict) && path.len() == 2 {
            let TokenKey::Dynamic(index_node, optional) = &path[1] else {
                unreachable!("guarded by the all-Dynamic check above");
            };
            if *optional {
                return Err(cap!(
                    "lower_variable.unsupported_expr.6",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-dict-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            // Pops the `Dict` receiver, pushes the i64 value (Int).
            lower_dict_string_index(index_node, range, ctx)?;
            return Ok(());
        }
        // A `Dynamic` segment on a non-list receiver is not a
        // materialised-list index — fall through to the generic
        // diagnostic below so the rejection message stays precise.
    }
    for seg in &path[1..] {
        let Some(schema) = current_schema.clone() else {
            return Err(cap!(
                "lower_variable.unsupported_expr.8",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Variable(field-on-non-schema-base, segment=`{}`)",
                        token_key_display(seg)
                    ),
                    range,
                }
            ));
        };
        let field_name: std::borrow::Cow<'_, str> = match seg {
            TokenKey::String(s, _, _) => std::borrow::Cow::Borrowed(s.as_str()),
            TokenKey::Index(i, optional) if schema.is_tuple && !*optional => {
                std::borrow::Cow::Owned(i.to_string())
            }
            TokenKey::Index(_, true) if schema.is_tuple => {
                return Err(cap!(
                    "lower_variable.unsupported_expr.7",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-tuple-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            _ => {
                return Err(cap!(
                    "lower_variable.unsupported_expr.7",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(non-string-segment)".to_string(),
                        range,
                    }
                ));
            }
        };
        // Recompute the layout for the current schema shape. Cached
        // canonical schemas are reused across calls so the resolver
        // doesn't repeatedly re-walk the analyzer tree.
        let layout = SchemaLayout::offsets_for(&schema)?;
        let field_idx = schema
            .fields
            .iter()
            .position(|f| f.name == field_name.as_ref())
            .ok_or_else(|| {
                cap!(
                    "lower_variable.unsupported_field_type",
                    LoweringError::UnsupportedFieldType {
                        schema: schema.name.clone(),
                        field: field_name.to_string(),
                        ty: "(unknown field)".to_string(),
                        range,
                    }
                )
            })?;
        let field_meta = &schema.fields[field_idx];
        let layout_field = &layout.fields[field_idx];
        // Pop the base address.
        let popped = ctx.tstack.pop().ok_or(cap!(
            "lower_variable.unsupported_expr.9",
            LoweringError::UnsupportedExpr {
                kind: "Variable(field-load-stack-empty)".to_string(),
                range,
            }
        ))?;
        if popped.wasm_slot() != IrType::I32 {
            return Err(cap!(
                "lower_variable.unsupported_expr.10",
                LoweringError::UnsupportedExpr {
                    kind: format!("Variable(field-base-not-i32, got={:?})", popped),
                    range,
                }
            ));
        }
        let field_ir = type_repr_to_ir_type_dict(&field_meta.ty)?;
        ctx.out.push(TaggedOp {
            op: Op::LoadFieldAtAbsolute {
                offset: layout_field.offset as u32,
                ty: field_ir,
            },
            range,
        });
        ctx.tstack.push(field_ir);
        // Update walking state for the next segment.
        current_schema = match &field_meta.ty {
            TypeRepr::Schema { schema } => Some((**schema).clone()),
            _ => None,
        };
    }
    Ok(())
}
