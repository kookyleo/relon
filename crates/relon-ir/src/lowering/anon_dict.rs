//! Lowering sub-module: anonymous-Dict-return planning, field
//! classification, and decorator desugar.
//!
//! Owns the W7 `#main(...) -> Dict { ... }` pipeline: builtin field
//! decorator desugaring (`@value` / `@expect` / ...), the
//! `anon_dict_return_plan` shape planner with its
//! `classify_anon_dict_*` family (scalar / str-int / list / enum-list
//! / variant-list fields), the `&sibling` reference edge graph that
//! fixes the field emit order, and `lower_anon_dict_body` which emits
//! the planned record.

use super::*;

/// Phase F.2 (W7 anon-Dict-return): plan emitted by
/// [`anon_dict_return_plan`] when `#main(...) -> Dict { ... }` is
/// being lifted from "rejected as UnsupportedTypeInMain" to "lower as
/// an anonymous schema with closure-typed fields lifted to internal
/// let-bindings".
#[derive(Debug, Clone)]
pub(super) struct AnonDictPlan {
    /// Synthesised return schema. Only carries the **scalar** fields
    /// — closure-typed source-level fields are lifted to internal
    /// let-bindings and do not appear here (they would be rejected
    /// by [`SchemaLayout::offsets_for`] anyway per the Phase B guard).
    pub(super) schema: Schema,
    /// Per-source-field classification in declaration order. The body
    /// walker iterates these to decide whether to emit a closure
    /// let-binding (no host-visible field) or a normal record store.
    pub(super) fields: Vec<AnonDictField>,
    /// R13: indices into `fields` giving the topological order the body
    /// walker must emit them in, so a `&sibling` / `&root` reference (to
    /// an earlier *or* later declared sibling) sees its target field's
    /// let already bound. For backward-only / reference-free bodies this
    /// is `0..fields.len()` (declaration order), preserving the
    /// pre-existing byte-for-byte compiled output.
    pub(super) emit_order: Vec<usize>,
}

/// One classified entry from [`AnonDictPlan::fields`]. The walker
/// pairs the source-level `name` with either a closure signature (to
/// emit `MakeClosure` + `LetSet`) or the canonical scalar type the
/// matching schema field will store.
#[derive(Debug, Clone)]
pub(super) enum AnonDictField {
    /// Source-level field whose value is an `Expr::Closure` literal.
    /// Lifted to an internal let-binding; its surface signature is
    /// memoised in `LowerCtx::closure_let_signatures` so a recursive
    /// self-call inside the body resolves to `Op::CallClosure`.
    Closure {
        name: String,
        param_tys: Vec<IrType>,
        ret_ty: IrType,
        /// Per-param mask: `true` when the param's `String` type came
        /// from the concat-body inference (not an annotation), so a
        /// call site may render a scalar argument to `String` first —
        /// see [`plan_anon_dict_closure_sig`].
        concat_coercible: Vec<bool>,
    },
    /// Source-level field whose value is a normal expression (the
    /// "host-visible" surface). Stored into the root record at the
    /// matching offset.
    Scalar { name: String, ty: TypeRepr },
    /// W5-P1: source-level field whose value is a `{str: int}` dict
    /// literal. Lifted to an internal let-binding (an `IrType::Dict`
    /// captured local materialised via `Op::ConstDict`); like a
    /// closure field it contributes no host-visible record slot.
    /// `entries` are in source declaration order.
    DictStrInt {
        name: String,
        entries: Vec<(String, i64)>,
    },
    /// W5-P4: source-level field whose value is a `["a", "b", ...]`
    /// list-of-string literal. Lifted to an internal let-binding (an
    /// `IrType::ListString` captured local materialised via
    /// `Op::ConstListString`); like a Dict field it contributes no
    /// host-visible record slot. `elements` are in source order.
    ListString { name: String, elements: Vec<String> },
    /// F1b: a host-visible field whose value is a `#main` **parameter
    /// identity** of pointer-array list type `List<Schema>` /
    /// `List<List<scalar>>`. The parameter's data lives in the *input*
    /// region while the object head sits in the *output* region — a
    /// cross-region link. Under the F1 arena-absolute slot convention
    /// the field slot stores the parameter list root's arena-absolute
    /// offset directly (no tail copy); the host's multi-region verifier
    /// classifies that offset into the input region, bounds-checks the
    /// whole reachable graph, and the reader follows it cross-region.
    /// `ty` is the canonical `List<Schema>` / `List<List<scalar>>` type
    /// (carrying the element schema). The source parameter is reached via
    /// the field's value node (the `Variable(param)` expr) in
    /// `lower_anon_dict_body`, so the name need not be stored separately.
    CrossRegionParamList { name: String, ty: TypeRepr },
}

/// Try to build an [`AnonDictPlan`] for the entry's body when the
/// return type is a bare `Dict` and the body is a dict literal.
/// Returns `Ok(None)` when the source does not match the anon-Dict
/// surface (preserving the existing
/// `build_main_return_schema → UnsupportedTypeInMain` path), and
/// `Ok(Some(_))` once the surface is recognised.
///
/// Per-field type classification today is **heuristic**: a closure
/// literal lifts to a `[I64] → I64` (or user-annotated) signature
/// matching the W7 production source shape (`fib: (Int k) -> Int => ...`); a
/// scalar field's type is taken from a small set of statically
/// derivable expressions (literal, arithmetic between literals,
/// `Variable(name)` against a `#main` Int param, and free-call
/// against a previously-classified closure field). Anything else
/// surfaces as a `LoweringError::UnsupportedExpr` — the broader
/// inference work stays Phase D scope. The shape is deliberately
/// minimal so the W7 cmp_lua workload passes the IR pass without
/// dragging analyzer-side per-field Dict inference into the picture.
/// Builtin `@`-decorator names. These resolve to host-registered
/// [`DecoratorPlugin`] semantics, not a user callable, so they have no
/// "desugar to `deco(value, args)`" form on the compiled path. `@value`
/// substitutes its first arg; `@expect`/`@msg`/`@error`/`@default` are
/// schema-field-meta hooks that are identity on ordinary values. A
/// future wave can lower the ones with a compiled-meaningful form; until
/// then they cap loudly rather than silently dropping the transform.
pub(super) const BUILTIN_DECORATOR_NAMES: &[&str] = &["value", "expect", "msg", "error", "default"];

/// Wave R11: desugar field decorators in a `#main(...) -> Dict { ... }`
/// body before the anon-Dict-return plan / body lowering run.
///
/// A decorated field `@deco(a, b) k: v` desugars to the call
/// `deco(v, a, b)` — the decorated value is the **first** positional
/// argument, the decorator's own args follow. This matches the
/// tree-walk contract exactly: `TreeWalkEvaluator::fallback_decorator`
/// prepends `value` ahead of the evaluated decorator args (closure call
/// `[value, ..args]`; native call `positional.insert(0, value)`). The
/// `examples/pricing.relon` doc-comment that reads "value appended last"
/// describes a `currency(symbol, val)` whose parameter order happens to
/// place `val` last; the evaluator's actual convention — confirmed by
/// running `@currency("USD") x: 12.3` → `"12.3 USD"` — is value-first.
///
/// Stacked decorators apply bottom-up (`@a @b v ≡ a(b(v))`): the
/// decorator nearest the value wraps first, the outermost wraps last.
/// The tree-walk iterates `node.decorators.iter().rev()`; this builds
/// the nested call in the same order (innermost `Vec::last` first).
///
/// Returns `Ok(None)` when no field carries a decorator (the rewritten
/// AST is then byte-identical to the original, so the existing lowering
/// path — and the codegen bytes it produces — are untouched). Returns
/// `Ok(Some(rewritten_root))` when at least one field was desugared.
/// Caps loudly (never silently wrong) on a decorator shape that cannot
/// become a plain `deco(value, args)` call: a builtin `@`-decorator
/// (no compiled callable), a multi-segment / dynamic decorator path, or
/// a named decorator argument (the local-closure / native call lowering
/// admits positional args only).
pub(super) fn desugar_anon_dict_decorators(root: &Node) -> Result<Option<Node>, LoweringError> {
    let Expr::Dict(pairs) = &*root.expr else {
        return Ok(None);
    };
    if pairs.iter().all(|(_, v)| v.decorators.is_empty()) {
        return Ok(None);
    }
    let mut new_pairs: Vec<(TokenKey, Node)> = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        if value.decorators.is_empty() {
            new_pairs.push((key.clone(), value.clone()));
            continue;
        }
        new_pairs.push((key.clone(), desugar_field_decorators(value)?));
    }
    let mut new_root = root.clone();
    new_root.expr = std::sync::Arc::new(Expr::Dict(new_pairs));
    Ok(Some(new_root))
}

