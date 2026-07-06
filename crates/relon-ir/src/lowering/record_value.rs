//! Lowering sub-module: record layout math and typed value → record
//! emission, including enum variant records.
//!
//! Owns the alignment / payload-slot layout helpers
//! (`*_for_lowering`), the record alloc / field-store op builders,
//! `lower_value_as_type` (the typed-slot entry point), and the whole
//! variant-record family: shape recognition (`standard_variant_shape`),
//! variant ctor / call lowering, and `BuildVariantRecord` emission.

use super::*;

pub(super) fn align_up_for_lowering(value: usize, align: usize) -> usize {
    if align <= 1 {
        value
    } else {
        (value + (align - 1)) & !(align - 1)
    }
}

pub(super) fn payload_slot_layout_for_lowering(
    ty: &TypeRepr,
) -> Result<(usize, usize), LoweringError> {
    match ty {
        TypeRepr::Bool | TypeRepr::Unit => Ok((1, 1)),
        TypeRepr::Int | TypeRepr::Float => Ok((8, 8)),
        TypeRepr::String
        | TypeRepr::List { .. }
        | TypeRepr::Schema { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => Ok((4, 4)),
        other => Err(cap!(
            "payload_slot_layout_for_lowering.unsupported_type",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{other:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}

pub(super) fn type_graph_alignment_for_lowering(ty: &TypeRepr) -> Result<usize, LoweringError> {
    match ty {
        TypeRepr::Bool | TypeRepr::Unit => Ok(1),
        TypeRepr::Int | TypeRepr::Float => Ok(8),
        TypeRepr::String | TypeRepr::List { .. } | TypeRepr::Schema { .. } => Ok(4),
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            variant_record_alignment_for_lowering(ty)
        }
        other => Err(cap!(
            "type_graph_alignment_for_lowering.unsupported_type",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{other:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}

pub(super) fn variant_record_alignment_for_lowering(ty: &TypeRepr) -> Result<usize, LoweringError> {
    let payloads: Vec<TypeRepr> = match ty {
        TypeRepr::Option { inner } => vec![inner.as_ref().clone()],
        TypeRepr::Result { ok, err } => vec![ok.as_ref().clone(), err.as_ref().clone()],
        TypeRepr::Enum { name, variants } => variants
            .iter()
            .filter_map(|variant| {
                variant.payload_schema(name).map(|schema| TypeRepr::Schema {
                    schema: Box::new(schema),
                })
            })
            .collect(),
        other => {
            return Err(cap!(
                "variant_record_alignment_for_lowering.unsupported_type",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!("{other:?}"),
                    range: TokenRange::default(),
                }
            ))
        }
    };
    let mut align = 4usize;
    for payload in &payloads {
        let (_, slot_align) = payload_slot_layout_for_lowering(payload)?;
        align = align
            .max(slot_align)
            .max(type_graph_alignment_for_lowering(payload)?);
    }
    Ok(align)
}

pub(super) fn variant_payload_offset_for_lowering(
    payload_ty: &TypeRepr,
) -> Result<usize, LoweringError> {
    let (_, payload_align) = payload_slot_layout_for_lowering(payload_ty)?;
    Ok(align_up_for_lowering(1, payload_align))
}

pub(super) fn variant_body_pairs(
    body: &Node,
    range: TokenRange,
) -> Result<&[(TokenKey, Node)], LoweringError> {
    match &*body.expr {
        Expr::Dict(pairs) => Ok(pairs.as_slice()),
        other => Err(cap!(
            "lower_variant_ctor_as_type.body_not_dict",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "variant constructor body must be a dict literal, got `{}`",
                    other.kind()
                ),
                range,
            }
        )),
    }
}

pub(super) fn variant_payload_node<'a>(
    pairs: &'a [(TokenKey, Node)],
    key_name: &str,
    range: TokenRange,
) -> Result<&'a Node, LoweringError> {
    let mut found: Option<&Node> = None;
    for (key, value) in pairs {
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "lower_variant_ctor_as_type.non_string_key",
                LoweringError::UnsupportedExpr {
                    kind: "variant constructor field key must be a string identifier".to_string(),
                    range,
                }
            ));
        };
        if name == key_name {
            if found.is_some() {
                return Err(cap!(
                    "lower_variant_ctor_as_type.duplicate_payload",
                    LoweringError::UnsupportedExpr {
                        kind: format!("duplicate variant payload field `{key_name}`"),
                        range,
                    }
                ));
            }
            found = Some(value);
        } else {
            return Err(cap!(
                "lower_variant_ctor_as_type.unexpected_field",
                LoweringError::UnsupportedExpr {
                    kind: format!("unexpected variant payload field `{name}`"),
                    range,
                }
            ));
        }
    }
    found.ok_or_else(|| {
        cap!(
            "lower_variant_ctor_as_type.missing_payload",
            LoweringError::UnsupportedExpr {
                kind: format!("missing variant payload field `{key_name}`"),
                range,
            }
        )
    })
}

