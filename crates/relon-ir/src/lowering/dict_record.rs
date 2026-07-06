//! Lowering sub-module: dict / tuple literal → record construction,
//! including dict-spread sources and schema field defaults.
//!
//! Resolves user pairs plus `..spread` contributions against the
//! canonical schema, computes the topological field emit order, and
//! stores each field into the in-construction record
//! (`StoreFieldAtRecord` chains). Also owns schema-default lowering
//! for fields the literal leaves unset.

use super::*;

/// Lower a dict literal into the in-construction record at
/// `record_local`. The schema describes the record's shape; the
/// `OffsetTable` carries field offsets; `dict_pairs` are the user-
/// supplied fields.
///
/// Steps:
///   1. Resolve user pairs to a (name, expr) map.
///   2. Compute topological emit order from the schema defaults.
///   3. For each field in topo order, lower the value expression
///      (either user-provided or schema default) and emit the
///      matching `StoreFieldAtRecord` op.
///
/// Synthesise a `source.field` field-access node from a dict-spread
/// source expression and a contributed field name. The source is
/// expected to be a `Variable(path)`; the synthesised access appends a
/// `String(field)` segment so it lowers through the existing
/// [`lower_variable`] schema field-walk (`LoadFieldAtAbsolute` chain).
/// The synthesised node carries the source's range so diagnostics point
/// back at the spread site.
pub(super) fn synthesize_field_access(source: &Node, field: &str) -> Node {
    let mut path = match source.expr.as_ref() {
        Expr::Variable(segs) => segs.clone(),
        // Non-`Variable` sources are rejected before this point by
        // `spread_source_schema`; fall back to an empty path so the
        // synthesised access loud-errors in `lower_variable` rather than
        // silently mis-lowering.
        _ => Vec::new(),
    };
    path.push(TokenKey::String(field.to_string(), source.range, false));
    Node::new(Expr::Variable(path), source.range)
}

/// Resolve the canonical [`Schema`] of a dict-spread source expression so
/// the fields it contributes are statically known. Only a
/// **statically-resolvable schema value** is admitted: a `Variable(path)`
/// whose root binds a schema-typed `#main` parameter / let / `self`,
/// optionally walking trailing field segments into nested schema fields.
///
/// Anything else (a non-`Variable` source, a non-schema root, a dynamic /
/// index segment, a field whose type is not itself a schema) is not
/// statically flattenable on the compiled path and caps loudly — the
/// silent-miscompile path is unreachable. This is the dict counterpart of
/// the list-spread `flatten_list_spread` static guard.
pub(super) fn spread_source_schema(
    source: &Node,
    ctx: &LowerCtx<'_>,
    range: TokenRange,
) -> Result<Schema, LoweringError> {
    let Expr::Variable(path) = source.expr.as_ref() else {
        return Err(cap!(
            "spread_source_schema.non_variable",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Dict(spread source `{}` is not a statically-resolvable schema value — \
                     compiled dict spread needs a schema-typed identifier source)",
                    source.expr.kind()
                ),
                range,
            }
        ));
    };
    let mut segs = path.iter();
    let head = match segs.next() {
        Some(TokenKey::String(s, _, _)) => s.as_str(),
        _ => {
            return Err(cap!(
                "spread_source_schema.non_string_head",
                LoweringError::UnsupportedExpr {
                    kind: "Dict(spread source root is not a bare identifier)".to_string(),
                    range,
                }
            ));
        }
    };

    // Resolve the root binding's canonical schema, mirroring the
    // root-resolution order in `lower_variable` (self → let → method
    // param → entry param).
    let resolve_brand = |brand: Option<&str>| -> Option<Schema> {
        brand
            .and_then(|n| ctx.schema_resolver.resolve(n))
            .and_then(|def| {
                let mut stack: Vec<&str> = Vec::new();
                canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, range).ok()
            })
    };
    let mut current_schema: Option<Schema> = if let Some(self_b) = ctx.self_binding.as_ref() {
        if head == "self" {
            Some(self_b.schema.clone())
        } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
            resolve_brand(b.schema_brand.as_deref())
        } else if let Some(p) = ctx.method_params.iter().find(|p| p.name == head) {
            p.schema.clone()
        } else {
            None
        }
    } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
        resolve_brand(b.schema_brand.as_deref())
    } else {
        ctx.params
            .iter()
            .find(|b| b.name == head)
            .and_then(|b| b.schema.clone())
    };

    // Walk any trailing segments into nested schema fields.
    for seg in segs {
        let field_name = match seg {
            TokenKey::String(s, _, _) => s.as_str(),
            _ => {
                return Err(cap!(
                    "spread_source_schema.non_string_segment",
                    LoweringError::UnsupportedExpr {
                        kind: "Dict(spread source path has a non-field segment)".to_string(),
                        range,
                    }
                ));
            }
        };
        let next = current_schema
            .as_ref()
            .and_then(|s| s.fields.iter().find(|f| f.name == field_name))
            .and_then(|f| match &f.ty {
                TypeRepr::Schema { schema } => Some((**schema).clone()),
                _ => None,
            });
        current_schema = next;
    }

    current_schema.ok_or_else(|| {
        cap!(
            "spread_source_schema.not_a_schema",
            LoweringError::UnsupportedExpr {
                kind: "Dict(spread source does not resolve to a statically-known schema value)"
                    .to_string(),
                range,
            }
        )
    })
}

