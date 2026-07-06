//! Lowering sub-module: Phase 5 schema method lowering.
//!
//! Enumerates user-declared schema methods up front (so inter-method
//! and entry-body calls resolve through `SchemaMethodRegistry` before
//! any body is lowered), derives each method's wasm-level signature,
//! and lowers every method body into its own [`Func`].

use super::*;

// =====================================================================
// Phase 5: schema method lowering.
// =====================================================================

/// One enumerated user-declared schema method, paired with the
/// canonical shape of its owning schema. Built by [`enumerate_methods`]
/// before any body lowering so each method's wasm-level function
/// index is decided up front — that's the prerequisite for inter-
/// method calls (`self.other_method()`) and for `obj.method()` calls
/// from the entry body, both of which resolve through
/// [`SchemaMethodRegistry`].
#[derive(Debug, Clone)]
pub(super) struct EnumeratedMethod {
    /// Owning schema name (key into the registry).
    schema_name: String,
    /// Canonical shape of the owning schema — supplied to the
    /// `SelfBinding` so method-body `self.field` walks reuse it.
    schema_shape: Schema,
    /// Analyzer-side metadata for the method (param types, body
    /// node, return type).
    info: SchemaMethodInfo,
    /// IR-level index this method occupies in `Module::funcs`.
    ir_idx: usize,
}

/// Walk every schema with a non-empty methods list, snapshot the
/// methods in source order, and assign IR-side indices. Methods with
/// `is_native` bodies are skipped — Phase 5 does not yet implement
/// the host-import path; the analyzer would have already accepted
/// `#native` methods as opaque references.
pub(super) fn enumerate_methods<'a>(
    tree: &'a AnalyzedTree,
    resolver: &SchemaResolver<'a>,
) -> Result<Vec<EnumeratedMethod>, LoweringError> {
    let mut out: Vec<EnumeratedMethod> = Vec::new();
    // Stable iteration order: schemas appear sorted by name. Without
    // sorting, the HashMap's iteration order would shift the wasm
    // function indices across compiles, breaking `relon.srcmap`
    // determinism the harness relies on.
    let mut schema_names: Vec<&String> = tree.schema_methods.keys().collect();
    schema_names.sort();
    for name in schema_names {
        let methods = match tree.schema_methods.get(name) {
            Some(m) if !m.is_empty() => m,
            _ => continue,
        };
        // Resolve the schema definition into a canonical shape so the
        // method body can walk `self.field` against a stable
        // `Schema` value. Schemas not in the resolver (e.g. native
        // carriers, anonymous dict schemas) get skipped — they don't
        // contribute method bodies the IR can emit.
        let Some(def) = resolver.resolve(name.as_str()) else {
            continue;
        };
        let mut stack: Vec<&str> = Vec::new();
        let schema_shape = canonical_schema_from_def(def, resolver, &mut stack, def.range)?;
        for info in methods {
            if info.is_native || info.body_node.is_none() {
                continue;
            }
            let ir_idx = out.len();
            out.push(EnumeratedMethod {
                schema_name: name.clone(),
                schema_shape: schema_shape.clone(),
                info: info.clone(),
                ir_idx,
            });
        }
    }
    Ok(out)
}

