//! Lowering sub-module: `match` lowering and enum pattern machinery.
//!
//! Owns the static arm decision (compile-time arm selection against a
//! statically-known scrutinee), the runtime enum match chain
//! (tag-test `If` ladder + payload pattern bindings + no-match trap),
//! enum-variant narrowing inside branches, and `lower_match` itself.

use super::*;

/// Static result of testing one match arm's pattern against the
/// scrutinee's statically-known shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StaticArmDecision {
    /// The scrutinee's static type provably satisfies this arm's
    /// pattern — `check_type` / brand-equality would return `Ok` for
    /// EVERY runtime value of this static type. Select this arm.
    Matches,
    /// The pattern provably never matches a value of this static type —
    /// `check_type` would always fail (or the eval brand-shortcut would
    /// `continue`). Skip this arm.
    Never,
    /// The static type does NOT pin the arm decision (a coarsening
    /// builtin like `Number` / `List` / `Dict`, a generic pattern, a
    /// dotted/optional pattern, or a builtin pattern against a branded
    /// dict). Keep the whole `match` capped and defer — never force.
    Undecidable,
}

/// Decide, purely statically, whether a value whose IR type is `ty`
/// (carrying optional schema `brand`) satisfies the arm `pattern`.
///
/// This MUST agree with the tree-walk `Expr::Match` arm semantics in
/// `relon-evaluator`'s `eval.rs` (the `check_type` / brand-equality
/// path) for the static type in question:
///
/// * `Wildcard` always matches.
/// * `Type(tn)` with a single-segment, non-generic, non-optional path:
///   - branded scrutinee (`brand == Some(b)`, i.e. a branded `Dict`):
///     `name == b` ⇒ match; a non-builtin `name != b` ⇒ the eval
///     brand-shortcut `continue`s (never matches); a builtin `name`
///     falls through to `check_type` against a branded dict, which the
///     static layer does not model ⇒ `Undecidable`.
///   - unbranded scrutinee: a builtin `name` matches iff it is the
///     EXACT scalar tag for `ty` (no coarsening / multi-type builtin);
///     a non-builtin (schema) `name` against a definite scalar value
///     can never satisfy `apply_schema` (which requires a `Dict`) ⇒
///     `Never`.
///   - any optional / dotted / generic pattern ⇒ `Undecidable`.
pub(super) fn static_arm_decision(
    ty: IrType,
    brand: Option<&str>,
    pattern: &Expr,
) -> StaticArmDecision {
    match pattern {
        Expr::Wildcard => StaticArmDecision::Matches,
        Expr::Type(tn) => static_type_pattern_decision(ty, brand, tn),
        // Any other pattern shape (literal patterns, etc.) is not part of
        // the strict-mode static surface — defer.
        _ => StaticArmDecision::Undecidable,
    }
}

pub(super) fn static_type_pattern_decision(
    ty: IrType,
    brand: Option<&str>,
    tn: &TypeNode,
) -> StaticArmDecision {
    // Optional (`W?`), dotted (`geo.Location`), or generic (`List<Int>`)
    // patterns engage check_type branches the static layer does not
    // model — defer rather than guess.
    if tn.is_optional || tn.path.len() != 1 || !tn.generics.is_empty() {
        return StaticArmDecision::Undecidable;
    }
    let name = tn.path[0].as_str();

    if let Some(b) = brand {
        // Branded scrutinee (a branded `Dict`). Mirror the eval
        // brand-shortcut block exactly.
        if name == b {
            // `type_node.path.len() == 1 && path[0] == brand` ⇒ match.
            return StaticArmDecision::Matches;
        }
        if !is_builtin_type_name(name) {
            // Non-builtin brand mismatch ⇒ eval `continue`s (never).
            return StaticArmDecision::Never;
        }
        // Builtin pattern against a branded dict falls through to
        // check_type (e.g. `Dict` / `Any` matching a branded dict).
        // Not modelled statically — defer.
        return StaticArmDecision::Undecidable;
    }

    // Unbranded scrutinee.
    if is_builtin_type_name(name) {
        // Only the EXACT scalar tags are decided here. Coarsening /
        // multi-type builtins (`Any`, `Number`, `List`, `Dict`,
        // `Closure`, `Fn`, `Enum`, `Tuple`) are deferred.
        match (name, ty) {
            ("Int", IrType::I64) => StaticArmDecision::Matches,
            ("Float", IrType::F64) => StaticArmDecision::Matches,
            ("Bool", IrType::Bool) => StaticArmDecision::Matches,
            ("String", IrType::String) => StaticArmDecision::Matches,
            // A scalar builtin pattern naming a DIFFERENT scalar than the
            // (definite-scalar) static type can never match.
            ("Int" | "Float" | "Bool" | "String", t) if is_definite_scalar(t) => {
                StaticArmDecision::Never
            }
            _ => StaticArmDecision::Undecidable,
        }
    } else {
        // Non-builtin pattern (a schema/brand name) against an unbranded
        // value. The eval path runs `check_type`, whose `apply_schema`
        // requires a `Dict`; a definite scalar can never satisfy it, so
        // the arm provably never matches. For non-scalar shapes (lists /
        // plain dicts) defer to stay honest.
        if is_definite_scalar(ty) {
            StaticArmDecision::Never
        } else {
            StaticArmDecision::Undecidable
        }
    }
}