/// Desugar one decorated field value into a nested decorator-call node.
/// See [`desugar_anon_dict_decorators`] for the arg-order / stack-order
/// contract. The returned node carries the original field's directives
/// (so a `#internal` decorated field stays internal) and `type_hint`,
/// but has its decorators stripped — the transform is now expressed as
/// the call chain in `expr`.
pub(super) fn desugar_field_decorators(value: &Node) -> Result<Node, LoweringError> {
    // Start from the bare value with decorators removed; fold each
    // decorator (innermost first) into a call wrapping the running node.
    let mut inner = value.clone();
    let decorators = std::mem::take(&mut inner.decorators);
    // Strip directives off the running call node — directives belong to
    // the field, not to the synthetic intermediate calls. They are
    // re-attached to the outermost node at the end.
    let directives = std::mem::take(&mut inner.directives);
    let type_hint = inner.type_hint.take();

    let mut current = inner;
    // Bottom-up: the decorator nearest the value (`Vec::last` — source
    // order stacks outermost-first into the vec) wraps first.
    for dec in decorators.iter().rev() {
        // Decorator path must be a single plain identifier resolving to
        // a user callable; multi-segment / dynamic paths have no
        // compiled-call form here.
        let path_ok =
            dec.path.len() == 1 && matches!(dec.path.first(), Some(TokenKey::String(_, _, _)));
        if !path_ok {
            return Err(cap!(
                "desugar_field_decorators.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: "field decorator with multi-segment / dynamic path".to_string(),
                    range: dec.range,
                }
            ));
        }
        let TokenKey::String(name, _, _) = &dec.path[0] else {
            unreachable!("guarded by path_ok");
        };
        if BUILTIN_DECORATOR_NAMES.contains(&name.as_str()) {
            return Err(cap!(
                "desugar_field_decorators.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("builtin `@{name}` decorator has no compiled call form"),
                    range: dec.range,
                }
            ));
        }
        // Named decorator args can't be threaded through the positional-
        // only local-closure / native call lowering; cap loudly.
        if dec.args.iter().any(|a| a.name.is_some()) {
            return Err(cap!(
                "desugar_field_decorators.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: format!("field decorator `@{name}` with a named argument"),
                    range: dec.range,
                }
            ));
        }
        // Build `deco(current, ..dec.args)` — value first, then the
        // decorator's own positional args.
        let mut call_args: Vec<relon_parser::CallArg> = Vec::with_capacity(dec.args.len() + 1);
        call_args.push(relon_parser::CallArg {
            name: None,
            value: current,
        });
        call_args.extend(dec.args.iter().cloned());
        current = Node::new(
            Expr::FnCall {
                path: dec.path.clone(),
                args: call_args,
            },
            dec.range,
        );
    }

    // Re-attach the field's directives + type hint to the outermost call.
    current.directives = directives;
    current.type_hint = type_hint;
    Ok(current)
}