pub(super) fn lower_schema_value_as_absolute_pointer(
    schema: &Schema,
    value: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match (&*value.expr, schema.is_tuple) {
        (Expr::Dict(pairs), false) => {
            let layout = SchemaLayout::offsets_for(schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    layout.root_size as u32,
                    layout.root_align as u32,
                ),
                range,
            });
            if ctx.schema_resolver.resolve(&schema.name).is_some() {
                lower_dict_into_record(schema, &layout, pairs, range, record_local, ctx)?;
            } else {
                lower_plain_dict_into_record(schema, &layout, pairs, range, record_local, ctx)?;
            }
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        (Expr::Tuple(elements), true) => {
            if elements.len() != schema.fields.len() {
                return Err(cap!(
                    "lower_schema_value_as_absolute_pointer.arity_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "tuple payload has {} elements but schema declares {}",
                            elements.len(),
                            schema.fields.len()
                        ),
                        range,
                    }
                ));
            }
            let layout = SchemaLayout::offsets_for(schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    layout.root_size as u32,
                    layout.root_align as u32,
                ),
                range,
            });
            lower_tuple_into_record(schema, &layout, elements, record_local, ctx)?;
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        _ => lower_expr(&value.expr, range, ctx),
    }
}

pub(super) fn alloc_record_op(
    ctx: &LowerCtx<'_>,
    record_local: u32,
    root_size: u32,
    root_align: u32,
) -> Op {
    if ctx.variant_records_in_scratch {
        Op::AllocScratchRecord {
            record_local_idx: record_local,
            root_size,
            root_align,
        }
    } else {
        Op::AllocSubRecord {
            record_local_idx: record_local,
            root_size,
            root_align,
        }
    }
}

pub(super) fn store_field_at_record_op(
    ctx: &LowerCtx<'_>,
    record_local: u32,
    offset: u32,
    ty: IrType,
) -> Op {
    if ctx.variant_records_in_scratch {
        Op::StoreFieldAtRecordAbsolute {
            record_local_idx: record_local,
            offset,
            ty,
        }
    } else {
        Op::StoreFieldAtRecord {
            record_local_idx: record_local,
            offset,
            ty,
        }
    }
}

pub(super) fn push_record_base_for_pointer(
    record_local: u32,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) {
    if ctx.variant_records_in_scratch {
        ctx.out.push(TaggedOp {
            op: Op::PushRecordBaseAbsolute {
                record_local_idx: record_local,
            },
            range,
        });
        ctx.tstack.push(IrType::I32);
    } else {
        push_record_base_as_absolute(record_local, range, ctx);
    }
}