/// `true` when the IR type pins the runtime value to a single concrete
/// scalar Relon shape (`Int` / `Float` / `Bool` / `String`).
/// These are the only types for which a scalar / schema pattern decision
/// can be made with certainty.
pub(super) fn is_definite_scalar(ty: IrType) -> bool {
    matches!(
        ty,
        IrType::I64 | IrType::F64 | IrType::Bool | IrType::String | IrType::Unit
    )
}

/// One lowered source arm in a runtime `#enum` match.
pub(super) struct RuntimeEnumMatchArm {
    /// `Some(tag)` for a concrete variant arm, `None` for wildcard.
    tag: Option<u8>,
    body_ops: Vec<TaggedOp>,
    body_ty: IrType,
    range: TokenRange,
}

/// Build the body ops for a guaranteed no-match trap of result type
/// `result_ty`: an `Op::Trap { NoMatch }` followed by a typed placeholder
/// const. The trap makes the placeholder unreachable; it exists only so
/// the type stack / wasm verifier see a value of the right type (mirrors
/// the stdlib bounds-trap shape `Op::Trap` + typed const). Returns `None`
/// for a result type with no scalar placeholder const, so the caller caps
/// cleanly rather than miscompiling.
pub(super) fn no_match_trap_body_ops(
    result_ty: IrType,
    range: TokenRange,
    ctx: &LowerCtx<'_>,
) -> Option<Vec<TaggedOp>> {
    let placeholder = match result_ty {
        IrType::I64 => Op::ConstI64(0),
        IrType::I32 => Op::ConstI32(0),
        IrType::F64 => Op::ConstF64(OrderedFloat(0.0)),
        IrType::Bool => Op::ConstBool(false),
        IrType::String => {
            let idx = ctx.const_intern.borrow_mut().strings.intern("");
            Op::ConstString {
                idx,
                value: String::new(),
            }
        }
        _ => return None,
    };
    Some(vec![
        TaggedOp {
            op: Op::Trap {
                kind: TrapKind::NoMatch,
            },
            range,
        },
        TaggedOp {
            op: placeholder,
            range,
        },
    ])
}

pub(super) fn enum_scrutinee_binding(
    scrutinee: &Node,
    ctx: &LowerCtx<'_>,
) -> Option<(String, TypeRepr)> {
    let Expr::Variable(path) = &*scrutinee.expr else {
        return None;
    };
    if path.len() != 1 {
        return None;
    }
    let TokenKey::String(name, _, _) = &path[0] else {
        return None;
    };
    if let Some(binding) = ctx
        .params
        .iter()
        .find(|binding| binding.name == name.as_str())
    {
        return enum_like_type(&binding.type_repr)
            .then(|| (binding.name.clone(), binding.type_repr.clone()));
    }
    let binding = ctx
        .lets
        .iter()
        .rev()
        .find(|binding| binding.name == name.as_str())?;
    let type_repr = binding.type_repr.as_ref()?;
    enum_like_type(type_repr).then(|| (binding.name.clone(), type_repr.clone()))
}

pub(super) fn enum_like_type(ty: &TypeRepr) -> bool {
    matches!(
        ty,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }
    )
}