/// Lower every enumerated schema method into an IR `Func` and build
/// the dispatch registry mapping `(schema_name, method_name)` to its
/// combined wasm-level function index plus signature. Called once per
/// entry-module lowering, before the entry body walk consumes the
/// registry.
pub(super) fn lower_schema_methods<'a>(
    tree: &'a AnalyzedTree,
    resolver: &SchemaResolver<'a>,
    const_intern: Rc<RefCell<ConstInternTables>>,
    native_imports: Rc<RefCell<NativeImportBuilder>>,
) -> Result<(Vec<Func>, SchemaMethodRegistry), LoweringError> {
    let enumerated = enumerate_methods(tree, resolver)?;
    let stdlib_offset = stdlib_function_count();
    let mut registry = SchemaMethodRegistry::default();
    // First pass: populate the registry so a method body lowered in
    // the second pass can self-dispatch to a sibling method whose
    // body hasn't been emitted yet (`bar()` from inside `foo()`).
    let mut method_sigs: Vec<MethodSig> = Vec::new();
    for m in &enumerated {
        let sig = method_signature_ir_types(&m.info, resolver)?;
        let wasm_idx = stdlib_offset + m.ir_idx as u32;
        let key = (m.schema_name.clone(), m.info.name.clone());
        registry
            .methods
            .insert(key, (wasm_idx, sig.param_tys.clone(), sig.ret_ty));
        method_sigs.push(sig);
    }
    // Second pass: lower each method's body now that the registry is
    // fully populated. #151 — each method ctx receives a clone of the
    // shared intern handle so its `Op::ConstString` / `Op::ConstList*`
    // ops mint idxs out of the same module-wide allocator as the
    // entry body.
    let mut funcs: Vec<Func> = Vec::with_capacity(enumerated.len());
    for (m, sig) in enumerated.iter().zip(method_sigs) {
        let func = lower_one_method(
            m,
            &sig,
            resolver,
            &registry,
            Rc::clone(&const_intern),
            Rc::clone(&native_imports),
        )?;
        funcs.push(func);
    }
    Ok((funcs, registry))
}

/// Resolved IR-side signature for one schema method. Built once per
/// method during the first pass through [`lower_schema_methods`] and
/// re-used when emitting the body. `param_schemas[i]` is `Some(_)`
/// when the i-th param (including the leading `self` slot) is schema-
/// typed and carries the canonical schema shape so chained-segment
/// reads inside the method body resolve their layouts statically.
#[derive(Debug, Clone)]
pub(super) struct MethodSig {
    param_tys: Vec<IrType>,
    ret_ty: IrType,
    param_schemas: Vec<Option<Schema>>,
}

/// Translate a `SchemaMethodInfo`'s declared param + return types to
/// IR-side types plus, for schema-typed params, their canonical shape
/// (needed so method-body walks can resolve chained field access on
/// those params). Phase 5 restricts the return surface to scalar /
/// `Bool` / `Unit` types — variable-length return values (`String` /
/// `List<Int>` / nested dict) require a tail-cursor protocol the
/// non-entry wasm signature doesn't carry yet.
pub(super) fn method_signature_ir_types(
    info: &SchemaMethodInfo,
    resolver: &SchemaResolver<'_>,
) -> Result<MethodSig, LoweringError> {
    // The receiver `self` is implicit at the source level; the IR
    // function carries it as an explicit i32 parameter at slot 0.
    let mut param_tys: Vec<IrType> = vec![IrType::I32];
    let mut param_schemas: Vec<Option<Schema>> = vec![None];
    for p in &info.params {
        let repr =
            type_node_to_canonical_with_schemas(&p.type_node, resolver).ok_or_else(|| {
                cap!(
                    "method_signature_ir_types.unsupported_type_in_main.1",
                    LoweringError::UnsupportedTypeInMain {
                        type_name: type_head_for_display(&p.type_node),
                        range: p.type_node.range,
                    }
                )
            })?;
        match repr {
            TypeRepr::Schema { schema } => {
                param_tys.push(IrType::I32);
                param_schemas.push(Some(*schema));
            }
            other => {
                param_tys.push(type_repr_to_ir_type(&other)?);
                param_schemas.push(None);
            }
        }
    }
    let ret_repr =
        type_node_to_canonical_with_schemas(&info.return_type, resolver).ok_or_else(|| {
            cap!(
                "method_signature_ir_types.unsupported_type_in_main.2",
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(&info.return_type),
                    range: info.return_type.range,
                }
            )
        })?;
    // Phase 5 scope: only scalar / `Bool` / `Unit` returns ride the
    // wasm function's single-value return slot. Variable-length
    // returns are deferred — they need a tail-cursor handshake the
    // non-entry signature doesn't carry yet.
    let ret_ty = match ret_repr {
        TypeRepr::Int => IrType::I64,
        TypeRepr::Float => IrType::F64,
        TypeRepr::Bool => IrType::Bool,
        TypeRepr::Unit => IrType::Unit,
        _ => {
            return Err(cap!(
                "method_signature_ir_types.unsupported_type_in_main.3",
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(&info.return_type),
                    range: info.return_type.range,
                }
            ));
        }
    };
    Ok(MethodSig {
        param_tys,
        ret_ty,
        param_schemas,
    })
}