pub(super) fn anon_dict_return_plan(
    sig: &MainSignature,
    root: &Node,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<AnonDictPlan>, LoweringError> {
    let Some(rt) = sig.return_type.as_ref() else {
        return Ok(None);
    };
    if !type_node_is_bare_dict(rt) {
        return Ok(None);
    }
    let Expr::Dict(pairs) = &*root.expr else {
        return Ok(None);
    };

    // Build a quick scalar-type index for the `#main` parameters so
    // a `Variable(n)` on the RHS of a scalar field classifies cleanly.
    let mut param_tys: HashMap<&str, IrType> = HashMap::new();
    for p in &sig.params {
        if let Some(canonical) = type_node_to_canonical(&p.type_node) {
            if let Ok(irt) = type_repr_to_ir_type(&canonical) {
                param_tys.insert(p.name.as_str(), irt);
            }
        }
    }
    // F1b: full canonical types (carrying element schemas) for every
    // `#main` parameter, so a host-visible field whose value is a
    // parameter identity of `List<Schema>` / `List<List<scalar>>` type
    // can be classified as a cross-region field rather than rejected.
    let mut param_canonicals: HashMap<&str, TypeRepr> = HashMap::new();
    for p in &sig.params {
        if let Some(canonical) = type_node_to_canonical_with_schemas(&p.type_node, resolver) {
            param_canonicals.insert(p.name.as_str(), canonical);
        }
    }

    // R13: reference-aware emit order. Fields are classified (and later
    // lowered) in topological order over their `&sibling` / `&root`
    // reference edges so a forward reference sees its target already
    // bound; a reference cycle surfaces here as a loud error aligned
    // with the tree-walk oracle's `CircularReference`. Backward-only /
    // reference-free bodies reproduce declaration order exactly, so the
    // pre-existing compiled output stays byte-for-byte identical. The
    // `#main` param name set drives the forward-reference oracle-
    // agreement gate (a forward ref into a reference-bearing field whose
    // component reads a param diverges from the tree-walk oracle).
    let main_param_names: HashSet<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
    let emit_order = anon_dict_emit_order(pairs, &main_param_names, root.range)?;

    // Classified entries indexed by *declaration* position so the
    // synthesised return schema (and its layout) keeps declaration
    // order regardless of the classification order. `None` marks a
    // dropped `#internal` scalar field.
    let mut fields_by_decl: Vec<Option<AnonDictField>> = vec![None; pairs.len()];
    let mut closure_field_sigs: HashMap<&str, (Vec<IrType>, IrType)> = HashMap::new();
    // W5-P3: `{String -> Int}` dict fields seen so far, so a later
    // sibling field's `d[k]` index classifies to the dict's `Int`
    // value type. Source order makes `d` visible before `result`.
    let mut dict_field_names: HashSet<&str> = HashSet::new();
    // R10/R13: host-visible scalar / list fields classified so far,
    // name -> IR type. A `&sibling.<name>` (or entry-level
    // `&root.<name>`, which is the same — the entry dict IS the root)
    // classifies to the target field's type. Topological classification
    // order guarantees the target is in this map before the reference is
    // classified, for both backward and forward references.
    let mut scalar_field_irts: HashMap<&str, IrType> = HashMap::new();

    for &decl_idx in &emit_order {
        let (key, value) = &pairs[decl_idx];
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "anon_dict_return_plan.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: "Dict(non-string-key in anon-Dict-return body)".to_string(),
                    range: root.range,
                }
            ));
        };
        // A field carrying a `#internal` pragma is hidden from the
        // host-visible return surface (the tree-walk oracle drops it
        // too — see the W7 dict-probe `#internal keys` workload).
        // Non-`#internal` collection fields, by contrast, MUST be
        // marshalled into the return buffer to match the oracle; a
        // List literal that is *not* internal becomes a host-visible
        // `List<elem>` field rather than a silently-dropped internal
        // let-binding.
        let is_internal = node_marked_internal(value);
        match &*value.expr {
            Expr::Closure {
                params,
                return_type,
                body,
            } => {
                // A closure value can never cross the host boundary, so
                // it is only legal as an internal helper binding. A
                // non-`#internal` closure field would otherwise be
                // silently dropped from the host output (the tree-walk
                // oracle errors on returning a closure), so reject it.
                if !is_internal {
                    return Err(cap!(
                        "anon_dict_return_plan.closure_across_boundary",
                        LoweringError::ClosureAcrossBoundary {
                            context: format!(
                                "anon-Dict-return field `{name}` is a closure but not `#internal`"
                            ),
                            range: value.range,
                        }
                    ));
                }
                // Read the real `(param_tys, ret_ty)` from the type
                // system: explicit param / return annotations first,
                // then a conservative String-concat body inference, then
                // the historical I64 default (W7 fib). See
                // `plan_anon_dict_closure_sig`.
                let (param_irts, ret_ty, concat_coercible) =
                    plan_anon_dict_closure_sig(params, return_type.as_ref(), &body.expr);
                closure_field_sigs.insert(name.as_str(), (param_irts.clone(), ret_ty));
                fields_by_decl[decl_idx] = Some(AnonDictField::Closure {
                    name: name.clone(),
                    param_tys: param_irts,
                    ret_ty,
                    concat_coercible,
                });
            }
            Expr::Dict(inner_pairs) => {
                // W5-P1: a `{str: int}` dict literal becomes a
                // dict-value internal let-binding. Only the
                // `{String -> Int}` shape is accepted in P1 — any
                // other entry shape surfaces UnsupportedExpr so the
                // edge stays honest (P2/P3 widen value/key types).
                //
                // A non-`#internal` dict-valued field has no compiled-
                // backend marshalling today (Dict is not a return type
                // on the buffer protocol), and the tree-walk oracle
                // *would* surface it — so leaving it as an internal
                // binding silently drops host-visible data. Reject it
                // loudly instead.
                if !is_internal {
                    return Err(cap!("anon_dict_return_plan.unsupported_field_type", LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: name.clone(),
                        ty: "Dict-valued anon-Dict-return field is only supported as `#internal`"
                            .to_string(),
                        range: value.range,
                    }));
                }
                let entries = classify_anon_dict_str_int_field(inner_pairs, value.range, name)?;
                dict_field_names.insert(name.as_str());
                fields_by_decl[decl_idx] = Some(AnonDictField::DictStrInt {
                    name: name.clone(),
                    entries,
                });
            }
            Expr::List(items) => {
                if is_internal {
                    // W5-P4: a `#internal ["a", "b", ...]` list-of-string
                    // literal becomes a `ListString` internal let-binding
                    // (the `#internal keys` field of the dict-probe
                    // workload). Only the all-String-literal shape is
                    // accepted; any other element surfaces UnsupportedExpr
                    // so the edge stays honest.
                    let elements = classify_anon_dict_list_string_field(items, value.range, name)?;
                    fields_by_decl[decl_idx] = Some(AnonDictField::ListString {
                        name: name.clone(),
                        elements,
                    });
                } else {
                    // Host-visible list field: classify the element type
                    // (`List<Int/Float/Bool/String>`) and emit a real
                    // record field. The body walker lowers the list
                    // literal to a const-pool record and marshals it into
                    // the return buffer's tail (pointer-indirect). Any
                    // shape the marshaller cannot handle (mixed / empty /
                    // nested element lists / schema elements) surfaces a
                    // loud error rather than silently dropping the field.
                    let list_ty = classify_anon_dict_list_field(
                        items,
                        value.range,
                        name,
                        resolver,
                        &param_tys,
                    )?;
                    // R13: register the list field's IR type so a sibling
                    // `&sibling.<name>` / `&root.<name>` reference (forward
                    // or backward) resolves to the same `List<...>` type.
                    if let Ok(irt) = type_repr_to_ir_type(&list_ty) {
                        scalar_field_irts.insert(name.as_str(), irt);
                    }
                    fields_by_decl[decl_idx] = Some(AnonDictField::Scalar {
                        name: name.clone(),
                        ty: list_ty,
                    });
                }
            }
            Expr::Variable(path)
                if !is_internal
                    && path
                        .iter()
                        .all(|seg| matches!(seg, TokenKey::String(_, _, _)))
                    && anon_dict_cross_region_param_list(path, &param_canonicals).is_some() =>
            {
                // F1b: a host-visible field whose value is a parameter
                // identity of `List<Schema>` / `List<List<scalar>>` type.
                // The object head is built in out_buf but the parameter's
                // list data lives in in_buf — a cross-region link. Under
                // the F1 arena-absolute slot convention the field slot
                // stores the parameter list root's arena-absolute offset
                // (the value `LoadListSchemaPtr` / `LoadListListPtr` pushes
                // post-F1) directly, with no tail copy; the host's
                // multi-region verifier classifies the offset into in_buf,
                // bounds-checks the reachable graph, then the reader
                // follows it cross-region. Only the in-place reader's
                // decode envelope is admitted (`List<Schema>` element
                // sub-records confined to S4-scope field shapes); anything
                // deeper stays a loud cap.
                let ty = anon_dict_cross_region_param_list(path, &param_canonicals)
                    .expect("guarded by the match arm guard")
                    .clone();
                fields_by_decl[decl_idx] = Some(AnonDictField::CrossRegionParamList {
                    name: name.clone(),
                    ty,
                });
            }
            _ => {
                // An `#internal` scalar field is hidden from the host
                // (the tree-walk oracle drops it). Scalar internals are
                // not referenceable by siblings on this surface — a
                // `Variable(name)` against one already loud-errors in
                // `classify_anon_dict_scalar_field_irt` — so there is no
                // let-binding to keep; just drop it. Without this skip
                // the field would surface to the host while tree-walk
                // omits it (a silent field-set divergence). The value is
                // pure, so dropping it changes no observable behaviour.
                if is_internal {
                    continue;
                }
                let ty = classify_anon_dict_scalar_field(
                    &value.expr,
                    value.range,
                    &param_tys,
                    &closure_field_sigs,
                    &dict_field_names,
                    &scalar_field_irts,
                    name,
                )?;
                if let Ok(irt) = type_repr_to_ir_type(&ty) {
                    scalar_field_irts.insert(name.as_str(), irt);
                }
                fields_by_decl[decl_idx] = Some(AnonDictField::Scalar {
                    name: name.clone(),
                    ty,
                });
            }
        }
    }

    // Collapse the declaration-indexed slots into the declaration-order
    // field list (dropped `#internal` scalar slots stay `None`). The
    // schema / layout below is built from this declaration-ordered list,
    // so record offsets are independent of the topological emit order.
    // `decl_to_field` maps each surviving declaration index to its
    // position in `fields` so the topological `emit_order` (in
    // declaration indices) can be re-expressed in `fields` indices for
    // the body walker.
    let mut fields: Vec<AnonDictField> = Vec::with_capacity(pairs.len());
    let mut decl_to_field: Vec<Option<usize>> = vec![None; pairs.len()];
    for (decl_idx, slot) in fields_by_decl.into_iter().enumerate() {
        if let Some(field) = slot {
            decl_to_field[decl_idx] = Some(fields.len());
            fields.push(field);
        }
    }
    // Body-walker emit order over `fields` indices: walk the declaration
    // indices in topological order, keeping only those that survived as
    // host-visible / let-bound fields.
    let field_emit_order: Vec<usize> = emit_order
        .iter()
        .filter_map(|&decl_idx| decl_to_field[decl_idx])
        .collect();

    // Build the host-visible schema from the scalar entries only.
    let schema_fields: Vec<Field> = fields
        .iter()
        .filter_map(|f| match f {
            AnonDictField::Scalar { name, ty } => Some(Field {
                name: name.clone(),
                ty: ty.clone(),
                default: None,
            }),
            // F1b: a cross-region parameter-list field is host-visible —
            // it contributes a real `List<Schema>` / `List<List<scalar>>`
            // record slot (carrying the same canonical element type the
            // host reader / verifier rebuild their layouts from).
            AnonDictField::CrossRegionParamList { name, ty, .. } => Some(Field {
                name: name.clone(),
                ty: ty.clone(),
                default: None,
            }),
            AnonDictField::Closure { .. }
            | AnonDictField::DictStrInt { .. }
            | AnonDictField::ListString { .. } => None,
        })
        .collect();
    let schema = Schema {
        name: MAIN_RETURN_SCHEMA_NAME.to_string(),
        generics: vec![],
        fields: schema_fields,
        is_tuple: false,
    };
    Ok(Some(AnonDictPlan {
        schema,
        fields,
        emit_order: field_emit_order,
    }))
}

/// Collect the host-visible sibling fields a value expression
/// references through a single-segment `&sibling.<name>` / `&root.<name>`
/// reference, restricted to names present in `field_names`. Used to
/// build the anon-Dict-return field dependency graph so forward
/// references can be emitted after their targets and reference cycles
/// surface as a loud `CircularReference`-aligned error.
///
/// Only the reference shape the compiled path lowers contributes an
/// edge: positional/runtime bases, dynamic keys, multi-segment paths
/// and bare `Variable` heads (which name `#main` params, not fields) are
/// deliberately ignored here — they are handled (or capped) elsewhere.
pub(super) fn collect_anon_dict_ref_edges<'a>(
    expr: &'a Expr,
    field_names: &HashSet<&'a str>,
    out: &mut Vec<&'a str>,
) {
    match expr {
        Expr::Reference {
            base: RefBase::Sibling | RefBase::Root,
            path,
        } => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                let n = name.as_str();
                if field_names.contains(n) && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        Expr::Binary(_, a, b) => {
            collect_anon_dict_ref_edges(&a.expr, field_names, out);
            collect_anon_dict_ref_edges(&b.expr, field_names, out);
        }
        Expr::Unary(_, inner) => collect_anon_dict_ref_edges(&inner.expr, field_names, out),
        Expr::Ternary { cond, then, els } => {
            collect_anon_dict_ref_edges(&cond.expr, field_names, out);
            collect_anon_dict_ref_edges(&then.expr, field_names, out);
            collect_anon_dict_ref_edges(&els.expr, field_names, out);
        }
        Expr::List(items) => {
            for n in items {
                collect_anon_dict_ref_edges(&n.expr, field_names, out);
            }
        }
        Expr::FnCall { args, .. } => {
            for a in args {
                collect_anon_dict_ref_edges(&a.value.expr, field_names, out);
            }
        }
        _ => {}
    }
}