pub(super) fn enum_like_name(ty: &TypeRepr) -> Option<&str> {
    match ty {
        TypeRepr::Option { .. } => Some("Option"),
        TypeRepr::Result { .. } => Some("Result"),
        TypeRepr::Enum { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

pub(super) fn enum_like_tags(ty: &TypeRepr) -> Option<Vec<u8>> {
    match ty {
        TypeRepr::Option { .. } | TypeRepr::Result { .. } => Some(vec![0, 1]),
        TypeRepr::Enum { variants, .. } => {
            Some(variants.iter().map(|variant| variant.tag).collect())
        }
        _ => None,
    }
}

pub(super) fn type_pattern_variant_name(enum_ty: &TypeRepr, pattern: &TypeNode) -> Option<String> {
    if pattern.is_optional || !pattern.generics.is_empty() || pattern.variant_fields.is_some() {
        return None;
    }
    let parts: Vec<&str> = pattern.path.iter().map(String::as_str).collect();
    match enum_ty {
        TypeRepr::Option { .. } => match parts.as_slice() {
            ["None"] | ["Option", "None"] => Some("None".to_string()),
            ["Some"] | ["Option", "Some"] => Some("Some".to_string()),
            _ => None,
        },
        TypeRepr::Result { .. } => match parts.as_slice() {
            ["Ok"] | ["Result", "Ok"] => Some("Ok".to_string()),
            ["Err"] | ["Result", "Err"] => Some("Err".to_string()),
            _ => None,
        },
        TypeRepr::Enum { name, variants } => {
            let variant_name = match parts.as_slice() {
                [variant] => *variant,
                [enum_name, variant] if enum_name == name => *variant,
                _ => return None,
            };
            variants
                .iter()
                .find(|variant| variant.name == variant_name)
                .map(|variant| variant.name.clone())
        }
        _ => None,
    }
}

pub(super) fn matched_enum_variant(
    enum_ty: &TypeRepr,
    enum_path: Option<&[String]>,
    variant: &str,
    range: TokenRange,
) -> Option<EnumVariantNarrowing> {
    let enum_name = enum_like_name(enum_ty)?.to_string();
    match enum_ty {
        TypeRepr::Option { inner } => {
            if !enum_path_matches("Option", enum_path) {
                return None;
            }
            let (tag, fields, direct_payload) = match variant {
                "None" => (0, Vec::new(), None),
                "Some" => {
                    let field = Field {
                        name: "value".to_string(),
                        ty: inner.as_ref().clone(),
                        default: None,
                    };
                    let direct_payload = DirectEnumPayload {
                        field_name: field.name.clone(),
                        ty: field.ty.clone(),
                    };
                    (1, vec![field], Some(direct_payload))
                }
                _ => return None,
            };
            Some(EnumVariantNarrowing {
                enum_name,
                variant: CanonicalEnumVariant {
                    name: variant.to_string(),
                    tag,
                    fields,
                    is_tuple: false,
                },
                direct_payload,
            })
        }
        TypeRepr::Result { ok, err } => {
            if !enum_path_matches("Result", enum_path) {
                return None;
            }
            let (tag, field_name, ty) = match variant {
                "Ok" => (0, "value", ok.as_ref().clone()),
                "Err" => (1, "error", err.as_ref().clone()),
                _ => return None,
            };
            let field = Field {
                name: field_name.to_string(),
                ty,
                default: None,
            };
            let direct_payload = DirectEnumPayload {
                field_name: field.name.clone(),
                ty: field.ty.clone(),
            };
            Some(EnumVariantNarrowing {
                enum_name,
                variant: CanonicalEnumVariant {
                    name: variant.to_string(),
                    tag,
                    fields: vec![field],
                    is_tuple: false,
                },
                direct_payload: Some(direct_payload),
            })
        }
        TypeRepr::Enum { name, variants } => {
            if !enum_path_matches(name, enum_path) {
                return None;
            }
            let variant = variants
                .iter()
                .find(|candidate| candidate.name == variant)?;
            Some(EnumVariantNarrowing {
                enum_name,
                variant: variant.clone(),
                direct_payload: None,
            })
        }
        _ => {
            let _ = range;
            None
        }
    }
}

pub(super) fn enum_pattern_variant(
    enum_ty: &TypeRepr,
    pattern: &Expr,
    range: TokenRange,
) -> Option<(EnumVariantNarrowing, Vec<PatternBinding>)> {
    match pattern {
        Expr::Type(tn) => {
            let variant_name = type_pattern_variant_name(enum_ty, tn)?;
            matched_enum_variant(enum_ty, None, &variant_name, range).map(|n| (n, Vec::new()))
        }
        Expr::VariantPattern {
            enum_path,
            variant,
            bindings,
        } => matched_enum_variant(enum_ty, Some(enum_path), variant, range)
            .map(|n| (n, bindings.clone())),
        _ => None,
    }
}

pub(super) fn enum_payload_field_type(
    narrowing: &EnumVariantNarrowing,
    field_name: &str,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    if let Some(payload) = &narrowing.direct_payload {
        if field_name == payload.field_name {
            return Ok(payload.ty.clone());
        }
        return Err(cap!(
            "lower_match.enum_pattern_unknown_payload",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Enum(variant `{}` has no payload field `{field_name}`)",
                    narrowing.variant.name
                ),
                range,
            }
        ));
    }

    let payload_schema = narrowing
        .variant
        .payload_schema(&narrowing.enum_name)
        .ok_or_else(|| {
            cap!(
                "lower_match.enum_pattern_unit_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(unit variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })?;
    payload_schema
        .fields
        .iter()
        .find(|field| field.name == field_name)
        .map(|field| field.ty.clone())
        .ok_or_else(|| {
            cap!(
                "lower_match.enum_pattern_unknown_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })
}

pub(super) fn enum_pattern_binding_field_name(
    narrowing: &EnumVariantNarrowing,
    binding: &PatternBinding,
    idx: usize,
) -> String {
    if let Some(field) = binding.field.clone() {
        return field;
    }
    if let Some(payload) = &narrowing.direct_payload {
        return payload.field_name.clone();
    }
    idx.to_string()
}

pub(super) fn emit_enum_pattern_bindings(
    scrutinee_let_idx: u32,
    narrowing: &EnumVariantNarrowing,
    bindings: &[PatternBinding],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Vec<String>, LoweringError> {
    let mut added = Vec::new();
    let mut seen_bindings: HashSet<&str> = HashSet::new();
    for (idx, binding) in bindings.iter().enumerate() {
        let Some(name) = binding.binding.as_ref() else {
            continue;
        };
        if !seen_bindings.insert(name.as_str()) {
            return Err(cap!(
                "lower_match.enum_pattern_duplicate_binding",
                LoweringError::UnsupportedExpr {
                    kind: format!("duplicate enum pattern binding `{name}`"),
                    range,
                }
            ));
        }
        let field_name = enum_pattern_binding_field_name(narrowing, binding, idx);
        let field_ty = enum_payload_field_type(narrowing, &field_name, range)?;
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: scrutinee_let_idx,
                ty: IrType::I32,
            },
            range,
        });
        ctx.tstack.push(IrType::I32);
        let key = TokenKey::String(field_name, range, false);
        lower_enum_payload_path(&[key], narrowing, range, ctx)?;
        let ty = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_match.enum_pattern_binding_stack",
                LoweringError::UnsupportedExpr {
                    kind: format!("enum pattern binding `{name}` produced no value"),
                    range,
                }
            )
        })?;
        let let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx: let_idx, ty },
            range,
        });
        let schema_brand = match &field_ty {
            TypeRepr::Schema { schema } => Some(schema.name.clone()),
            _ => None,
        };
        ctx.lets.push(LetBinding {
            name: name.clone(),
            idx: let_idx,
            ty,
            schema_brand,
            type_repr: Some(field_ty.clone()),
        });
        added.push(name.clone());
    }
    Ok(added)
}

