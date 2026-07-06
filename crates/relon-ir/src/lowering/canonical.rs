//! Lowering sub-module: `SchemaDef` → canonical `Schema` / `Enum`
//! resolution and field-default topology.
//!
//! Owns generic substitution (`GenericSubst`), the
//! `canonical_*_from_def` family that turns analyzer schema defs into
//! the canonical `TypeRepr` world, and the field-default reference
//! graph: `topo_order_fields`, cycle reporting, and resolvability
//! checks for `&sibling`-style default references.

use super::*;

// =====================================================================
// Phase 3.b: dict-literal lowering helpers.
// =====================================================================

pub(super) type GenericSubst = HashMap<String, TypeNode>;

pub(super) fn generic_subst_for_def(def: &SchemaDef, ty: &TypeNode) -> Option<GenericSubst> {
    if def.generics.len() != ty.generics.len() {
        return None;
    }
    Some(
        def.generics
            .iter()
            .cloned()
            .zip(ty.generics.iter().cloned())
            .collect(),
    )
}

pub(super) fn apply_generic_subst(ty: &TypeNode, subst: &GenericSubst) -> TypeNode {
    if subst.is_empty() {
        ty.clone()
    } else {
        relon_analyzer::substitute_generics_in_typenode(ty, subst)
    }
}

/// If `return_type` names a user-declared record schema (single-segment
/// TypeNode with no generics), return its canonical-form `Schema`
/// recursively flattened. Returns `Ok(None)` for custom `#enum` so the
/// normal single-field return path can carry `TypeRepr::Enum`.
pub(super) fn resolve_return_user_schema(
    return_type: Option<&TypeNode>,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<Schema>, LoweringError> {
    let Some(t) = return_type else {
        return Ok(None);
    };
    if t.path.len() != 1 || !t.generics.is_empty() || t.variant_fields.is_some() {
        return Ok(None);
    }
    let name = &t.path[0];
    // Built-in scalar / wrapper heads stay on the scalar path even
    // though they would also fail the user-schema lookup below.
    if matches!(
        name.as_str(),
        "Int"
            | "Float"
            | "Bool"
            | "String"
            | "List"
            | "Option"
            | "Result"
            | "Tuple"
            | "Null"
            | "Unit"
    ) {
        return Ok(None);
    }
    let Some(def) = resolver.resolve(name) else {
        return Ok(None);
    };
    if !def.variants.is_empty() {
        return Ok(None);
    }
    let mut stack: Vec<&str> = Vec::new();
    let schema = canonical_schema_from_def(def, resolver, &mut stack, t.range)?;
    Ok(Some(schema))
}

pub(super) fn canonical_enum_from_def<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    canonical_enum_from_def_with_subst(def, resolver, stack, range, &GenericSubst::new())
}

pub(super) fn canonical_enum_from_def_with_subst<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
    subst: &GenericSubst,
) -> Result<TypeRepr, LoweringError> {
    let name = def.name.as_deref().ok_or_else(|| {
        cap!(
            "canonical_schema_from_def.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: "anonymous-enum-schema".to_string(),
                range,
            }
        )
    })?;
    if stack.contains(&name) {
        let mut cycle: Vec<String> = stack.iter().map(|s| s.to_string()).collect();
        cycle.push(name.to_string());
        return Err(cap!(
            "canonical_schema_from_def.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: name.to_string(),
                cycle,
                range,
            }
        ));
    }
    if def.variants.len() > u8::MAX as usize + 1 {
        return Err(cap!(
            "canonical_schema_from_def.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("enum `{name}` has more than 256 variants"),
                range,
            }
        ));
    }

    stack.push(name);
    let mut variants = Vec::with_capacity(def.variants.len());
    for (tag, variant) in def.variants.iter().enumerate() {
        let mut fields = Vec::with_capacity(variant.fields.len());
        for field in &variant.fields {
            let ty_node = field.type_hint.as_ref().ok_or_else(|| {
                cap!(
                    "canonical_schema_from_def.unsupported_field_type",
                    LoweringError::UnsupportedFieldType {
                        schema: name.to_string(),
                        field: format!("{}.{}", variant.name, field.name),
                        ty: "<untyped>".to_string(),
                        range: field.value_range,
                    }
                )
            })?;
            fields.push(Field {
                name: field.name.clone(),
                ty: canonical_type_repr_with_subst(
                    ty_node,
                    resolver,
                    stack,
                    field.value_range,
                    subst,
                )?,
                default: None,
            });
        }
        variants.push(CanonicalEnumVariant {
            name: variant.name.clone(),
            tag: tag as u8,
            is_tuple: fields_are_tuple_payload(&fields),
            fields,
        });
    }
    stack.pop();
    Ok(TypeRepr::Enum {
        name: name.to_string(),
        variants,
    })
}