/// True when `expr` reads a `#main` parameter through a bare
/// single-segment `Variable([param])`. Walks the same expression shapes
/// the anon-Dict-return scalar / list classifier understands. Used by
/// the forward-reference oracle-agreement gate in [`anon_dict_emit_order`].
pub(super) fn expr_reads_main_param(expr: &Expr, main_param_names: &HashSet<&str>) -> bool {
    match expr {
        Expr::Variable(path) => {
            matches!(path.as_slice(), [TokenKey::String(name, _, _)]
                if main_param_names.contains(name.as_str()))
                // A `d[k]` style index still reads its head identifier;
                // treat any leading param identifier as a param read.
                || matches!(path.first(), Some(TokenKey::String(name, _, _))
                    if main_param_names.contains(name.as_str()))
        }
        Expr::Binary(_, a, b) => {
            expr_reads_main_param(&a.expr, main_param_names)
                || expr_reads_main_param(&b.expr, main_param_names)
        }
        Expr::Unary(_, inner) => expr_reads_main_param(&inner.expr, main_param_names),
        Expr::Ternary { cond, then, els } => {
            expr_reads_main_param(&cond.expr, main_param_names)
                || expr_reads_main_param(&then.expr, main_param_names)
                || expr_reads_main_param(&els.expr, main_param_names)
        }
        Expr::List(items) => items
            .iter()
            .any(|n| expr_reads_main_param(&n.expr, main_param_names)),
        Expr::FnCall { args, .. } => args
            .iter()
            .any(|a| expr_reads_main_param(&a.value.expr, main_param_names)),
        _ => false,
    }
}

/// Connected-component labelling of the undirected anon-Dict reference
/// graph (`field_refs[i]` = the sibling fields field `i` references).
/// Returns a component id per field. Used by the forward-reference
/// oracle-agreement gate so a reference whose component reads a `#main`
/// parameter can be distinguished from a fully param-free one.
pub(super) fn anon_dict_ref_components(n: usize, field_refs: &[Vec<usize>]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut root = x;
        while parent[root] != root {
            root = parent[root];
        }
        // Path compression.
        let mut cur = x;
        while parent[cur] != root {
            let next = parent[cur];
            parent[cur] = root;
            cur = next;
        }
        root
    }
    for (i, refs) in field_refs.iter().enumerate() {
        for &j in refs {
            let ri = find(&mut parent, i);
            let rj = find(&mut parent, j);
            if ri != rj {
                parent[ri] = rj;
            }
        }
    }
    (0..n).map(|i| find(&mut parent, i)).collect()
}

/// Decide the order in which the anon-Dict-return body fields must be
/// classified / emitted so that a `&sibling.<name>` / `&root.<name>`
/// reference always sees its target field already bound — regardless of
/// whether the target is declared earlier (backward) or later (forward)
/// in source.
///
/// `pairs` are the source dict entries in declaration order. The
/// returned vector is a permutation of `0..pairs.len()` (the topological
/// order over the reference-edge graph). A field `i` that references a
/// sibling field `j` produces edge `j → i` (j must be ready first), so
/// Kahn's algorithm emits `j` before `i`.
///
/// The ready queue is drained in ascending declaration index so that a
/// graph with **only backward edges** (every reference targets an
/// earlier field) yields the identity order `0,1,2,…` — preserving the
/// byte-for-byte output of the pre-existing source-ordered lowering. A
/// forward reference is the only thing that perturbs the order.
///
/// A reference cycle (`x: &sibling.y, y: &sibling.x`, or a self
/// reference `x: &sibling.x`) leaves Kahn unable to drain the graph and
/// surfaces as [`LoweringError::CyclicFieldDependency`] — the compiled
/// path's loud analogue of the tree-walk oracle's `CircularReference`.
pub(super) fn anon_dict_emit_order(
    pairs: &[(TokenKey, Node)],
    main_param_names: &HashSet<&str>,
    range: TokenRange,
) -> Result<Vec<usize>, LoweringError> {
    let n = pairs.len();
    let mut name_to_idx: HashMap<&str, usize> = HashMap::with_capacity(n);
    for (i, (key, _)) in pairs.iter().enumerate() {
        if let TokenKey::String(name, _, _) = key {
            // First declaration wins for duplicate keys; the dict
            // builder rejects genuine duplicates elsewhere, and using
            // the first keeps edge resolution deterministic.
            name_to_idx.entry(name.as_str()).or_insert(i);
        }
    }
    let field_names: HashSet<&str> = name_to_idx.keys().copied().collect();

    let mut incoming = vec![0usize; n];
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    // Per-field: reference-bearing (references some sibling), reads a
    // `#main` param directly, and the sibling fields it references (by
    // declaration index). Used by the forward-reference oracle-agreement
    // gate below.
    let mut is_ref_bearing = vec![false; n];
    let mut reads_param = vec![false; n];
    let mut field_refs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, (_, value)) in pairs.iter().enumerate() {
        reads_param[i] = expr_reads_main_param(&value.expr, main_param_names);
        let mut refs: Vec<&str> = Vec::new();
        collect_anon_dict_ref_edges(&value.expr, &field_names, &mut refs);
        is_ref_bearing[i] = !refs.is_empty();
        for r in refs {
            // edge target → this field; skip a self edge so a field that
            // references its own name still surfaces as a cycle (Kahn
            // counts the incoming edge and never drains it).
            if let Some(&j) = name_to_idx.get(r) {
                outgoing[j].push(i);
                incoming[i] += 1;
                field_refs[i].push(j);
            }
        }
    }

    // Forward-reference oracle-agreement gate.
    //
    // The tree-walk oracle resolves anon-Dict field references lazily.
    // A *forward* reference (a field referencing a later-declared
    // sibling) forces the target field's thunk; when that target is
    // itself reference-bearing and its connected reference component
    // reaches a `#main` parameter, the oracle forces it under a scope
    // that has lost the `#main` parameter frame and raises
    // `variable_not_found`. The compiled path *can* evaluate it, but
    // emitting a value where the reference oracle errors would be a
    // silent divergence — so we cap that exact shape loudly. Forward
    // references whose target is a non-reference leaf (`x: a + b`), and
    // reference chains whose whole connected component is `#main`-param-
    // free, both resolve consistently four-way and are admitted.
    let component = anon_dict_ref_components(n, &field_refs);
    let mut component_reads_param: HashSet<usize> = HashSet::new();
    for (i, &reads) in reads_param.iter().enumerate() {
        if reads {
            component_reads_param.insert(component[i]);
        }
    }
    for (i, refs) in field_refs.iter().enumerate() {
        for &j in refs {
            // forward reference: the target is declared after the
            // referencing field.
            if j > i && is_ref_bearing[j] && component_reads_param.contains(&component[i]) {
                let (fname, frange) = match &pairs[i].0 {
                    TokenKey::String(s, r, _) => (s.clone(), *r),
                    other => (format!("{other:?}"), range),
                };
                return Err(cap!(
                    "anon_dict_emit_order.forward_ref_through_param",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "AnonDictReturn(field `{}`: forward reference into a reference-bearing \
                             field whose component reads a `#main` parameter — the tree-walk \
                             oracle cannot resolve this shape consistently)",
                            fname
                        ),
                        range: frange,
                    }
                ));
            }
        }
    }

    // Kahn's algorithm with an ascending-index ready set so backward-only
    // graphs reproduce declaration order exactly.
    let mut ready: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for (i, &deg) in incoming.iter().enumerate() {
        if deg == 0 {
            ready.insert(i);
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while let Some(&i) = ready.iter().next() {
        ready.remove(&i);
        order.push(i);
        for &j in &outgoing[i] {
            incoming[j] -= 1;
            if incoming[j] == 0 {
                ready.insert(j);
            }
        }
    }
    if order.len() != n {
        let cycle = find_anon_dict_ref_cycle(pairs, &outgoing, &incoming);
        return Err(cap!(
            "anon_dict_emit_order.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                cycle,
                range,
            }
        ));
    }
    Ok(order)
}

/// Build a representative reference-cycle path (field names, first name
/// repeated at the end) for the anon-Dict-return diagnostic. The caller
/// has already proven a cycle exists (Kahn could not drain the graph).
pub(super) fn find_anon_dict_ref_cycle(
    pairs: &[(TokenKey, Node)],
    outgoing: &[Vec<usize>],
    incoming: &[usize],
) -> Vec<String> {
    let n = outgoing.len();
    let field_name = |i: usize| match &pairs[i].0 {
        TokenKey::String(name, _, _) => name.clone(),
        other => format!("{other:?}"),
    };
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
            return cycle.into_iter().map(field_name).collect();
        }
    }
    Vec::new()
}

/// True when `t` is a single-segment `Dict` with no generic
/// arguments — the surface [`anon_dict_return_plan`] hangs the W7
/// anon-Dict-return lifting off. Multi-segment paths (`pkg.Dict`),
/// `Dict<K, V>` with explicit generics, and variant-style nodes are
/// out of scope.
pub(super) fn type_node_is_bare_dict(t: &TypeNode) -> bool {
    t.path.len() == 1 && t.path[0] == "Dict" && t.generics.is_empty() && t.variant_fields.is_none()
}