pub(super) fn enum_tag_test_ops(
    scrutinee_let_idx: u32,
    tag: u8,
    range: TokenRange,
) -> Vec<TaggedOp> {
    vec![
        TaggedOp {
            op: Op::LetGet {
                idx: scrutinee_let_idx,
                ty: IrType::I32,
            },
            range,
        },
        TaggedOp {
            op: Op::LoadI8UAtAbsolute { offset: 0 },
            range,
        },
        TaggedOp {
            op: Op::ConstI32(i32::from(tag)),
            range,
        },
        TaggedOp {
            op: Op::Eq(IrType::I32),
            range,
        },
    ]
}

pub(super) fn runtime_enum_match_chain(
    arms: &[RuntimeEnumMatchArm],
    scrutinee_let_idx: u32,
    result_ty: IrType,
    range: TokenRange,
) -> Vec<TaggedOp> {
    let mut else_body = arms
        .last()
        .expect("runtime enum match must have at least one arm")
        .body_ops
        .clone();
    for arm in arms[..arms.len().saturating_sub(1)].iter().rev() {
        let Some(tag) = arm.tag else {
            else_body = arm.body_ops.clone();
            continue;
        };
        let mut seq = enum_tag_test_ops(scrutinee_let_idx, tag, arm.range);
        seq.push(TaggedOp {
            op: Op::If {
                result_ty,
                then_body: arm.body_ops.clone(),
                else_body,
            },
            range,
        });
        else_body = seq;
    }
    else_body
}