pub(super) fn push_record_base_as_absolute(
    record_local: u32,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) {
    ctx.out.push(TaggedOp {
        op: Op::PushRecordBase {
            record_local_idx: record_local,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LocalGet(2),
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
}

pub(super) fn lower_value_as_type(
    expected: &TypeRepr,
    value: &Node,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match (expected, &*value.expr) {
        (expected, Expr::Ternary { cond, then, els }) => {
            lower_ternary_as_type(expected, cond, then, els, value.range, ctx)
        }
        (TypeRepr::List { element }, Expr::FnCall { path, args })
            if variant_record_list_element(element) =>
        {
            if let Some(()) = peephole::emit_variant_list_filter_call_as_type(
                element,
                path,
                args,
                value.range,
                ctx,
            )? {
                Ok(())
            } else if let Some(()) = peephole::emit_variant_list_filter_method_as_type(
                element,
                path,
                args,
                value.range,
                ctx,
            )? {
                Ok(())
            } else if let Some(()) =
                peephole::emit_variant_list_map_call_as_type(element, path, args, value.range, ctx)?
            {
                Ok(())
            } else if let Some(()) = peephole::emit_variant_list_map_method_as_type(
                element,
                path,
                args,
                value.range,
                ctx,
            )? {
                Ok(())
            } else {
                lower_expr(&value.expr, value.range, ctx)
            }
        }
        (
            TypeRepr::List {
                element: expected_element,
            },
            Expr::Comprehension {
                element,
                id,
                iterable,
                condition,
            },
        ) if variant_record_list_element(expected_element) => lower_comprehension_as_type(
            expected_element,
            element,
            id,
            iterable,
            condition.as_ref(),
            value.range,
            ctx,
        ),
        (TypeRepr::List { element }, Expr::List(items)) if variant_record_list_element(element) => {
            lower_variant_record_list_literal(element, items, value.range, ctx)
        }
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::VariantCtor {
                enum_path,
                variant,
                body,
            },
        ) => lower_variant_ctor_as_type(expected, enum_path, variant, body, value.range, ctx),
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::Variable(path),
        ) => {
            if let Some(variant) = variant_name_from_path(expected, path, false) {
                lower_standard_variant_record(expected, variant.as_str(), None, value.range, ctx)
            } else {
                lower_expr(&value.expr, value.range, ctx)
            }
        }
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::FnCall { path, args },
        ) => {
            if let Some(variant) = variant_name_from_path(expected, path, true) {
                lower_variant_call_as_type(expected, variant.as_str(), args, value.range, ctx)
            } else {
                lower_expr(&value.expr, value.range, ctx)
            }
        }
        (TypeRepr::Schema { schema }, _) => {
            lower_schema_value_as_absolute_pointer(schema, value, value.range, ctx)
        }
        _ => lower_expr(&value.expr, value.range, ctx),
    }
}

pub(super) fn variant_record_list_element(element: &TypeRepr) -> bool {
    matches!(
        element,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }
    )
}

pub(super) fn variant_list_literal_for_type(expected: &TypeRepr, expr: &Expr) -> bool {
    matches!(expected, TypeRepr::List { element } if variant_record_list_element(element))
        && matches!(expr, Expr::List(_))
}

pub(super) fn variant_record_list_inplace_expr_for_type(expected: &TypeRepr, expr: &Expr) -> bool {
    if !matches!(expected, TypeRepr::List { element } if variant_record_list_element(element)) {
        return false;
    }
    match expr {
        Expr::List(_) | Expr::Comprehension { .. } => true,
        Expr::FnCall { path, .. } => {
            if let [TokenKey::String(name, _, _), ..] = path.as_slice() {
                if name == "_list_map" || name == "_list_filter" {
                    return true;
                }
            }
            matches!(path.last(), Some(TokenKey::String(name, _, _)) if name == "map" || name == "filter")
        }
        _ => false,
    }
}

pub(super) fn lower_variant_record_list_literal(
    element: &TypeRepr,
    items: &[Node],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    for item in items {
        lower_value_as_type(element, item, ctx)?;
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_variant_record_list_literal.empty_element_stack",
                LoweringError::UnsupportedExpr {
                    kind: "List<Enum>(element produced no value)".to_string(),
                    range: item.range,
                }
            )
        })?;
        if top.wasm_slot() != IrType::I32.wasm_slot() {
            return Err(cap!(
                "lower_variant_record_list_literal.element_type_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!("List<Enum>(element produced {top:?}, expected variant pointer)"),
                    range: item.range,
                }
            ));
        }
    }
    let len = u32::try_from(items.len()).map_err(|_| {
        cap!(
            "lower_variant_record_list_literal.length_overflow",
            LoweringError::UnsupportedExpr {
                kind: "List<Enum>(too many elements)".to_string(),
                range,
            }
        )
    })?;
    ctx.out.push(TaggedOp {
        op: Op::BuildPointerList { len },
        range,
    });
    ctx.tstack.push(IrType::ListList);
    Ok(())
}