pub(super) fn fields_are_tuple_payload(fields: &[Field]) -> bool {
    !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(idx, field)| field.name == idx.to_string())
}

/// Recursively build a canonical [`Schema`] from a [`SchemaDef`].
///
/// `stack` carries the in-progress schema names so a cycle in nested
/// declarations (`#schema A { B b: * }`, `#schema B { A a: * }`)
/// surfaces as [`LoweringError::CyclicFieldDependency`] rather than
/// infinite recursion. Cycles in nested-schema *types* are
/// independent of the per-schema field-default cycle the topological
/// emit pass detects later — both surface the same error variant so
/// users get a uniform diagnostic for either shape.
pub(super) fn canonical_schema_from_def<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<Schema, LoweringError> {
    canonical_schema_from_def_with_subst(def, resolver, stack, range, &GenericSubst::new())
}

pub(super) fn canonical_schema_from_def_with_subst<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
    subst: &GenericSubst,
) -> Result<Schema, LoweringError> {
    let name = def.name.as_deref().ok_or_else(|| {
        cap!(
            "canonical_schema_from_def.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: "anonymous-nested-schema".to_string(),
                range,
            }
        )
    })?;
    if stack.contains(&name) {
        let mut cycle: Vec<String> = stack.iter().map(|s| s.to_string()).collect();
        cycle.push(name.to_string());
        return Err(cap!(
            "canonical_schema_from_def.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: name.to_string(),
                cycle,
                range,
            }
        ));
    }
    stack.push(name);
    if let Some(elements) = &def.tuple_elements {
        let mut tys = Vec::with_capacity(elements.len());
        for (idx, ty_node) in elements.iter().enumerate() {
            let ty = canonical_type_repr_with_subst(ty_node, resolver, stack, range, subst)
                .map_err(|_| {
                    cap!(
                        "canonical_schema_from_def.unsupported_tuple_element_type",
                        LoweringError::UnsupportedFieldType {
                            schema: name.to_string(),
                            field: idx.to_string(),
                            ty: type_head_for_display(ty_node),
                            range: ty_node.range,
                        }
                    )
                })?;
            tys.push(ty);
        }
        stack.pop();
        let mut schema = Schema::tuple(name.to_string(), tys);
        schema.generics = def.generics.clone();
        return Ok(schema);
    }
    let mut fields = Vec::with_capacity(def.fields.len());
    for f in &def.fields {
        let ty_node = f.type_hint.as_ref().ok_or_else(|| {
            cap!(
                "canonical_schema_from_def.unsupported_field_type",
                LoweringError::UnsupportedFieldType {
                    schema: name.to_string(),
                    field: f.name.clone(),
                    ty: "<untyped>".to_string(),
                    range: f.value_range,
                }
            )
        })?;
        let ty = canonical_type_repr_with_subst(ty_node, resolver, stack, f.value_range, subst)?;
        fields.push(Field {
            name: f.name.clone(),
            ty,
            default: None,
        });
    }
    stack.pop();
    Ok(Schema {
        name: name.to_string(),
        generics: def.generics.clone(),
        fields,
        is_tuple: false,
    })
}

/// Convert a schema field type into the canonical [`TypeRepr`]. This is the
/// resolver-aware form used for named schemas, including tuple schemas.
pub(super) fn canonical_type_repr<'a>(
    ty: &TypeNode,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    canonical_type_repr_with_subst(ty, resolver, stack, range, &GenericSubst::new())
}