pub(super) fn lower_branch_with_enum_narrowing(
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
    scrutinee_name: &str,
    scrutinee_let_idx: u32,
    narrowing: Option<EnumVariantNarrowing>,
    pattern_bindings: &[PatternBinding],
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let previous = narrowing.clone().map(|narrowing| {
        parent
            .enum_variant_narrowing
            .insert(scrutinee_name.to_string(), narrowing)
    });

    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    let let_len_before = parent.lets.len();

    if let Some(narrowing) = narrowing.as_ref() {
        emit_enum_pattern_bindings(
            scrutinee_let_idx,
            narrowing,
            pattern_bindings,
            range,
            parent,
        )?;
    }
    lower_expr(&node.expr, node.range, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    parent.lets.truncate(let_len_before);

    if let Some(previous) = previous {
        match previous {
            Some(old) => {
                parent
                    .enum_variant_narrowing
                    .insert(scrutinee_name.to_string(), old);
            }
            None => {
                parent.enum_variant_narrowing.remove(scrutinee_name);
            }
        }
    }

    if branch_stack.len() != 1 {
        return Err(cap!(
            "lower_branch.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("Match(branch-stack={})", branch_stack.len()),
                range,
            }
        ));
    }
    Ok((branch_ops, branch_stack[0]))
}