#[derive(Debug, Clone)]
pub(super) struct VariantPayloadShape {
    ty: TypeRepr,
    key: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub(super) struct VariantShape {
    tag: u8,
    payload: Option<VariantPayloadShape>,
}

pub(super) fn path_strings(path: &[TokenKey]) -> Option<Vec<&str>> {
    path.iter()
        .map(|key| match key {
            TokenKey::String(name, _, _) => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn enum_path_matches(expected_name: &str, enum_path: Option<&[String]>) -> bool {
    match enum_path {
        Some(path) => path.is_empty() || path == [expected_name],
        None => true,
    }
}

pub(super) fn variant_name_from_path(
    expected: &TypeRepr,
    path: &[TokenKey],
    require_payload: bool,
) -> Option<String> {
    let parts = path_strings(path)?;
    match expected {
        TypeRepr::Option { .. } => {
            let name = match parts.as_slice() {
                ["None"] if !require_payload => "None",
                ["Option", "None"] if !require_payload => "None",
                ["Some"] if require_payload => "Some",
                ["Option", "Some"] if require_payload => "Some",
                _ => return None,
            };
            Some(name.to_string())
        }
        TypeRepr::Result { .. } => {
            let name = match parts.as_slice() {
                ["Ok"] if require_payload => "Ok",
                ["Result", "Ok"] if require_payload => "Ok",
                ["Err"] if require_payload => "Err",
                ["Result", "Err"] if require_payload => "Err",
                _ => return None,
            };
            Some(name.to_string())
        }
        TypeRepr::Enum { name, variants } => {
            let variant_name = match parts.as_slice() {
                [variant] => *variant,
                [enum_name, variant] if enum_name == name => *variant,
                _ => return None,
            };
            let variant = variants.iter().find(|v| v.name == variant_name)?;
            if require_payload != variant.fields.is_empty() {
                Some(variant.name.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(super) fn standard_variant_shape(
    expected: &TypeRepr,
    enum_path: Option<&[String]>,
    variant: &str,
    range: TokenRange,
) -> Result<VariantShape, LoweringError> {
    match expected {
        TypeRepr::Option { inner } => {
            if !enum_path_matches("Option", enum_path) {
                return Err(cap!(
                    "standard_variant_shape.option_enum_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "expected Option variant, got {}.{variant}",
                            enum_path.map(|p| p.join(".")).unwrap_or_default()
                        ),
                        range,
                    }
                ));
            }
            match variant {
                "None" => Ok(VariantShape {
                    tag: 0,
                    payload: None,
                }),
                "Some" => Ok(VariantShape {
                    tag: 1,
                    payload: Some(VariantPayloadShape {
                        ty: inner.as_ref().clone(),
                        key: Some("value"),
                    }),
                }),
                other => Err(cap!(
                    "standard_variant_shape.option_variant_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!("unknown Option variant `{other}`"),
                        range,
                    }
                )),
            }
        }
        TypeRepr::Result { ok, err } => {
            if !enum_path_matches("Result", enum_path) {
                return Err(cap!(
                    "standard_variant_shape.result_enum_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "expected Result variant, got {}.{variant}",
                            enum_path.map(|p| p.join(".")).unwrap_or_default()
                        ),
                        range,
                    }
                ));
            }
            match variant {
                "Ok" => Ok(VariantShape {
                    tag: 0,
                    payload: Some(VariantPayloadShape {
                        ty: ok.as_ref().clone(),
                        key: Some("value"),
                    }),
                }),
                "Err" => Ok(VariantShape {
                    tag: 1,
                    payload: Some(VariantPayloadShape {
                        ty: err.as_ref().clone(),
                        key: Some("error"),
                    }),
                }),
                other => Err(cap!(
                    "standard_variant_shape.result_variant_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!("unknown Result variant `{other}`"),
                        range,
                    }
                )),
            }
        }
        TypeRepr::Enum { name, variants } => {
            if !enum_path_matches(name, enum_path) {
                return Err(cap!(
                    "standard_variant_shape.not_variant_type",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "expected {name} variant, got {}.{variant}",
                            enum_path.map(|p| p.join(".")).unwrap_or_default()
                        ),
                        range,
                    }
                ));
            }
            let Some(v) = variants.iter().find(|v| v.name == variant) else {
                return Err(cap!(
                    "standard_variant_shape.not_variant_type",
                    LoweringError::UnsupportedExpr {
                        kind: format!("unknown {name} variant `{variant}`"),
                        range,
                    }
                ));
            };
            Ok(VariantShape {
                tag: v.tag,
                payload: v.payload_schema(name).map(|schema| VariantPayloadShape {
                    ty: TypeRepr::Schema {
                        schema: Box::new(schema),
                    },
                    key: None,
                }),
            })
        }
        other => Err(cap!(
            "standard_variant_shape.not_variant_type",
            LoweringError::UnsupportedExpr {
                kind: format!("variant constructor needs enum target, got `{other:?}`"),
                range,
            }
        )),
    }
}

pub(super) fn lower_variant_call_as_type(
    expected: &TypeRepr,
    variant: &str,
    args: &[CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let shape = standard_variant_shape(expected, None, variant, range)?;
    let Some(payload) = shape.payload.as_ref() else {
        if args.is_empty() {
            return emit_standard_variant_record(
                expected, variant, shape.tag, None, None, range, ctx,
            );
        }
        return Err(cap!(
            "lower_prelude_variant_call_as_type.arity_mismatch",
            LoweringError::UnsupportedExpr {
                kind: format!("{variant}(...) does not take payload"),
                range,
            }
        ));
    };

    if payload.key.is_some() {
        if args.len() != 1 || args[0].name.is_some() {
            return Err(cap!(
                "lower_prelude_variant_call_as_type.arity_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!("{variant}(...) expects exactly one positional payload"),
                    range,
                }
            ));
        }
        return emit_standard_variant_record(
            expected,
            variant,
            shape.tag,
            Some(&payload.ty),
            Some(&args[0].value),
            range,
            ctx,
        );
    }

    let TypeRepr::Schema { schema } = &payload.ty else {
        return Err(cap!(
            "standard_variant_shape.not_variant_type",
            LoweringError::UnsupportedExpr {
                kind: format!("variant `{variant}` payload is not a record"),
                range,
            }
        ));
    };
    if !fields_are_tuple_payload(&schema.fields) {
        return Err(cap!(
            "lower_prelude_variant_call_as_type.arity_mismatch",
            LoweringError::UnsupportedExpr {
                kind: format!("struct variant `{variant}` must be constructed with `{{ ... }}`"),
                range,
            }
        ));
    }
    if args.len() != schema.fields.len() || args.iter().any(|arg| arg.name.is_some()) {
        return Err(cap!(
            "lower_prelude_variant_call_as_type.arity_mismatch",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "{variant}(...) expects {} positional payload values",
                    schema.fields.len()
                ),
                range,
            }
        ));
    }
    let layout = SchemaLayout::offsets_for(schema)?;
    let record_local = ctx.alloc_record_local();
    ctx.out.push(TaggedOp {
        op: alloc_record_op(
            ctx,
            record_local,
            layout.root_size as u32,
            layout.root_align as u32,
        ),
        range,
    });
    for (idx, arg) in args.iter().enumerate() {
        lower_dict_field_value(schema, idx, &arg.value, arg.value.range, ctx)?;
        let canonical_field = &schema.fields[idx];
        let layout_field = &layout.fields[idx];
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty)?;
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple variant field {idx} produced no value"),
                    range: arg.value.range,
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
                    kind: format!("tuple variant field {idx}: got {top:?}, expected {store_ty:?}"),
                    range: arg.value.range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: arg.value.range,
        });
    }
    push_record_base_for_pointer(record_local, range, ctx);
    emit_variant_record_from_lowered_payload(
        expected,
        variant,
        shape.tag,
        Some(&payload.ty),
        range,
        ctx,
    )
}