/// Statically derive a [`TypeRepr`] for a scalar dict field in the
/// W7 anon-Dict-return path. Today's surface intentionally stays
/// minimal — anything beyond the supported shapes surfaces as
/// `UnsupportedExpr` so the future inference work has a clear edge
/// rather than a half-implemented fallback.
///
/// Supported value shapes:
/// * `Expr::Int` / `Expr::Float` / `Expr::Bool` / `Expr::String`.
/// * `Expr::Variable([name])` where `name` resolves to a `#main`
///   parameter with a known scalar IR type.
/// * `Expr::FnCall { path: [name], args }` where `name` was already
///   classified as a closure field — the field type is the closure's
///   declared return type.
/// * `Expr::Binary(Add|Sub|Mul|Div|Mod, lhs, rhs)` over integers /
///   floats — propagates the operand type (Int + Int → Int).
pub(super) fn classify_anon_dict_scalar_field(
    expr: &Expr,
    range: TokenRange,
    main_param_tys: &HashMap<&str, IrType>,
    closure_field_sigs: &HashMap<&str, (Vec<IrType>, IrType)>,
    dict_field_names: &HashSet<&str>,
    scalar_field_irts: &HashMap<&str, IrType>,
    field_name: &str,
) -> Result<TypeRepr, LoweringError> {
    let irt = classify_anon_dict_scalar_field_irt(
        expr,
        range,
        main_param_tys,
        closure_field_sigs,
        dict_field_names,
        scalar_field_irts,
        field_name,
    )?;
    ir_type_to_type_repr(irt).ok_or_else(|| {
        cap!(
            "classify_anon_dict_scalar_field.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "AnonDictReturn(field `{}`: non-scalar inferred IR type {:?})",
                    field_name, irt,
                ),
                range,
            }
        )
    })
}

/// W5-P1: classify a `{str: int}` dict literal sitting on the RHS of
/// an anon-Dict-return `#internal` field. Returns the `(key, value)`
/// entry set in source declaration order when every entry is a
/// string-key / integer-literal pair; any other entry shape (non-string
/// key, spread, non-Int value, nested dict) surfaces `UnsupportedExpr`
/// so the P1 surface stays honest — value/key-type widening is P2/P3.
pub(super) fn classify_anon_dict_str_int_field(
    pairs: &[(TokenKey, Node)],
    range: TokenRange,
    field_name: &str,
) -> Result<Vec<(String, i64)>, LoweringError> {
    let mut entries: Vec<(String, i64)> = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        let TokenKey::String(key_name, _, _) = key else {
            return Err(cap!(
                "classify_anon_dict_str_int_field.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(dict field `{}`: non-string dict key)",
                        field_name
                    ),
                    range,
                }
            ));
        };
        let Expr::Int(v) = &*value.expr else {
            return Err(cap!("classify_anon_dict_str_int_field.unsupported_expr.2", LoweringError::UnsupportedExpr {
                kind: format!(
                    "AnonDictReturn(dict field `{}`: value for key `{}` is `{}`, only Int literals supported in P1)",
                    field_name,
                    key_name,
                    value.expr.kind()
                ),
                range: value.range,
            }));
        };
        entries.push((key_name.clone(), *v));
    }
    Ok(entries)
}

/// W5-P4: classify a `["a", "b", ...]` list-of-string literal sitting on
/// the RHS of an anon-Dict-return `#internal` field (the `keys` field of
/// the dict-probe workload). Returns the element set in source order when
/// every element is a String literal; any other element shape (non-String
/// literal, nested list, spread) surfaces `UnsupportedExpr` so the
/// surface stays honest — non-String list fields are out of scope here.
pub(super) fn classify_anon_dict_list_string_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
) -> Result<Vec<String>, LoweringError> {
    let mut elements: Vec<String> = Vec::with_capacity(items.len());
    for node in items {
        let Expr::String(s) = &*node.expr else {
            return Err(cap!(
                "classify_anon_dict_list_string_field.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                    "AnonDictReturn(list field `{}`: element `{}`, only String literals supported)",
                    field_name,
                    node.expr.kind()
                ),
                    range,
                }
            ));
        };
        elements.push(s.clone());
    }
    Ok(elements)
}

/// True when a dict-field value node carries a `#internal` pragma. The
/// anon-Dict-return path uses this to keep `#internal` collection /
/// closure fields off the host-visible return surface (matching the
/// tree-walk oracle, which also drops them) while still marshalling
/// every non-`#internal` field.
pub(super) fn node_marked_internal(node: &Node) -> bool {
    node.directives
        .iter()
        .any(|d| d.name == relon_parser::directive::INTERNAL)
}

/// Classify a host-visible list-literal anon-Dict-return field into a
/// `List<elem>` [`TypeRepr`] by sniffing the element shape. Mirrors the
/// `Expr::List` arm of [`lower_expr`] (which picks `ConstListInt` /
/// `ConstListFloat` / `ConstListBool` / `ConstListString` from the same
/// first-element type). Only homogeneous scalar / String element lists
/// are accepted; empty lists (no element to type), mixed-type lists, and
/// lists of lists / schemas surface a loud error so an unmarshallable
/// field never silently disappears from the host output.
pub(super) fn classify_anon_dict_list_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
    resolver: &SchemaResolver<'_>,
    main_param_tys: &HashMap<&str, IrType>,
) -> Result<TypeRepr, LoweringError> {
    let Some(first) = items.first() else {
        return Err(cap!(
            "classify_anon_dict_list_field.unsupported_field_type.1",
            LoweringError::UnsupportedFieldType {
                schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                field: field_name.to_string(),
                ty: "empty list field — element type cannot be inferred for the return marshaller"
                    .to_string(),
                range,
            }
        ));
    };
    if let Some(variant_list_ty) =
        classify_anon_dict_variant_list_field(items, range, field_name, main_param_tys)?
    {
        return Ok(variant_list_ty);
    }
    if let Some(enum_list_ty) =
        classify_anon_dict_enum_list_field(items, range, field_name, resolver)?
    {
        return Ok(enum_list_ty);
    }
    let element = match &*first.expr {
        Expr::Int(_) => TypeRepr::Int,
        Expr::Float(_) => TypeRepr::Float,
        Expr::Bool(_) => TypeRepr::Bool,
        Expr::String(_) => TypeRepr::String,
        other => {
            return Err(cap!(
                "classify_anon_dict_list_field.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                    "list element `{}` — only homogeneous List<Int/Float/Bool/String> fields are \
                     marshalled in anon-Dict returns",
                    other.kind()
                ),
                    range,
                }
            ));
        }
    };
    // Enforce homogeneity up front so a mixed list (which `lower_expr`
    // would reject deeper, or worse mis-type) fails here with a precise
    // field name rather than a generic codegen error.
    for node in &items[1..] {
        let ok = matches!(
            (&element, &*node.expr),
            (TypeRepr::Int, Expr::Int(_))
                | (TypeRepr::Float, Expr::Float(_))
                | (TypeRepr::Bool, Expr::Bool(_))
                | (TypeRepr::String, Expr::String(_))
        );
        if !ok {
            return Err(cap!(
                "classify_anon_dict_list_field.unsupported_field_type.3",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous list field (expected all {element:?} elements, found `{}`)",
                        node.expr.kind()
                    ),
                    range,
                }
            ));
        }
    }
    Ok(TypeRepr::List {
        element: Box::new(element),
    })
}

pub(super) fn classify_anon_dict_enum_list_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<TypeRepr>, LoweringError> {
    let Some(first) = items.first() else {
        return Ok(None);
    };
    let Some((enum_name, first_variant)) = enum_variant_literal_path(first.expr.as_ref()) else {
        return Ok(None);
    };
    let Some(def) = resolver.resolve(&enum_name) else {
        return Ok(None);
    };
    if def.variants.is_empty() {
        return Ok(None);
    }
    if !def.variants.iter().any(|v| v.name == first_variant) {
        return Err(cap!(
            "classify_anon_dict_enum_list_field.unsupported_field_type.1",
            LoweringError::UnsupportedFieldType {
                schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                field: field_name.to_string(),
                ty: format!("enum `{enum_name}` has no variant `{first_variant}`"),
                range: first.range,
            }
        ));
    }

    for node in &items[1..] {
        let Some((item_enum, item_variant)) = enum_variant_literal_path(node.expr.as_ref()) else {
            return Err(cap!(
                "classify_anon_dict_enum_list_field.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous list field: expected `{enum_name}` enum variants, found `{}`",
                        node.expr.kind()
                    ),
                    range: node.range,
                }
            ));
        };
        if item_enum != enum_name {
            return Err(cap!(
                "classify_anon_dict_enum_list_field.unsupported_field_type.3",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous enum list field: expected `{enum_name}`, found `{item_enum}`"
                    ),
                    range: node.range,
                }
            ));
        }
        if !def.variants.iter().any(|v| v.name == item_variant) {
            return Err(cap!(
                "classify_anon_dict_enum_list_field.unsupported_field_type.4",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!("enum `{enum_name}` has no variant `{item_variant}`"),
                    range: node.range,
                }
            ));
        }
    }

    let mut stack: Vec<&str> = Vec::new();
    let enum_ty = canonical_enum_from_def(def, resolver, &mut stack, range)?;
    Ok(Some(TypeRepr::List {
        element: Box::new(enum_ty),
    }))
}