pub(super) fn try_lower_runtime_enum_match(
    scrutinee: &Node,
    arms: &[(Node, Node)],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    let Some((scrutinee_name, enum_ty)) = enum_scrutinee_binding(scrutinee, ctx) else {
        return Ok(false);
    };
    if enum_like_name(&enum_ty).is_none() {
        return Ok(false);
    }
    if arms.is_empty() {
        return Err(cap!(
            "lower_match.empty_enum_match",
            LoweringError::UnsupportedExpr {
                kind: "Match(enum with no arms)".to_string(),
                range,
            }
        ));
    }

    let scrutinee_let_idx = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    let all_tags: HashSet<u8> = enum_like_tags(&enum_ty)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut covered_tags: HashSet<u8> = HashSet::new();
    let mut lowered: Vec<RuntimeEnumMatchArm> = Vec::new();
    let mut has_wildcard = false;

    for (pattern, body) in arms {
        match &*pattern.expr {
            Expr::Wildcard => {
                let (body_ops, body_ty) = lower_branch(body, range, ctx)?;
                lowered.push(RuntimeEnumMatchArm {
                    tag: None,
                    body_ops,
                    body_ty,
                    range: pattern.range,
                });
                has_wildcard = true;
                break;
            }
            Expr::Type(_) | Expr::VariantPattern { .. } => {
                let Some((narrowing, pattern_bindings)) =
                    enum_pattern_variant(&enum_ty, pattern.expr.as_ref(), pattern.range)
                else {
                    return Err(cap!(
                        "lower_match.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "Match(enum pattern `{}` is not a variant of the scrutinee enum)",
                                pattern.expr.kind()
                            ),
                            range: pattern.range,
                        }
                    ));
                };
                let tag = narrowing.variant.tag;
                let narrowing = Some(narrowing);
                let (body_ops, body_ty) = lower_branch_with_enum_narrowing(
                    body,
                    range,
                    ctx,
                    &scrutinee_name,
                    scrutinee_let_idx,
                    narrowing,
                    &pattern_bindings,
                )?;
                covered_tags.insert(tag);
                lowered.push(RuntimeEnumMatchArm {
                    tag: Some(tag),
                    body_ops,
                    body_ty,
                    range: pattern.range,
                });
            }
            other => {
                return Err(cap!(
                    "lower_match.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Match(enum pattern `{}` is not supported by compiled runtime dispatch)",
                            other.kind()
                        ),
                        range: pattern.range,
                    }
                ));
            }
        }
    }

    if lowered.is_empty() {
        return Err(cap!(
            "lower_match.empty_enum_match",
            LoweringError::UnsupportedExpr {
                kind: "Match(enum with no lowerable arms)".to_string(),
                range,
            }
        ));
    }

    let result_ty = lowered[0].body_ty;
    for arm in lowered.iter().skip(1) {
        if arm.body_ty != result_ty {
            return Err(cap!(
                "lower_ternary.if_branch_type_mismatch",
                LoweringError::IfBranchTypeMismatch {
                    then_ty: result_ty,
                    else_ty: arm.body_ty,
                    range,
                }
            ));
        }
    }

    // Non-exhaustive enum match with no `_` catch-all: every uncovered
    // tag must trap at runtime exactly as the tree-walk oracle does
    // (`TypeMismatch { expected: "a matching arm" }`). Append a synthetic
    // trailing wildcard arm whose body is the `TrapKind::NoMatch` trap +
    // a typed placeholder, so the dispatch chain's innermost `else`
    // traps instead of silently returning a wrong arm's body.
    if !has_wildcard && !all_tags.is_subset(&covered_tags) {
        let Some(body_ops) = no_match_trap_body_ops(result_ty, range, ctx) else {
            return Err(cap!(
                "lower_match.no_match_trap_result_ty",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Match(non-exhaustive enum, no-match trap placeholder unavailable for \
                         result type {result_ty:?})"
                    ),
                    range,
                }
            ));
        };
        lowered.push(RuntimeEnumMatchArm {
            tag: None,
            body_ops,
            body_ty: result_ty,
            range,
        });
        has_wildcard = true;
    }
    let _ = has_wildcard;

    lower_expr(&scrutinee.expr, scrutinee.range, ctx)?;
    let scrut_ty = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_match.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: "Match(enum scrutinee produced no value)".to_string(),
                range: scrutinee.range,
            }
        )
    })?;
    if scrut_ty != IrType::I32 {
        return Err(cap!(
            "lower_match.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Match(enum scrutinee lowered to {scrut_ty:?}, expected I32 pointer)"
                ),
                range: scrutinee.range,
            }
        ));
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: scrutinee_let_idx,
            ty: IrType::I32,
        },
        range: scrutinee.range,
    });

    let chain = runtime_enum_match_chain(&lowered, scrutinee_let_idx, result_ty, range);
    ctx.out.extend(chain);
    ctx.tstack.push(result_ty);
    Ok(true)
}