pub(super) fn lower_variant_ctor_as_type(
    expected: &TypeRepr,
    enum_path: &[String],
    variant: &str,
    body: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let shape = standard_variant_shape(expected, Some(enum_path), variant, range)?;
    let pairs = variant_body_pairs(body, range)?;
    let payload_node = if let Some(payload) = shape.payload.as_ref() {
        if let Some(key_name) = payload.key {
            Some(variant_payload_node(pairs, key_name, range)?)
        } else {
            Some(body)
        }
    } else {
        if !pairs.is_empty() {
            return Err(cap!(
                "lower_variant_ctor_as_type.unit_variant_has_fields",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` does not take payload fields"),
                    range,
                }
            ));
        }
        None
    };
    emit_standard_variant_record(
        expected,
        variant,
        shape.tag,
        shape.payload.as_ref().map(|p| &p.ty),
        payload_node,
        range,
        ctx,
    )
}

pub(super) fn lower_standard_variant_record(
    expected: &TypeRepr,
    variant: &str,
    payload_node: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let shape = standard_variant_shape(expected, None, variant, range)?;
    emit_standard_variant_record(
        expected,
        variant,
        shape.tag,
        shape.payload.as_ref().map(|p| &p.ty),
        payload_node,
        range,
        ctx,
    )
}

pub(super) fn emit_standard_variant_record(
    expected: &TypeRepr,
    variant: &str,
    tag: u8,
    payload_ty: Option<&TypeRepr>,
    payload_node: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if let Some(payload_ty) = payload_ty {
        let Some(payload_node) = payload_node else {
            return Err(cap!(
                "emit_standard_variant_record.missing_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` requires a payload"),
                    range,
                }
            ));
        };
        lower_value_as_type(payload_ty, payload_node, ctx)?;
        emit_variant_record_from_lowered_payload(
            expected,
            variant,
            tag,
            Some(payload_ty),
            range,
            ctx,
        )
    } else {
        if payload_node.is_some() {
            return Err(cap!(
                "emit_standard_variant_record.unexpected_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` does not take a payload"),
                    range,
                }
            ));
        }
        emit_variant_record_from_lowered_payload(expected, variant, tag, None, range, ctx)
    }
}