/// Classify a host-visible anon-Dict-return list-literal field whose
/// elements are built-in `Option` / `Result` variant constructors into a
/// `List<Option<T>>` / `List<Result<T, E>>` [`TypeRepr`]. The named-enum
/// counterpart ([`classify_anon_dict_enum_list_field`]) cannot reach these
/// because `Option` / `Result` are prelude sum types — `resolver.resolve`
/// returns `None` for them — so they fell through to the homogeneous-scalar
/// classifier and capped (`classify_anon_dict_list_field.unsupported_field_type.2`).
///
/// Once a concrete element type is recovered the field becomes a normal
/// `List<variant>` whose lowering already exists: the body walker routes the
/// list literal through the `variant_list_literal_for_type` pointer-array of
/// tagged variant records in `lower_dict_field_value`, returned via the
/// in-place region-walk ABI (verifier-gated). So the only missing piece was
/// recovering the payload type the declared-schema path gets for free from the
/// annotation.
///
/// The inner type is inferred by sniffing the scalar payload of a
/// payload-bearing variant (`Some { value }` / `Ok { value }` / `Err { error }`),
/// requiring homogeneity across the list. Shapes whose inner type cannot be
/// proven from the literal alone are left capped (returned as `Ok(None)` so the
/// caller's own loud cap fires, or a precise `Err` for an outright malformed
/// list):
///   * an all-`None` `Option` list (no `Some` payload to type the inner),
///   * a `Result` list missing either the `Ok` or the `Err` arm,
///   * a non-scalar / non-param payload expression,
///   * a heterogeneous payload type.
pub(super) fn classify_anon_dict_variant_list_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
    main_param_tys: &HashMap<&str, IrType>,
) -> Result<Option<TypeRepr>, LoweringError> {
    let Some(first) = items.first() else {
        return Ok(None);
    };
    let Some((enum_name, _)) = enum_variant_literal_path(first.expr.as_ref()) else {
        return Ok(None);
    };
    // Only the two built-in sum types are handled here; named user enums
    // stay on the resolver-backed path.
    let kind = match enum_name.as_str() {
        "Option" => VariantListKind::Option,
        "Result" => VariantListKind::Result,
        _ => return Ok(None),
    };

    // Accumulated payload scalar types per arm. `None` until a
    // payload-bearing element pins it down.
    let mut some_ty: Option<TypeRepr> = None; // Option.Some / Result.Ok
    let mut err_ty: Option<TypeRepr> = None; // Result.Err

    for node in items {
        let Expr::VariantCtor {
            enum_path,
            variant,
            body,
        } = &*node.expr
        else {
            return Err(cap!(
                "classify_anon_dict_variant_list_field.unsupported_field_type.1",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "list element `{}` — expected a `{enum_name}` variant constructor",
                        node.expr.kind()
                    ),
                    range: node.range,
                }
            ));
        };
        if enum_path.join(".") != enum_name {
            return Err(cap!(
                "classify_anon_dict_variant_list_field.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous variant list field: expected `{enum_name}`, found `{}`",
                        enum_path.join(".")
                    ),
                    range: node.range,
                }
            ));
        }
        // Determine which payload slot this variant feeds and its key.
        let payload_slot = match (kind, variant.as_str()) {
            (VariantListKind::Option, "None") => None,
            (VariantListKind::Option, "Some") => Some(("value", false)),
            (VariantListKind::Result, "Ok") => Some(("value", false)),
            (VariantListKind::Result, "Err") => Some(("error", true)),
            (_, other) => {
                return Err(cap!(
                    "classify_anon_dict_variant_list_field.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: field_name.to_string(),
                        ty: format!("`{enum_name}` has no variant `{other}`"),
                        range: node.range,
                    }
                ));
            }
        };
        let Some((key, is_err)) = payload_slot else {
            // Payload-free variant (`None`) — nothing to type.
            continue;
        };
        let payload_node =
            variant_payload_node(variant_body_pairs(body, node.range)?, key, node.range)?;
        let payload_ty =
            variant_payload_scalar_ty(&payload_node.expr, main_param_tys).ok_or_else(|| {
                cap!(
                    "classify_anon_dict_variant_list_field.unsupported_field_type.4",
                    LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: field_name.to_string(),
                        ty: format!(
                            "`{enum_name}.{variant}` payload `{}` is not a scalar literal or \
                             scalar `#main` parameter — cannot type the variant list element",
                            payload_node.expr.kind()
                        ),
                        range: payload_node.range,
                    }
                )
            })?;
        let slot = if is_err { &mut err_ty } else { &mut some_ty };
        match slot {
            Some(existing) if *existing != payload_ty => {
                return Err(cap!(
                    "classify_anon_dict_variant_list_field.unsupported_field_type.5",
                    LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: field_name.to_string(),
                        ty: format!(
                            "heterogeneous `{enum_name}` payload type: {existing:?} vs {payload_ty:?}"
                        ),
                        range: payload_node.range,
                    }
                ));
            }
            Some(_) => {}
            None => *slot = Some(payload_ty),
        }
    }

    match kind {
        VariantListKind::Option => {
            // All-`None` cannot pin the inner type from the literal alone.
            let Some(inner) = some_ty else {
                return Ok(None);
            };
            Ok(Some(TypeRepr::List {
                element: Box::new(TypeRepr::Option {
                    inner: Box::new(inner),
                }),
            }))
        }
        VariantListKind::Result => {
            // Need both arms present to type `Result<T, E>` fully.
            let (Some(ok), Some(err)) = (some_ty, err_ty) else {
                return Ok(None);
            };
            let _ = range;
            Ok(Some(TypeRepr::List {
                element: Box::new(TypeRepr::Result {
                    ok: Box::new(ok),
                    err: Box::new(err),
                }),
            }))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum VariantListKind {
    Option,
    Result,
}

/// Recover the scalar [`TypeRepr`] of a variant payload expression. Only
/// shapes whose type is provable at classify time are accepted: scalar
/// literals and a bare `#main` scalar parameter reference. Anything else
/// (computed expressions, nested collections, schemas) returns `None` so the
/// caller caps loudly rather than guessing a layout.
pub(super) fn variant_payload_scalar_ty(
    expr: &Expr,
    main_param_tys: &HashMap<&str, IrType>,
) -> Option<TypeRepr> {
    match expr {
        Expr::Int(_) => Some(TypeRepr::Int),
        Expr::Float(_) => Some(TypeRepr::Float),
        Expr::Bool(_) => Some(TypeRepr::Bool),
        Expr::String(_) => Some(TypeRepr::String),
        Expr::Variable(path) => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                return match main_param_tys.get(name.as_str())? {
                    IrType::I64 => Some(TypeRepr::Int),
                    IrType::F64 => Some(TypeRepr::Float),
                    IrType::Bool => Some(TypeRepr::Bool),
                    IrType::String => Some(TypeRepr::String),
                    _ => None,
                };
            }
            None
        }
        _ => None,
    }
}

pub(super) fn enum_variant_literal_path(expr: &Expr) -> Option<(String, String)> {
    match expr {
        Expr::Variable(path) | Expr::FnCall { path, .. } => enum_variant_literal_token_path(path),
        Expr::VariantCtor {
            enum_path, variant, ..
        } => {
            if enum_path.is_empty() {
                None
            } else {
                Some((enum_path.join("."), variant.clone()))
            }
        }
        _ => None,
    }
}

pub(super) fn enum_variant_literal_token_path(path: &[TokenKey]) -> Option<(String, String)> {
    let mut parts = Vec::with_capacity(path.len());
    for seg in path {
        match seg {
            TokenKey::String(s, _, _) => parts.push(s.clone()),
            _ => return None,
        }
    }
    if parts.len() < 2 {
        return None;
    }
    let variant = parts.pop()?;
    Some((parts.join("."), variant))
}