/// Lower one schema method body into a `Func`. Self lives at wasm
/// local `0`; declared parameters fill locals `1..=N`. The body must
/// leave exactly one value of the declared return type on the
/// operand stack — the trailing `Op::Return` marker handles wasm
/// emission.
pub(super) fn lower_one_method<'a>(
    m: &EnumeratedMethod,
    sig: &MethodSig,
    resolver: &SchemaResolver<'a>,
    registry: &SchemaMethodRegistry,
    const_intern: Rc<RefCell<ConstInternTables>>,
    native_imports: Rc<RefCell<NativeImportBuilder>>,
) -> Result<Func, LoweringError> {
    let MethodSig {
        param_tys,
        ret_ty,
        param_schemas,
    } = sig;
    let ret_ty = *ret_ty;
    let body_node = m.info.body_node.as_ref().ok_or_else(|| {
        cap!(
            "lower_one_method.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!("SchemaMethod(no-body for `{}`)", m.info.name),
                range: m.info.range,
            }
        )
    })?;
    // Build the per-param metadata, skipping the leading `self` slot
    // since the method ctx tracks it separately via `SelfBinding`.
    let mut method_params: Vec<MethodParam> = Vec::with_capacity(m.info.params.len());
    for (i, p) in m.info.params.iter().enumerate() {
        let wasm_local_idx = (i + 1) as u32;
        // `param_tys[0]` is `self`; the user-declared params start at
        // index 1.
        let ty = param_tys[i + 1];
        let schema = param_schemas.get(i + 1).cloned().unwrap_or(None);
        method_params.push(MethodParam {
            name: p.name.clone(),
            ty,
            wasm_local_idx,
            schema,
        });
    }
    let self_binding = SelfBinding {
        wasm_local_idx: 0,
        schema: m.schema_shape.clone(),
    };
    // `params: &[]` — the method body has no `#main` param surface;
    // every reference flows through `self_binding` / `method_params`
    // / `lets`.
    const EMPTY_PARAMS: &[LocalBinding] = &[];
    let mut ctx = LowerCtx::new_method(
        EMPTY_PARAMS,
        resolver.clone(),
        registry.clone(),
        self_binding,
        method_params,
        const_intern,
        native_imports,
    );
    lower_expr(&body_node.expr, body_node.range, &mut ctx)?;
    // Validate the body left exactly one value of the declared
    // return type on the virtual stack.
    let top = ctx.tstack.last().copied().ok_or_else(|| {
        cap!(
            "lower_one_method.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "SchemaMethod(`{}::{}`) body produced no value",
                    m.schema_name, m.info.name
                ),
                range: body_node.range,
            }
        )
    })?;
    if top.wasm_slot() != ret_ty.wasm_slot() {
        return Err(cap!(
            "lower_one_method.unsupported_type_in_main",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!(
                    "method `{}::{}` returns `{:?}` but body produced `{:?}`",
                    m.schema_name, m.info.name, ret_ty, top
                ),
                range: body_node.range,
            }
        ));
    }
    ctx.out.push(TaggedOp {
        op: Op::Return,
        range: body_node.range,
    });
    Ok(Func {
        name: format!("__method_{}__{}", m.schema_name, m.info.name),
        params: param_tys.to_vec(),
        ret: ret_ty,
        body: ctx.out,
        range: m.info.range,
    })
}