/// Nested branded dicts recurse via the same helper after allocating
/// a fresh sub-record.
pub(super) fn lower_dict_into_record(
    schema: &Schema,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    range: TokenRange,
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Resolve the analyzer-side SchemaDef so default expressions can
    // be lowered. The canonical Schema we have here only carries
    // field name + type — defaults live on the SchemaDef.
    let def = ctx.schema_resolver.resolve(&schema.name).ok_or_else(|| {
        cap!(
            "lower_dict_into_record.unknown_schema_brand",
            LoweringError::UnknownSchemaBrand {
                name: schema.name.clone(),
                range,
            }
        )
    })?;

    // Build name → user-expr map. Reject duplicate keys. Values are a
    // `Cow`: an explicit `k: v` field borrows the source node; a field
    // contributed by a `...source` spread is a synthesised
    // `source.field` access (owned).
    let mut user_values: HashMap<String, std::borrow::Cow<'_, Node>> = HashMap::new();
    for (key, value) in dict_pairs {
        // Wave R12-lower: dict spread `{ ...source, k: v } -> Schema`.
        // Each field the spread source contributes (and that the result
        // schema declares) is lowered as a synthesised `source.field`
        // access into the matching schema slot — matching the tree-walk
        // `Expr::Dict` spread branch (the source's keys merge into the
        // result; the analyzer already rejects duplicate keys via
        // `DuplicateField`, so no later-key override is ever silently
        // applied). The source must be a statically-resolvable schema
        // value (a schema-typed param / let / `self`); anything else
        // caps loudly in `spread_source_schema`.
        if let TokenKey::Spread(_) = key {
            let src_schema = spread_source_schema(value, ctx, range)?;
            for src_field in &src_schema.fields {
                // Only fields the result schema declares are merged. A
                // source field absent from the result is dropped exactly
                // as the tree-walk merge would (the result `Value::Dict`
                // only keeps keys the schema validates) — but because the
                // analyzer brands the result to `schema`, every source
                // field the result keeps is one it declares; a source
                // field the result does NOT declare is not a result key.
                if !schema.fields.iter().any(|f| f.name == src_field.name) {
                    continue;
                }
                let access = synthesize_field_access(value, &src_field.name);
                if user_values
                    .insert(src_field.name.clone(), std::borrow::Cow::Owned(access))
                    .is_some()
                {
                    return Err(cap!(
                        "lower_dict_into_record.duplicate_spread_field",
                        LoweringError::UnsupportedFieldType {
                            schema: schema.name.clone(),
                            field: src_field.name.clone(),
                            ty: "duplicate field produced by spread".to_string(),
                            range,
                        }
                    ));
                }
            }
            continue;
        }
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "lower_dict_into_record.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("Dict(non-string-key in branded dict for `{}`)", schema.name),
                    range,
                }
            ));
        };
        // Schema must declare this field.
        if !schema.fields.iter().any(|f| &f.name == name) {
            return Err(cap!(
                "lower_dict_into_record.unsupported_field_type.1",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: format!("(unknown field, not declared on `{}`)", schema.name),
                    range,
                }
            ));
        }
        if user_values
            .insert(name.clone(), std::borrow::Cow::Borrowed(value))
            .is_some()
        {
            return Err(cap!(
                "lower_dict_into_record.duplicate_field",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: "duplicate field".to_string(),
                    range,
                }
            ));
        }
    }

    let user_set: std::collections::HashSet<&str> =
        user_values.keys().map(|s| s.as_str()).collect();
    let order = topo_order_fields(&schema.name, def, &user_set, range)?;

    for idx in order {
        let canonical_field = &schema.fields[idx];
        // `SchemaLayout::offsets_for` walks `schema.fields` in
        // declaration order, so `layout.fields[i].name ==
        // schema.fields[i].name` is invariant by construction.
        let layout_field = &layout.fields[idx];
        debug_assert_eq!(layout_field.name, canonical_field.name);
        let field_range = def.fields[idx].value_range;
        // Lower the value expression (user-supplied or schema default).
        if let Some(user_value) = user_values.get(canonical_field.name.as_str()) {
            // Wave R11: a field decorator on a branded `-> Schema` return
            // field is not yet desugared on the compiled path (only the
            // anon-Dict-return surface is). Cap loudly rather than lower
            // the raw value and silently drop the decorator transform —
            // that would diverge from the tree-walk oracle.
            if !user_value.decorators.is_empty() {
                return Err(cap!(
                    "lower_dict_into_record.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: schema.name.clone(),
                        field: canonical_field.name.clone(),
                        ty: "field decorator on a branded `-> Schema` return field is not yet \
                             lowered (only anon-Dict-return field decorators desugar today)"
                            .to_string(),
                        range: user_value.range,
                    }
                ));
            }
            lower_dict_field_value(schema, idx, user_value.as_ref(), user_value.range, ctx)?;
        } else {
            // Schema default. Re-bind `#main` params; let-scope is
            // shared with the surrounding body (defaults sit at the
            // schema-instantiation site, not inside an isolated
            // scope, so referenced field names already resolved
            // through the topo-ordered store ops above are reachable
            // via `LetGet` over the per-field default-local — see
            // below for the sibling lookup mechanism).
            //
            // For Phase 3.b sibling field references are resolved
            // through the lowered value expression directly: the
            // default expression `a + 1` lowers to `LetGet { idx:
            // sibling_let_of_a }`. That trick requires us to keep a
            // per-record map from field name → let-local index when
            // a field's value is consumed by a later default. The
            // simpler shape: emit a `LetSet` for every default-
            // evaluated field so the wasm side caches the value and
            // a later default can read it back via `LetGet`.
            lower_dict_default(
                &schema.name,
                idx,
                &canonical_field.ty,
                def,
                ctx,
                field_range,
            )?;
        }
        // Stack now holds the field's value (with type matching the
        // canonical Field). Emit the StoreFieldAtRecord.
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty)?;
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            // Pointer-indirect fields all store as an i32 pointer.
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_dict_into_record.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Dict field `{}` of `{}` produced no value",
                        canonical_field.name, schema.name
                    ),
                    range,
                }
            )
        })?;
        // Host-visible field-store boundary: compare the full IrType,
        // not the collapsed wasm slot. The host decodes the slot per the
        // schema's declared type, so a String / List* / Dict mistagged
        // onto a differently-typed slot must cap here rather than slip
        // through on a shared i32 slot.
        if top != store_ty {
            return Err(cap!(
                "lower_dict_into_record.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    ty: format!("got {:?}, expected {:?}", top, store_ty),
                    range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: field_range,
        });

        // Cache the freshly-stored value into a let-local so later
        // sibling defaults can `LetGet` it. We only do this for
        // fields the schema's defaults actually reference — but
        // computing that subset requires a second pass. For the
        // Phase 3.b surface we cache *every* field, accepting the
        // unused-local overhead in exchange for simpler bookkeeping.
        // The wasm engine drops unused locals at JIT time.
        //
        // The cache mechanism: re-lower the value into a `LetSet` so
        // the value lives in a wasm local, then map the field name
        // to that let-idx. Because the value has already been
        // consumed by `StoreFieldAtRecord`, we re-emit a `LetGet`
        // that pulls the *stored slot* back through `LoadField`-like
        // semantics — but that's expensive. Simpler: stash the
        // value in a let *before* the StoreFieldAtRecord.
        //
        // Reorder: emit value → LetSet (cache) → LetGet (push back)
        // → StoreFieldAtRecord. The implementation does this by
        // splicing the LetSet/Get pair just before the store.
        //
        // We thread the cache via `ctx`'s let-binding stack so the
        // existing `Variable(name)` lookup resolves to the cached
        // value when a later default emits a reference.
        //
        // Performed below.
        let bound_ty = top;
        let let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        // Reach into the just-emitted op stream: splice
        // [LetSet, LetGet] right before the trailing
        // StoreFieldAtRecord. The current top-of-`out` is that
        // StoreFieldAtRecord (we pushed it just above) — pop, push
        // the cache pair, push it back. Cheaper than re-walking.
        let store_op = ctx.out.pop().expect("StoreFieldAtRecord just pushed");
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: let_idx,
                ty: bound_ty,
            },
            range: field_range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: let_idx,
                ty: bound_ty,
            },
            range: field_range,
        });
        ctx.out.push(store_op);
        ctx.lets.push(LetBinding {
            name: canonical_field.name.clone(),
            idx: let_idx,
            ty: bound_ty,
            schema_brand: None,
            type_repr: None,
        });
    }

    // Pop the field-name let bindings we pushed so the surrounding
    // scope sees its original let stack.
    let drop_count = schema.fields.len();
    let new_len = ctx.lets.len().saturating_sub(drop_count);
    ctx.lets.truncate(new_len);

    Ok(())
}