pub(super) fn classify_anon_dict_scalar_field_irt(
    expr: &Expr,
    range: TokenRange,
    main_param_tys: &HashMap<&str, IrType>,
    closure_field_sigs: &HashMap<&str, (Vec<IrType>, IrType)>,
    dict_field_names: &HashSet<&str>,
    scalar_field_irts: &HashMap<&str, IrType>,
    field_name: &str,
) -> Result<IrType, LoweringError> {
    match expr {
        Expr::Int(_) => Ok(IrType::I64),
        Expr::Float(_) => Ok(IrType::F64),
        Expr::Bool(_) => Ok(IrType::Bool),
        Expr::String(_) => Ok(IrType::String),
        // R10/R13: a static sibling/root reference to another
        // host-visible field. At the entry-level dict (which IS the
        // document root) `&sibling.<name>` and `&root.<name>` resolve to
        // the same field, so both bases classify here. Classification
        // runs in topological order over the reference edges, so the
        // target field's type is in `scalar_field_irts` whether it is
        // declared earlier (backward) or later (forward). Only a single
        // static `String` trailing segment naming a host-visible scalar
        // *or list* field is accepted; positional bases
        // (Uncle/Prev/Next/Index/This), dynamic keys and multi-segment
        // paths fall through to the loud cap below.
        Expr::Reference {
            base: RefBase::Sibling | RefBase::Root,
            path,
        } => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(t) = scalar_field_irts.get(name.as_str()) {
                    return Ok(*t);
                }
            }
            Err(cap!(
                "classify_anon_dict_scalar_field_irt.reference_unresolved",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(field `{}`: sibling/root reference {:?} \
                         does not name a host-visible field)",
                        field_name, path
                    ),
                    range,
                }
            ))
        }
        Expr::Variable(path) => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(t) = main_param_tys.get(name.as_str()) {
                    return Ok(*t);
                }
            }
            // W5-P3: `d[k]` — a sibling `{String -> Int}` dict field
            // indexed by a String key — classifies to the dict's `Int`
            // value type. The head must name a known dict field and the
            // single trailing segment must be a `Dynamic` (bracket)
            // index; `lower_dict_string_index` emits the actual probe.
            if let [TokenKey::String(name, _, _), TokenKey::Dynamic(_, optional)] = path.as_slice()
            {
                if !optional && dict_field_names.contains(name.as_str()) {
                    return Ok(IrType::I64);
                }
            }
            Err(cap!(
                "classify_anon_dict_scalar_field_irt.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(field `{}`: cannot classify Variable({:?}))",
                        field_name, path
                    ),
                    range,
                }
            ))
        }
        Expr::FnCall { path, .. } => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some((_, ret_ty)) = closure_field_sigs.get(name.as_str()) {
                    return Ok(*ret_ty);
                }
            }
            // W5-P4: `result: list.sum(range(...)[.map|.filter]*)` — the
            // dict-probe workload's host-visible field. `list.sum` over a
            // range pipeline always yields an `Int` accumulator (the
            // peephole `emit_range_pipeline_loop` enforces an Int-valued
            // element and rejects otherwise), so classify the field as
            // I64; the actual loop + capture is lowered in `lower_expr`.
            if let [TokenKey::String(head, _, _), TokenKey::String(method, _, _)] = path.as_slice()
            {
                if head == "list" && method == "sum" {
                    return Ok(IrType::I64);
                }
            }
            Err(cap!(
                "classify_anon_dict_scalar_field_irt.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(field `{}`: cannot classify FnCall({:?}) — \
                     only calls into previously-classified closure fields are \
                     supported at this surface)",
                        field_name, path
                    ),
                    range,
                }
            ))
        }
        Expr::Binary(_, lhs, rhs) => {
            // Conservative arithmetic propagation: both sides must
            // resolve to the same scalar IR type. Mixed Int/Float
            // promotes to Float (mirroring the runtime). String
            // concat (`+`) is recognised when both sides are String.
            let lt = classify_anon_dict_scalar_field_irt(
                &lhs.expr,
                lhs.range,
                main_param_tys,
                closure_field_sigs,
                dict_field_names,
                scalar_field_irts,
                field_name,
            )?;
            let rt = classify_anon_dict_scalar_field_irt(
                &rhs.expr,
                rhs.range,
                main_param_tys,
                closure_field_sigs,
                dict_field_names,
                scalar_field_irts,
                field_name,
            )?;
            match (lt, rt) {
                (IrType::I64, IrType::I64) => Ok(IrType::I64),
                (IrType::F64, IrType::F64)
                | (IrType::F64, IrType::I64)
                | (IrType::I64, IrType::F64) => Ok(IrType::F64),
                (IrType::Bool, IrType::Bool) => Ok(IrType::Bool),
                (IrType::String, IrType::String) => Ok(IrType::String),
                _ => Err(cap!(
                    "classify_anon_dict_scalar_field_irt.unsupported_expr.3",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                        "AnonDictReturn(field `{}`: binary with mixed scalar types {:?} / {:?})",
                        field_name, lt, rt
                    ),
                        range,
                    }
                )),
            }
        }
        _ => Err(cap!(
            "classify_anon_dict_scalar_field_irt.unsupported_expr.4",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "AnonDictReturn(field `{}`: unsupported value shape `{}`)",
                    field_name,
                    expr.kind()
                ),
                range,
            }
        )),
    }
}

/// Reverse of `type_repr_to_ir_type` for the host-visible anon-Dict
/// field types. Covers the scalar / String leaves plus the marshalled
/// scalar-element list types — the latter so a `&sibling.<list>` /
/// `&root.<list>` reference field classifies to the same `List<...>`
/// type as the field it aliases. Returns `None` for IR types that have
/// no anon-Dict-return canonical form (schemas, cross-region pointer
/// lists, closures, dicts).
pub(super) fn ir_type_to_type_repr(t: IrType) -> Option<TypeRepr> {
    let list = |element: TypeRepr| {
        Some(TypeRepr::List {
            element: Box::new(element),
        })
    };
    match t {
        IrType::I64 => Some(TypeRepr::Int),
        IrType::F64 => Some(TypeRepr::Float),
        IrType::Bool => Some(TypeRepr::Bool),
        IrType::String => Some(TypeRepr::String),
        IrType::Unit => Some(TypeRepr::Unit),
        IrType::ListInt => list(TypeRepr::Int),
        IrType::ListFloat => list(TypeRepr::Float),
        IrType::ListBool => list(TypeRepr::Bool),
        IrType::ListString => list(TypeRepr::String),
        _ => None,
    }
}