pub(super) fn canonical_type_repr_with_subst<'a>(
    ty: &TypeNode,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
    subst: &GenericSubst,
) -> Result<TypeRepr, LoweringError> {
    let concrete_ty;
    let ty = if subst.is_empty() {
        ty
    } else {
        concrete_ty = apply_generic_subst(ty, subst);
        &concrete_ty
    };
    if ty.path.len() != 1 || ty.variant_fields.is_some() {
        return Err(cap!(
            "canonical_type_repr.unsupported_field_type.1",
            LoweringError::UnsupportedFieldType {
                schema: stack.last().copied().unwrap_or("?").to_string(),
                field: "?".to_string(),
                ty: type_head_for_display(ty),
                range,
            }
        ));
    }

    let head = ty.path[0].as_str();
    if is_removed_unit_null_type_name(head) {
        return Err(cap!(
            "canonical_type_repr.unsupported_field_type.reserved",
            LoweringError::UnsupportedFieldType {
                schema: stack.last().copied().unwrap_or("?").to_string(),
                field: "?".to_string(),
                ty: head.to_string(),
                range,
            }
        ));
    }

    let base = match (head, ty.generics.as_slice()) {
        ("Int", []) => TypeRepr::Int,
        ("Float", []) => TypeRepr::Float,
        ("Bool", []) => TypeRepr::Bool,
        ("String", []) => TypeRepr::String,
        ("List", [elem]) => TypeRepr::List {
            element: Box::new(canonical_type_repr(elem, resolver, stack, range)?),
        },
        ("Option", [inner]) => TypeRepr::Option {
            inner: Box::new(canonical_type_repr(inner, resolver, stack, range)?),
        },
        ("Result", [ok, err]) => TypeRepr::Result {
            ok: Box::new(canonical_type_repr(ok, resolver, stack, range)?),
            err: Box::new(canonical_type_repr(err, resolver, stack, range)?),
        },
        ("Tuple", _) => TypeRepr::Schema {
            schema: Box::new(
                tuple_type_node_to_schema(ty, Some(resolver)).ok_or_else(|| {
                    cap!(
                        "canonical_type_repr.unsupported_field_type.tuple",
                        LoweringError::UnsupportedFieldType {
                            schema: stack.last().copied().unwrap_or("?").to_string(),
                            field: "?".to_string(),
                            ty: type_head_for_display(ty),
                            range,
                        }
                    )
                })?,
            ),
        },
        _ => {
            if matches!(
                head,
                "Int" | "Float" | "Bool" | "String" | "List" | "Option" | "Result" | "Tuple"
            ) {
                return Err(cap!(
                    "canonical_type_repr.unsupported_field_type.2",
                    LoweringError::UnsupportedFieldType {
                        schema: stack.last().copied().unwrap_or("?").to_string(),
                        field: "?".to_string(),
                        ty: type_head_for_display(ty),
                        range,
                    }
                ));
            }
            let Some(def) = resolver.resolve(head) else {
                return Err(cap!(
                    "canonical_type_repr.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: stack.last().copied().unwrap_or("?").to_string(),
                        field: "?".to_string(),
                        ty: head.to_string(),
                        range,
                    }
                ));
            };
            let Some(schema_subst) = generic_subst_for_def(def, ty) else {
                return Err(cap!(
                    "canonical_type_repr.unsupported_field_type.generics",
                    LoweringError::UnsupportedFieldType {
                        schema: stack.last().copied().unwrap_or("?").to_string(),
                        field: "?".to_string(),
                        ty: type_head_for_display(ty),
                        range,
                    }
                ));
            };
            if !def.variants.is_empty() {
                canonical_enum_from_def_with_subst(def, resolver, stack, range, &schema_subst)?
            } else {
                TypeRepr::Schema {
                    schema: Box::new(canonical_schema_from_def_with_subst(
                        def,
                        resolver,
                        stack,
                        range,
                        &schema_subst,
                    )?),
                }
            }
        }
    };

    Ok(maybe_optional(ty, base))
}