/// Lower a record literal whose schema is synthetic, such as a custom enum
/// payload. These records have no `SchemaDef`, so there are no defaults or
/// sibling-default references to resolve; every declared field must be present.
pub(super) fn lower_plain_dict_into_record(
    schema: &Schema,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    range: TokenRange,
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let mut user_values: HashMap<String, &Node> = HashMap::new();
    for (key, value) in dict_pairs {
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Dict(non-string-key in payload record for `{}`)",
                        schema.name
                    ),
                    range,
                }
            ));
        };
        if !schema.fields.iter().any(|f| &f.name == name) {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.1",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: format!("(unknown field, not declared on `{}`)", schema.name),
                    range,
                }
            ));
        }
        if user_values.insert(name.clone(), value).is_some() {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: "duplicate field".to_string(),
                    range,
                }
            ));
        }
    }

    for (idx, canonical_field) in schema.fields.iter().enumerate() {
        let layout_field = &layout.fields[idx];
        debug_assert_eq!(layout_field.name, canonical_field.name);
        let Some(user_value) = user_values.get(canonical_field.name.as_str()) else {
            return Err(cap!(
                "lower_plain_dict_into_record.missing_field",
                LoweringError::MissingFieldNoDefault {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    range,
                }
            ));
        };
        if !user_value.decorators.is_empty() {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.3",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    ty: "field decorator on an enum payload field is not lowered".to_string(),
                    range: user_value.range,
                }
            ));
        }
        lower_dict_field_value(schema, idx, user_value, user_value.range, ctx)?;
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty)?;
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_plain_dict_into_record.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Dict field `{}` of `{}` produced no value",
                        canonical_field.name, schema.name
                    ),
                    range,
                }
            )
        })?;
        // Host-visible field-store boundary: compare the full IrType,
        // not the collapsed wasm slot. The host decodes the slot per the
        // schema's declared type, so a String / List* / Dict mistagged
        // onto a differently-typed slot must cap here rather than slip
        // through on a shared i32 slot.
        if top != store_ty {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.4",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    ty: format!("got {:?}, expected {:?}", top, store_ty),
                    range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: user_value.range,
        });
    }
    Ok(())
}