/// Phase F.2 (W7): body walker for the anon-Dict-return path. Walks
/// the dict literal in declaration order; each entry is either a
/// closure-field let-binding (no host-visible store) or a scalar
/// field store into the root record.
///
/// Closure fields are pre-registered as `IrType::Closure` let-locals
/// **before** their body lowers — this gives recursive self-calls
/// (W7's `fib(k - 1)` inside `fib`'s body) a stable let slot to
/// `LetGet` off and consume via `Op::CallClosure`.
pub(super) fn lower_anon_dict_body(
    plan: &AnonDictPlan,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Build a name → user-supplied Node map so we can pull each
    // classified plan field's value back out of the source dict.
    let mut user_values: HashMap<&str, &Node> = HashMap::new();
    for (key, value) in dict_pairs {
        if let TokenKey::String(name, _, _) = key {
            user_values.insert(name.as_str(), value);
        }
    }

    // Resolve each host-visible scalar / cross-region field's layout
    // slot by name. The layout walks `schema.fields` in declaration
    // order; looking the slot up by name (rather than a running index)
    // lets the body walker emit fields in topological order — needed so
    // a forward `&sibling` / `&root` reference's target field is already
    // bound — without disturbing the record offset each value stores to.
    let layout_field_by_name = |name: &str| layout.fields.iter().find(|f| f.name == name);

    // R13: emit fields in topological order over their reference edges
    // (see `AnonDictPlan::emit_order`). Backward-only / reference-free
    // bodies keep declaration order, so the pre-existing byte output is
    // unchanged.
    for &field_idx in &plan.emit_order {
        let plan_field = &plan.fields[field_idx];
        match plan_field {
            AnonDictField::Closure {
                name,
                param_tys,
                ret_ty,
                concat_coercible,
            } => {
                let value = user_values.get(name.as_str()).copied().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(missing source value for closure field `{}`)",
                                name
                            ),
                            range: TokenRange::default(),
                        }
                    )
                })?;
                // Pre-allocate the let-idx the closure handle will
                // land in. Registered before the body lowers so a
                // recursive `Variable(name)` inside the body resolves
                // to `LetGet { idx, Closure }`.
                let let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: let_idx,
                    ty: IrType::Closure,
                    schema_brand: None,
                    type_repr: None,
                });
                ctx.closure_let_signatures
                    .insert(let_idx, (param_tys.clone(), *ret_ty));
                if concat_coercible.iter().any(|&c| c) {
                    ctx.closure_concat_coercible
                        .insert(let_idx, concat_coercible.clone());
                }

                // Lower the closure body — pushes `IrType::Closure` on
                // top of the vstack and appends the lambda to
                // `ctx.lambda_funcs`.
                lower_closure_as_value(&value.expr, value.range, param_tys, *ret_ty, ctx)?;

                // Stash the handle into the pre-allocated let-local.
                let popped = ctx.tstack.pop().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.2",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(closure field `{}` produced no value)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                debug_assert_eq!(popped, IrType::Closure);
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: let_idx,
                        ty: IrType::Closure,
                    },
                    range: value.range,
                });
            }
            AnonDictField::DictStrInt { name, entries } => {
                // W5-P1: materialise the `{str:int}` dict into the const
                // pool via `Op::ConstDict` and stash the arena pointer
                // into a fresh `IrType::Dict` internal let-local. This
                // is the construction + capture half; the read half
                // (`DictGetByStringKey`) is a P3 follow-up, so the
                // let-local is value-only for now.
                let let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: let_idx,
                    ty: IrType::Dict,
                    schema_brand: None,
                    type_repr: None,
                });
                let dict_idx = ctx.const_intern.borrow_mut().alloc_dict_idx();
                ctx.out.push(TaggedOp {
                    op: Op::ConstDict {
                        idx: dict_idx,
                        entries: entries.clone(),
                    },
                    range: TokenRange::default(),
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: let_idx,
                        ty: IrType::Dict,
                    },
                    range: TokenRange::default(),
                });
            }
            AnonDictField::ListString { name, elements } => {
                // W5-P4: materialise the `["a", ...]` list into the const
                // pool via `Op::ConstListString` and stash the arena
                // pointer into a fresh `IrType::ListString` internal
                // let-local. Captured by a later sibling `result` field
                // (the map-loop body `keys[i % 10]`); the let-binding is
                // registered before `result` lowers so `Variable(keys)`
                // resolves to `LetGet { idx, ListString }`.
                let let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: let_idx,
                    ty: IrType::ListString,
                    schema_brand: None,
                    type_repr: None,
                });
                let list_idx = ctx.const_intern.borrow_mut().alloc_list_string_idx();
                ctx.out.push(TaggedOp {
                    op: Op::ConstListString {
                        idx: list_idx,
                        elements: elements.clone(),
                    },
                    range: TokenRange::default(),
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: let_idx,
                        ty: IrType::ListString,
                    },
                    range: TokenRange::default(),
                });
            }
            AnonDictField::Scalar { name, ty } => {
                let value = user_values.get(name.as_str()).copied().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.3",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(missing source value for scalar field `{}`)",
                                name
                            ),
                            range: TokenRange::default(),
                        }
                    )
                })?;
                let expected_ir = type_repr_to_ir_type(ty)?;
                let is_variant_list_literal = variant_list_literal_for_type(ty, &value.expr);
                // Same pointer-array-list provenance guard as the
                // top-level and branded-struct return paths. `List<String>`
                // still needs the const-pool path; `List<Enum>` source
                // literals are constructed directly in the output tail by
                // `BuildPointerList`, so they are also safe here.
                if pointer_array_list_ir_type(expected_ir)
                    && !pointer_array_list_source_is_const_pool(&value.expr)
                    && !is_variant_list_literal
                {
                    return Err(cap!(
                        "lower_anon_dict_body.unsupported_field_type.1",
                        LoweringError::UnsupportedFieldType {
                            schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                            field: name.clone(),
                            ty: format!(
                            "{expected_ir:?} sourced from `{}` — pointer-array list fields are                              only marshalled from in-source list literals",
                            value.expr.kind()
                        ),
                            range: value.range,
                        }
                    ));
                }
                if is_variant_list_literal {
                    lower_value_as_type(ty, value, ctx)?;
                } else {
                    lower_expr(&value.expr, value.range, ctx)?;
                }
                let top = ctx.tstack.pop().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.4",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(scalar field `{}` produced no value)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                if top.wasm_slot() != expected_ir.wasm_slot() {
                    return Err(cap!(
                        "lower_anon_dict_body.unsupported_field_type.2",
                        LoweringError::UnsupportedFieldType {
                            schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                            field: name.clone(),
                            ty: format!("expected {:?}, got {:?}", expected_ir, top),
                            range: value.range,
                        }
                    ));
                }
                // R10: stash this scalar field's value in an internal
                // let-local so a *later* sibling field can read it back
                // with `&sibling.<name>` / `&root.<name>` (the entry dict
                // IS the document root, so both bases resolve to the same
                // field). The let holds the field's natural scalar value
                // (the same value `lower_variable` would `LetGet`); the
                // pointer-indirect tail-emit, if any, happens below on the
                // value we re-load — so the reference and the stored field
                // observe a bit-identical value. Source order makes the
                // binding visible only to fields declared after it, which
                // is exactly the backward-only contract the reference arm
                // in `lower_expr` enforces. Registered for every
                // host-visible scalar field (including String); a forward
                // or positional reference simply never finds it and caps.
                let field_let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: field_let_idx,
                    ty: expected_ir,
                    schema_brand: None,
                    type_repr: None,
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: field_let_idx,
                        ty: expected_ir,
                    },
                    range: value.range,
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetGet {
                        idx: field_let_idx,
                        ty: expected_ir,
                    },
                    range: value.range,
                });
                let layout_field = layout_field_by_name(name).ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.5",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(scalar field `{}`: no matching layout slot)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                debug_assert_eq!(&layout_field.name, name);
                // Pointer-indirect fields (String / List<scalar> /
                // List<String>) push an *absolute* arena address from
                // `lower_expr` (a `ConstString` / `ConstList*` record or
                // a `Load*Ptr` param). They must be copied into the
                // return buffer's tail area first — the fixed-area slot
                // stores a *buffer-relative* offset, not the absolute
                // address. This mirrors the branded-dict path
                // (`lower_dict_field_value`): emit
                // `EmitTailRecordFromAbsoluteAddr { ty }` to perform the
                // copy, then store the resulting i32 offset. Without
                // this the slot held the raw arena pointer and the host
                // reader dereferenced garbage → a silent empty
                // String / List (the W7-anon-dict mis-compile).
                let store_ty = if is_variant_list_literal {
                    expected_ir
                } else if pointer_indirect_ir_type(expected_ir) {
                    ctx.out.push(TaggedOp {
                        op: Op::EmitTailRecordFromAbsoluteAddr { ty: expected_ir },
                        range: value.range,
                    });
                    IrType::I32
                } else {
                    expected_ir
                };
                ctx.out.push(TaggedOp {
                    op: Op::StoreFieldAtRecord {
                        record_local_idx: record_local,
                        offset: layout_field.offset as u32,
                        ty: store_ty,
                    },
                    range: value.range,
                });
            }
            AnonDictField::CrossRegionParamList { name, ty, .. } => {
                // F1b: cross-region object field. The value is a `#main`
                // parameter identity of `List<Schema>` / `List<List<scalar>>`
                // type whose data lives in the *input* region; the object
                // head is in the *output* region. We do NOT copy the data
                // into the output tail (that is exactly what the old
                // `EmitTailRecordFromAbsoluteAddr` cap rejected as
                // unsupported for these pointer-array element lists, and a
                // copy would also lose the cross-region link).
                //
                // Under the F1 arena-absolute slot convention every pointer
                // slot stores an arena-absolute u32 offset. `lower_expr`
                // over the parameter identity emits `LoadListSchemaPtr` /
                // `LoadListListPtr`, which post-F1 push the parameter list
                // root header's arena-absolute offset (the input marshaller
                // baked `in_ptr` into the slot). We store that offset
                // directly into the object's field slot via
                // `StoreFieldAtRecord { ty: ListSchema / ListList }`. The
                // host's multi-region verifier (which the object positive-
                // `bytes_written` path now always runs) reads the slot,
                // classifies the offset into the input region, bounds-checks
                // the whole reachable graph, and only then does the reader
                // follow it cross-region — bit-equal to the tree-walk
                // oracle. An offset that classifies to no region / runs off
                // its region is a loud verifier error; the decode never runs.
                let value = user_values.get(name.as_str()).copied().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.6",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(missing source value for cross-region field `{}`)",
                                name
                            ),
                            range: TokenRange::default(),
                        }
                    )
                })?;
                let expected_ir = type_repr_to_ir_type(ty)?;
                // F1b admitted `ListSchema` / `ListList`; F3 widens to the
                // pointer-array `ListString` and the inline-fixed scalar
                // lists (`ListInt` / `ListFloat` / `ListBool`). Every one is
                // a cross-region list whose param root slot stores an
                // arena-absolute offset (the value `lower_expr` pushes via
                // the matching `LoadList*Ptr`).
                debug_assert!(matches!(
                    expected_ir,
                    IrType::ListSchema
                        | IrType::ListList
                        | IrType::ListString
                        | IrType::ListInt
                        | IrType::ListFloat
                        | IrType::ListBool
                ));
                lower_expr(&value.expr, value.range, ctx)?;
                let top = ctx.tstack.pop().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.7",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(cross-region field `{}` produced no value)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                if top != expected_ir {
                    return Err(cap!(
                        "lower_anon_dict_body.unsupported_field_type.3",
                        LoweringError::UnsupportedFieldType {
                            schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                            field: name.clone(),
                            ty: format!(
                                "cross-region field expected {:?}, got {:?}",
                                expected_ir, top
                            ),
                            range: value.range,
                        }
                    ));
                }
                let layout_field = layout_field_by_name(name).ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.8",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(cross-region field `{}`: no matching layout slot)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                debug_assert_eq!(&layout_field.name, name);
                // Store the parameter list root's arena-absolute offset
                // straight into the slot — no tail copy, the cross-region
                // link is the point.
                ctx.out.push(TaggedOp {
                    op: Op::StoreFieldAtRecord {
                        record_local_idx: record_local,
                        offset: layout_field.offset as u32,
                        ty: expected_ir,
                    },
                    range: value.range,
                });
            }
        }
    }

    Ok(())
}