/// Decide topological order in which a schema's fields must be
/// emitted, given the set of user-provided field names. A field
/// that's user-provided stops dependency tracking for itself (the
/// user value wins and is independent of the schema default).
/// Otherwise the default expression's referenced sibling fields
/// become incoming edges.
///
/// Returns `Err(CyclicFieldDependency)` when the dependency graph on
/// the **needs-defaults** subset has a cycle. User-provided values
/// can break a cycle: a schema `{ Int a: b, Int b: a }` constructed
/// as `{ a: 1 }` is fine — only `b` needs defaulting and its
/// reference to `a` is satisfied by the user value.
pub(super) fn topo_order_fields(
    schema_name: &str,
    def: &SchemaDef,
    user_provided: &std::collections::HashSet<&str>,
    range: TokenRange,
) -> Result<Vec<usize>, LoweringError> {
    // Collect per-field referenced sibling field names. Only fields
    // we'll evaluate via their default expression need this — others
    // get the user-supplied value and contribute no edges.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); def.fields.len()];
    let name_to_idx: HashMap<&str, usize> = def
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    for (i, field) in def.fields.iter().enumerate() {
        if user_provided.contains(field.name.as_str()) {
            // User-supplied: ignore its default expression.
            continue;
        }
        if field.is_wildcard {
            // `Int x: *` declares the field with no default value.
            // The dict literal must provide it.
            return Err(cap!(
                "topo_order_fields.missing_field_no_default",
                LoweringError::MissingFieldNoDefault {
                    schema: schema_name.to_string(),
                    field: field.name.clone(),
                    range,
                }
            ));
        }
        collect_field_refs(&field.value_node.expr, &name_to_idx, &mut deps[i]);
        // Sanity: every reference must resolve to a sibling field.
        // We can't know yet which references are unresolved at this
        // step — `collect_field_refs` only walks bare-identifier
        // references; an unresolved one was already a diagnostic at
        // analyzer time. We still surface the case where a default
        // expression names a sibling that doesn't exist as
        // `UnknownFieldReferenceInDefault`. The walk runs the same
        // resolution again and reports the first miss.
        check_field_default_refs_resolvable(
            schema_name,
            &field.name,
            &field.value_node.expr,
            &name_to_idx,
        )?;
    }
    // Kahn-style topological sort. `incoming[i]` = number of edges
    // pointing into i. A field `j` evaluated from a default that
    // references `i` requires `i` ready first → edge `i → j`. We
    // build the graph from `deps[i] = list of i's prerequisite
    // field indices` ⇒ for every `r ∈ deps[i]` add edge `r → i`,
    // i.e. incoming[i] += 1 for each ref.
    let n = def.fields.len();
    let mut incoming = vec![0usize; n];
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, refs) in deps.iter().enumerate() {
        for &r in refs {
            outgoing[r].push(i);
            incoming[i] += 1;
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut ready: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for (i, &incoming_count) in incoming.iter().enumerate().take(n) {
        if incoming_count == 0 {
            ready.push_back(i);
        }
    }
    while let Some(i) = ready.pop_front() {
        order.push(i);
        for &j in &outgoing[i] {
            incoming[j] -= 1;
            if incoming[j] == 0 {
                ready.push_back(j);
            }
        }
    }
    if order.len() != n {
        // Find one cycle path for the error message via DFS.
        let cycle = find_cycle_path(&outgoing, def, &incoming);
        return Err(cap!(
            "topo_order_fields.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: schema_name.to_string(),
                cycle,
                range,
            }
        ));
    }
    Ok(order)
}

/// DFS through the remaining (non-zero-incoming) field-default graph
/// looking for a cycle path. The caller has already established at
/// least one cycle exists (Kahn's algorithm couldn't drain the
/// graph); we build a representative path so the user sees the field
/// chain that participates.
pub(super) fn find_cycle_path(outgoing: &[Vec<usize>], def: &SchemaDef, incoming: &[usize]) -> Vec<String> {
    let n = outgoing.len();
    let mut visited = vec![false; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..n {
        if visited[start] || incoming[start] == 0 {
            continue;
        }
        if let Some(cycle) =
            dfs_find_cycle(start, outgoing, &mut visited, &mut on_stack, &mut stack)
        {
            return cycle.iter().map(|&i| def.fields[i].name.clone()).collect();
        }
    }
    // Fallback: should never happen given the caller's invariant.
    Vec::new()
}

pub(super) fn dfs_find_cycle(
    start: usize,
    outgoing: &[Vec<usize>],
    visited: &mut [bool],
    on_stack: &mut [bool],
    stack: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    visited[start] = true;
    on_stack[start] = true;
    stack.push(start);
    for &next in &outgoing[start] {
        if on_stack[next] {
            // Cycle: emit the portion of the stack from `next` to the
            // current node, plus `next` repeated at the end for a
            // readable arrow chain.
            let entry = stack.iter().position(|&i| i == next).unwrap_or(0);
            let mut cycle = stack[entry..].to_vec();
            cycle.push(next);
            on_stack[start] = false;
            stack.pop();
            return Some(cycle);
        }
        if !visited[next] {
            if let Some(c) = dfs_find_cycle(next, outgoing, visited, on_stack, stack) {
                on_stack[start] = false;
                stack.pop();
                return Some(c);
            }
        }
    }
    on_stack[start] = false;
    stack.pop();
    None
}

/// Walk a default expression and record any bare-identifier
/// references whose head matches a sibling field. Multi-segment
/// references (`a.b.c`) only contribute the head segment — if the
/// head resolves to a sibling, the rest of the path is treated as a
/// post-access we don't track.
pub(super) fn collect_field_refs(expr: &Expr, name_to_idx: &HashMap<&str, usize>, out: &mut Vec<usize>) {
    match expr {
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            if let Some(TokenKey::String(name, _, _)) = path.first() {
                if let Some(&idx) = name_to_idx.get(name.as_str()) {
                    if !out.contains(&idx) {
                        out.push(idx);
                    }
                }
            }
        }
        Expr::Binary(_, a, b) => {
            collect_field_refs(&a.expr, name_to_idx, out);
            collect_field_refs(&b.expr, name_to_idx, out);
        }
        Expr::Unary(_, inner) => collect_field_refs(&inner.expr, name_to_idx, out),
        Expr::Ternary { cond, then, els } => {
            collect_field_refs(&cond.expr, name_to_idx, out);
            collect_field_refs(&then.expr, name_to_idx, out);
            collect_field_refs(&els.expr, name_to_idx, out);
        }
        Expr::List(items) => {
            for n in items {
                collect_field_refs(&n.expr, name_to_idx, out);
            }
        }
        Expr::Dict(pairs) => {
            for (_, v) in pairs {
                collect_field_refs(&v.expr, name_to_idx, out);
            }
        }
        Expr::Where { expr, bindings } => {
            collect_field_refs(&bindings.expr, name_to_idx, out);
            collect_field_refs(&expr.expr, name_to_idx, out);
        }
        Expr::FnCall { args, .. } => {
            for a in args {
                collect_field_refs(&a.value.expr, name_to_idx, out);
            }
        }
        // Other shapes don't matter for the Phase 3.b surface (they
        // either fail to lower upstream or don't reference siblings).
        _ => {}
    }
}