/// Lower the elements of a tuple literal into a positional record.
/// `tuple_schema` is the synthesised anonymous positional-record
/// schema (`is_tuple == true`); `layout` is its offset table; `elements`
/// are the source-level element expressions in declaration order (arity
/// already validated by the caller).
///
/// Each element is lowered like a branded-dict field of the matching
/// canonical type: scalars (`Int` / `Float` / `Bool`) land inline; a
/// `String` element is materialised to an absolute address then copied
/// into the tail area via `EmitTailRecordFromAbsoluteAddr`, leaving an
/// i32 buffer-relative pointer for the slot store — byte-identical to the
/// branded-record path, so the host object-return decode + verifier read
/// it back unchanged (only the final container shape forks to an array).
pub(super) fn lower_tuple_into_record(
    tuple_schema: &Schema,
    layout: &OffsetTable,
    elements: &[Node],
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    for (idx, element) in elements.iter().enumerate() {
        let canonical_field = &tuple_schema.fields[idx];
        let layout_field = &layout.fields[idx];
        debug_assert_eq!(layout_field.name, canonical_field.name);

        lower_dict_field_value(tuple_schema, idx, element, element.range, ctx)?;

        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty)?;
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple element {idx} produced no value"),
                    range: element.range,
                }
            )
        })?;
        // Host-visible field-store boundary: compare the full IrType,
        // not the collapsed wasm slot. The host decodes the slot per the
        // schema's declared type, so a String / List* / Dict mistagged
        // onto a differently-typed slot must cap here rather than slip
        // through on a shared i32 slot.
        if top != store_ty {
            return Err(cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple element {idx}: got {top:?}, expected {store_ty:?}"),
                    range: element.range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: element.range,
        });
    }
    Ok(())
}