/// Wave R5 — static lowering of a strict-mode `match` whose scrutinee's
/// type is statically known, so the winning arm is selected at compile
/// time (no runtime brand dispatch).
///
/// Semantics matched byte-for-byte against the tree-walk `Expr::Match`
/// arm (see `relon-evaluator`'s `eval.rs`):
///
/// 1. The scrutinee is evaluated exactly once (and any trap fires).
/// 2. Arms are tried in SOURCE ORDER; the first arm whose pattern the
///    scrutinee's static type satisfies wins.
/// 3. The winning arm's body is the result.
///
/// The static decision per arm is made by [`static_arm_decision`], which
/// is proven to agree with the runtime `check_type` / brand-equality for
/// the scrutinee's static type. If ANY arm before the winner is
/// undecidable, or no arm statically matches (the eval would trap with
/// `TypeMismatch { expected: "a matching arm" }`, a cross-backend trap
/// shape the static layer cannot yet surface), the whole construct is
/// kept `cap!`'d and deferred (R6 — the `#relaxed` / dynamic
/// brand-dispatch form lives there too).
pub(super) fn lower_match(
    scrutinee: &Node,
    arms: &[(Node, Node)],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if try_lower_runtime_enum_match(scrutinee, arms, range, ctx)? {
        return Ok(());
    }

    // 1. Determine the scrutinee's static IR type by speculatively
    //    lowering it (rolled back — the real evaluation is re-emitted
    //    below for trap / side-effect parity).
    let ty = probe_expr_ir_ty(scrutinee, ctx)?;

    // 2. Determine the scrutinee's static schema brand, if it is a plain
    //    variable path rooted at a schema-typed binding. Any other
    //    scrutinee shape carries no brand (brand == None).
    let brand: Option<String> = match &*scrutinee.expr {
        Expr::Variable(path) => resolve_receiver_schema_brand(path, ctx),
        _ => None,
    };

    // 3. Walk arms in source order. Select the first arm that statically
    //    matches. Bail (cap) the moment an earlier arm is undecidable —
    //    its runtime match could pre-empt our chosen arm.
    let mut selected: Option<usize> = None;
    for (idx, (pattern, _body)) in arms.iter().enumerate() {
        match static_arm_decision(ty, brand.as_deref(), &pattern.expr) {
            StaticArmDecision::Matches => {
                selected = Some(idx);
                break;
            }
            StaticArmDecision::Never => continue,
            StaticArmDecision::Undecidable => {
                // Defensive cap: an undecidable arm means the scrutinee is
                // not pinned to a single type, i.e. this is dynamic
                // runtime-`#brand` dispatch. The analyzer now rejects that
                // shape up-front (`Diagnostic::DynamicBrandDispatchMatch`),
                // so a well-analyzed program never reaches here. We keep
                // the cap rather than `unreachable!` so a caller that
                // lowers un-analyzed IR fails honestly instead of
                // miscompiling.
                return Err(cap!(
                    "lower_match.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Match(arm pattern not statically decidable for scrutinee type \
                             {ty:?} brand {brand:?} — dynamic brand-dispatch is rejected by \
                             the analyzer; declare an `#enum`)"
                        ),
                        range: pattern.range,
                    }
                ));
            }
        }
    }

    let Some(selected) = selected else {
        // No arm statically matches: the construct ALWAYS traps at
        // runtime, exactly as the tree-walk oracle's `Expr::Match`
        // no-match path does (`TypeMismatch { expected: "a matching
        // arm" }`). `TrapKind::NoMatch` lifts to that same typed
        // `RuntimeError::TypeMismatch` on every backend (cranelift /
        // llvm / wasm), so we lower a guaranteed trap rather than cap.
        //
        // We need a typed value on the stack for the verifier's
        // both-arms-typed / result-type contract even though the trap
        // makes everything after it unreachable. The first arm's body
        // type is the match's nominal result type; re-lowering that body
        // after the trap yields a correctly-typed (dead) placeholder.
        // Probe it first so a body we cannot lower caps cleanly instead
        // of half-emitting.
        let result_ty = probe_expr_ir_ty(&arms[0].1, ctx)?;

        // Evaluate the scrutinee for value / trap / side-effect-ordering
        // parity (its own traps fire here), then discard it.
        lower_expr(&scrutinee.expr, scrutinee.range, ctx)?;
        let scrut_ty = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_match.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: "Match(scrutinee produced no value)".to_string(),
                    range: scrutinee.range,
                }
            )
        })?;
        let discard_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: discard_idx,
                ty: scrut_ty,
            },
            range: scrutinee.range,
        });

        // The guaranteed no-match trap.
        ctx.out.push(TaggedOp {
            op: Op::Trap {
                kind: TrapKind::NoMatch,
            },
            range,
        });

        // Dead placeholder of the result type so the type stack / wasm
        // verifier stay satisfied (mirrors the stdlib bounds-trap which
        // emits `Op::Trap` followed by a typed const). Re-lower the first
        // arm body; everything here is unreachable past the trap.
        let body = &arms[0].1;
        lower_expr(&body.expr, body.range, ctx)?;
        let placeholder_ty = ctx.tstack.last().copied();
        debug_assert_eq!(
            placeholder_ty,
            Some(result_ty),
            "no-match placeholder type drifted from probe"
        );
        return Ok(());
    };

    // 4. Evaluate the scrutinee for value / trap / side-effect-ordering
    //    parity, then discard its value into a fresh internal let-local
    //    that is never read (the R4 `type(v)` discard pattern). The
    //    scrutinee's traps already fired in its op stream.
    lower_expr(&scrutinee.expr, scrutinee.range, ctx)?;
    let scrut_ty = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_match.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: "Match(scrutinee produced no value)".to_string(),
                range: scrutinee.range,
            }
        )
    })?;
    let discard_idx = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: discard_idx,
            ty: scrut_ty,
        },
        range: scrutinee.range,
    });

    // 5. Lower the selected arm's body as the result of the whole match.
    let body = &arms[selected].1;
    lower_expr(&body.expr, body.range, ctx)
}