pub(super) fn emit_variant_record_from_lowered_payload(
    expected: &TypeRepr,
    variant: &str,
    tag: u8,
    payload_ty: Option<&TypeRepr>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let record_align = variant_record_alignment_for_lowering(expected)?;
    let (payload_offset, payload_ir_ty, record_size) = if let Some(payload_ty) = payload_ty {
        let expected_ir = type_repr_to_ir_type_dict(payload_ty)?;
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "emit_standard_variant_record.empty_payload_stack",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` payload produced no value"),
                    range,
                }
            )
        })?;
        if top.wasm_slot() != expected_ir.wasm_slot() {
            return Err(cap!(
                "emit_standard_variant_record.payload_type_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "variant `{variant}` payload produced {top:?}, expected {expected_ir:?}"
                    ),
                    range,
                }
            ));
        }
        let (payload_size, _) = payload_slot_layout_for_lowering(payload_ty)?;
        let offset = variant_payload_offset_for_lowering(payload_ty)?;
        (
            Some(offset as u32),
            Some(expected_ir),
            (offset + payload_size) as u32,
        )
    } else {
        (None, None, 1)
    };

    let op = if ctx.variant_records_in_scratch {
        Op::BuildVariantRecordScratch {
            tag,
            record_size,
            record_align: record_align as u32,
            payload_offset,
            payload_ty: payload_ir_ty,
        }
    } else {
        Op::BuildVariantRecord {
            tag,
            record_size,
            record_align: record_align as u32,
            payload_offset,
            payload_ty: payload_ir_ty,
        }
    };
    ctx.out.push(TaggedOp { op, range });
    ctx.tstack.push(IrType::I32);
    Ok(())
}