/// Lower one user-supplied dict-literal field value. Field `idx`
/// describes the schema-side canonical type; the value's source-side
/// expression decides which lowering arm to take.
pub(super) fn lower_dict_field_value(
    schema: &Schema,
    field_idx: usize,
    value: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let canonical = &schema.fields[field_idx];
    match (&canonical.ty, &*value.expr) {
        (TypeRepr::Schema { schema: sub_schema }, Expr::Dict(pairs)) if !sub_schema.is_tuple => {
            // Nested branded dict. Allocate a sub-record, recurse,
            // then push the sub-record's base offset for the parent's
            // pointer slot.
            let sub_layout = SchemaLayout::offsets_for(sub_schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    sub_layout.root_size as u32,
                    sub_layout.root_align as u32,
                ),
                range,
            });
            lower_dict_into_record(sub_schema, &sub_layout, pairs, range, record_local, ctx)?;
            // Store pointer slots use arena-absolute offsets. The record-local
            // itself is out-buffer-relative, so rebase it before the parent
            // field store consumes it.
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        (TypeRepr::Schema { schema: sub_schema }, Expr::Tuple(elements)) if sub_schema.is_tuple => {
            if elements.len() != sub_schema.fields.len() {
                return Err(cap!(
                    "lower_tuple_field.arity_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "tuple field has {} elements but schema declares {}",
                            elements.len(),
                            sub_schema.fields.len()
                        ),
                        range,
                    }
                ));
            }
            let sub_layout = SchemaLayout::offsets_for(sub_schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    sub_layout.root_size as u32,
                    sub_layout.root_align as u32,
                ),
                range,
            });
            lower_tuple_into_record(sub_schema, &sub_layout, elements, record_local, ctx)?;
            // Store pointer slots use arena-absolute offsets. The record-local
            // itself is out-buffer-relative, so rebase it before the parent
            // field store consumes it.
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::VariantCtor { .. },
        )
        | (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::Variable(_),
        )
        | (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::FnCall { .. },
        ) => lower_value_as_type(&canonical.ty, value, ctx),
        (TypeRepr::String, _) | (TypeRepr::List { .. }, _) => {
            // F3: cross-region branded-struct field. When the field is a
            // `List<…>` and its value is a bare `#main` parameter identity
            // whose data lives in the input region (the object head sits in
            // the output region), store the parameter list root's
            // arena-absolute offset directly into the field slot — exactly
            // like the anon-Dict `CrossRegionParamList` path (F1b/F2). The
            // value `lower_expr` pushes over a `Variable(param)` is the
            // `LoadList*Ptr` arena-absolute offset (F1 slot convention); we
            // store it verbatim with NO tail copy (the copy would lose the
            // cross-region link and, for pointer-array lists, mis-relocate
            // the in-buffer offsets). The host's object positive-path
            // verifier (`verify_object_return_multi`) classifies the offset
            // into the input region, bounds-checks the whole reachable
            // graph, and only then does the `BufferReader` field reader
            // follow it cross-region — bit-equal to the tree-walk oracle.
            if let TypeRepr::List { .. } = &canonical.ty {
                if branded_field_cross_region_param_list(&canonical.ty, value, ctx) {
                    lower_expr(&value.expr, range, ctx)?;
                    let popped = ctx.tstack.pop().ok_or(cap!(
                        "lower_dict_field_value.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: "Dict(cross-region-field-value-stack-empty)".to_string(),
                            range,
                        }
                    ))?;
                    let expected_ir = type_repr_to_ir_type_dict(&canonical.ty)?;
                    if popped != expected_ir {
                        return Err(cap!(
                            "lower_dict_field_value.unsupported_field_type.1",
                            LoweringError::UnsupportedFieldType {
                                schema: schema.name.clone(),
                                field: canonical.name.clone(),
                                ty: format!(
                                    "cross-region field expected {expected_ir:?}, got {popped:?}"
                                ),
                                range,
                            }
                        ));
                    }
                    // The slot stores the arena-absolute offset directly.
                    // The caller's `StoreFieldAtRecord` writes an i32
                    // pointer-indirect slot; push the i32 offset for it.
                    ctx.tstack.push(IrType::I32);
                    return Ok(());
                }
            }
            if variant_list_literal_for_type(&canonical.ty, &value.expr) {
                lower_value_as_type(&canonical.ty, value, ctx)?;
                let popped = ctx.tstack.pop().ok_or(cap!(
                    "lower_dict_field_value.unsupported_expr.variant_list_stack_empty",
                    LoweringError::UnsupportedExpr {
                        kind: "Dict(variant-list-field-value-stack-empty)".to_string(),
                        range,
                    }
                ))?;
                if popped != IrType::ListList {
                    return Err(cap!(
                        "lower_dict_field_value.unsupported_field_type.variant_list_stack",
                        LoweringError::UnsupportedFieldType {
                            schema: schema.name.clone(),
                            field: canonical.name.clone(),
                            ty: format!("variant list field expected ListList, got {popped:?}"),
                            range,
                        }
                    ));
                }
                ctx.tstack.push(IrType::I32);
                return Ok(());
            }

            // Pointer-array list fields (`List<String>` / `List<Schema>`
            // / `List<List<_>>`) inside a branded-struct return are only
            // marshalled correctly from a const-pool `ConstListString`
            // block. A value sourced from a `#main` parameter / load /
            // call lives in the input buffer with non-contiguous,
            // whole-buffer-relative offsets the rigid-delta tail copy
            // (`EmitTailRecordFromAbsoluteAddr`) cannot relocate — it
            // would segfault / corrupt. Reject loudly before lowering so
            // the silent path is unreachable. (`List<Int/Float/Bool>` is
            // inline-fixed and copies correctly from any source, so the
            // pointer-*array* check excludes it.)
            if let TypeRepr::List { element } = &canonical.ty {
                let field_ir = match element.as_ref() {
                    TypeRepr::String => IrType::ListString,
                    TypeRepr::Schema { .. } => IrType::ListSchema,
                    TypeRepr::List { .. }
                    | TypeRepr::Option { .. }
                    | TypeRepr::Result { .. }
                    | TypeRepr::Enum { .. } => IrType::ListList,
                    _ => IrType::ListInt,
                };
                if pointer_array_list_ir_type(field_ir)
                    && !pointer_array_list_source_is_const_pool(&value.expr)
                {
                    return Err(cap!(
                        "lower_dict_field_value.unsupported_field_type.2",
                        LoweringError::UnsupportedFieldType {
                            schema: schema.name.clone(),
                            field: canonical.name.clone(),
                            ty: format!(
                                "{:?} sourced from `{}` — pointer-array list fields are only \
                             marshalled from in-source list literals, not parameters / loads / \
                             calls",
                                canonical.ty,
                                value.expr.kind()
                            ),
                            range,
                        }
                    ));
                }
            }
            // Recursively lower the value to produce an absolute
            // pointer (ConstString / ConstListInt / LoadStringPtr /
            // ...). Then copy the record into the parent's tail
            // area and push the buffer-relative offset.
            lower_expr(&value.expr, range, ctx)?;
            // Top of stack is an absolute address. Emit the tail-
            // record memcpy.
            let popped = ctx.tstack.pop().ok_or(cap!(
                "lower_dict_field_value.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "Dict(field-value-stack-empty)".to_string(),
                    range,
                }
            ))?;
            // Cross-check the IR type against the declared field
            // type — saves a confusing codegen-time failure when the
            // dict field expects a String but the value lowered to
            // List<Int>.
            let expected_ir = match &canonical.ty {
                TypeRepr::String => IrType::String,
                TypeRepr::List { element } => match element.as_ref() {
                    TypeRepr::Int => IrType::ListInt,
                    TypeRepr::Float => IrType::ListFloat,
                    TypeRepr::Bool => IrType::ListBool,
                    TypeRepr::String => IrType::ListString,
                    TypeRepr::Schema { .. } => IrType::ListSchema,
                    TypeRepr::List { .. }
                    | TypeRepr::Option { .. }
                    | TypeRepr::Result { .. }
                    | TypeRepr::Enum { .. } => IrType::ListList,
                    _ => IrType::ListInt,
                },
                _ => unreachable!(),
            };
            if popped != expected_ir {
                return Err(cap!(
                    "lower_dict_field_value.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: schema.name.clone(),
                        field: canonical.name.clone(),
                        ty: format!("expected {expected_ir:?}, got {popped:?}"),
                        range,
                    }
                ));
            }
            if !ctx.variant_records_in_scratch {
                ctx.out.push(TaggedOp {
                    op: Op::EmitTailRecordFromAbsoluteAddr { ty: expected_ir },
                    range,
                });
            }
            ctx.tstack.push(IrType::I32);
            Ok(())
        }
        // Scalar leaves: just lower the value. The
        // StoreFieldAtRecord ranges already align.
        _ => lower_expr(&value.expr, range, ctx),
    }
}