/// Recursive walker mirroring [`collect_field_refs`] that reports the
/// first bare-identifier reference whose head doesn't resolve to a
/// sibling field. Lowering uses this to surface
/// `UnknownFieldReferenceInDefault` instead of letting the inner
/// `lower_expr` fall through with an `UnresolvedVariable` (which the
/// user would see as a confusing diagnostic about `#main` params).
pub(super) fn check_field_default_refs_resolvable(
    schema: &str,
    field: &str,
    expr: &Expr,
    name_to_idx: &HashMap<&str, usize>,
) -> Result<(), LoweringError> {
    let mut stack: Vec<&Expr> = vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Variable(path) | Expr::Reference { path, .. } => {
                if let Some(TokenKey::String(name, range, _)) = path.first() {
                    if !name_to_idx.contains_key(name.as_str()) {
                        return Err(cap!("check_field_default_refs_resolvable.unknown_field_reference_in_default", LoweringError::UnknownFieldReferenceInDefault {
                            schema: schema.to_string(),
                            field: field.to_string(),
                            referenced: name.clone(),
                            range: *range,
                        }));
                    }
                }
            }
            Expr::Binary(_, a, b) => {
                stack.push(&a.expr);
                stack.push(&b.expr);
            }
            Expr::Unary(_, inner) => stack.push(&inner.expr),
            Expr::Ternary { cond, then, els } => {
                stack.push(&cond.expr);
                stack.push(&then.expr);
                stack.push(&els.expr);
            }
            Expr::List(items) => {
                for n in items {
                    stack.push(&n.expr);
                }
            }
            Expr::Dict(pairs) => {
                for (_, v) in pairs {
                    stack.push(&v.expr);
                }
            }
            Expr::Where { expr, bindings } => {
                stack.push(&expr.expr);
                stack.push(&bindings.expr);
            }
            Expr::FnCall { args, .. } => {
                for a in args {
                    stack.push(&a.value.expr);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Map a Phase 3.b `TypeRepr` to its corresponding `IrType` in dict
/// field context. Reuses [`type_repr_to_ir_type`] for the strict
/// subset (base types + `List<base>`) and extends with the cases
/// only dict fields can carry: nested branded `Schema { .. }` rides a
/// pointer slot, and `Option` / `Result` fold to i32 too. An
/// unsupported `List<…>` element (one the strict mapper already
/// rejects) is reported loudly rather than silently collapsed to
/// `IrType::ListInt` — a mistyped list slot must cap at the lowering
/// boundary, not decode as a list of integers on the host side.
pub(super) fn type_repr_to_ir_type_dict(t: &TypeRepr) -> Result<IrType, LoweringError> {
    if let Ok(ty) = type_repr_to_ir_type(t) {
        return Ok(ty);
    }
    match t {
        TypeRepr::Schema { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => Ok(IrType::I32),
        // A `List<…>` reaching here failed the strict mapper, i.e. its
        // element type is outside the supported lattice. Reject loudly
        // — mirror `type_repr_to_ir_type` — instead of pretending it is
        // a `List<Int>`.
        other => Err(cap!(
            "type_repr_to_ir_type_dict.unsupported_type_in_main",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{other:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}