/// Lower a schema-default expression for field `field_idx`. The
/// default's body lives on the analyzer-side `SchemaFieldDef::value_node`;
/// we re-route the existing `lower_expr` recursion at that body so
/// references to sibling fields hit the just-pushed let-bindings
/// (we cache each evaluated field into a let-local in
/// [`lower_dict_into_record`]).
pub(super) fn lower_dict_default(
    schema_name: &str,
    field_idx: usize,
    expected_ty: &TypeRepr,
    def: &SchemaDef,
    ctx: &mut LowerCtx<'_>,
    range: TokenRange,
) -> Result<(), LoweringError> {
    let field = &def.fields[field_idx];
    if field.is_wildcard {
        return Err(cap!(
            "lower_dict_default.missing_field_no_default",
            LoweringError::MissingFieldNoDefault {
                schema: schema_name.to_string(),
                field: field.name.clone(),
                range,
            }
        ));
    }
    // Lower the default expression with the surrounding lets in
    // scope. The let-stack already carries `<prior-field-name> →
    // value` bindings because the topological order placed
    // dependencies first.
    let value_node = &field.value_node;
    if matches!(
        expected_ty,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }
    ) {
        lower_value_as_type(expected_ty, value_node, ctx)?;
    } else {
        lower_expr(&value_node.expr, value_node.range, ctx)?;
    }
    Ok(())
}
