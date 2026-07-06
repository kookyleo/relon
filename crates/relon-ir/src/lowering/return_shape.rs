//! Lowering sub-module: return-shape canonicalisation and cross-region
//! marshalling classification.
//!
//! Turns `#main`'s return `TypeNode` into canonical `TypeRepr` form
//! (tuples, nested lists, `List<Schema>` envelopes), and owns the
//! pointer-indirect / pointer-array / cross-region walk predicates
//! that decide which list-bearing shapes the compiled backends may
//! marshal in place versus cap loudly. Also hosts the strict
//! `TypeRepr` / `TypeNode` → `IrType` mappers.

use super::*;

pub(super) fn is_removed_unit_null_type_name(name: &str) -> bool {
    matches!(name, "Null" | "Unit")
}

pub(super) fn maybe_optional(t: &TypeNode, base: TypeRepr) -> TypeRepr {
    if t.is_optional {
        TypeRepr::Option {
            inner: Box::new(base),
        }
    } else {
        base
    }
}

/// Convert a `Tuple<...>` type node into the anonymous positional schema used
/// by compiled host boundaries. Element types are converted recursively, so
/// tuples can contain tuples, lists, options, results, and named schemas; the
/// layout pass remains responsible for rejecting shapes a backend cannot yet
/// materialise.
pub(super) fn tuple_type_node_to_schema(
    t: &TypeNode,
    resolver: Option<&SchemaResolver<'_>>,
) -> Option<Schema> {
    if t.path.len() != 1 || t.path[0].as_str() != "Tuple" || t.variant_fields.is_some() {
        return None;
    }
    let mut elements = Vec::with_capacity(t.generics.len());
    for g in &t.generics {
        let elem = match resolver {
            Some(r) => type_node_to_canonical_with_schemas(g, r)?,
            None => type_node_to_canonical(g)?,
        };
        elements.push(elem);
    }
    Some(Schema::tuple(TUPLE_RETURN_SCHEMA_NAME, elements))
}

/// Convert a `Tuple<...>` return type into the same positional schema used for
/// tuple parameters. A non-tuple head returns `None`; a tuple with an element
/// type that cannot be represented canonically returns an explicit lowering
/// error at the element span.
pub(super) fn return_tuple_canonical(
    t: &TypeNode,
    resolver: &SchemaResolver<'_>,
) -> Option<Result<Schema, LoweringError>> {
    if t.path.len() != 1 || t.path[0].as_str() != "Tuple" || t.variant_fields.is_some() {
        return None;
    }
    let mut elements = Vec::with_capacity(t.generics.len());
    for g in &t.generics {
        let Some(elem) = type_node_to_canonical_with_schemas(g, resolver) else {
            return Some(Err(cap!(
                "return_tuple_canonical.unsupported_type_in_main",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!("Tuple element `{}`", type_head_for_display(g)),
                    range: g.range,
                }
            )));
        };
        elements.push(elem);
    }
    Some(Ok(Schema::tuple(TUPLE_RETURN_SCHEMA_NAME, elements)))
}

/// Map a parsed builtin type to a canonical [`TypeRepr`] without resolving
/// user schemas. This keeps the scalar/list paths usable in places that do not
/// have a schema resolver, while still allowing normal recursive builtin
/// nesting such as `List<Tuple<Int, String>>` and `Option<List<Int>>`.
pub(super) fn type_node_to_canonical(t: &TypeNode) -> Option<TypeRepr> {
    if t.path.len() != 1 || t.variant_fields.is_some() {
        return None;
    }
    let head = t.path[0].as_str();
    if is_removed_unit_null_type_name(head) {
        return None;
    }

    let base = match (head, t.generics.as_slice()) {
        ("Int", []) => TypeRepr::Int,
        ("Float", []) => TypeRepr::Float,
        ("Bool", []) => TypeRepr::Bool,
        ("String", []) => TypeRepr::String,
        ("List", [elem]) => TypeRepr::List {
            element: Box::new(type_node_to_canonical(elem)?),
        },
        ("Option", [inner]) => TypeRepr::Option {
            inner: Box::new(type_node_to_canonical(inner)?),
        },
        ("Result", [ok, err]) => TypeRepr::Result {
            ok: Box::new(type_node_to_canonical(ok)?),
            err: Box::new(type_node_to_canonical(err)?),
        },
        ("Tuple", _) => TypeRepr::Schema {
            schema: Box::new(tuple_type_node_to_schema(t, None)?),
        },
        _ => return None,
    };

    Some(maybe_optional(t, base))
}

/// Return-side canonicalizer for the `List<List<…>>` return type
/// (`#main(...) -> List<List<Int|Float|Bool|String|Schema|List<…>>>`). The
/// scalar [`type_node_to_canonical`] only accepts a single level of list,
/// so a nested-list **return** head falls through to here.
///
/// S1/S2 admitted the inline-fixed scalar inner (`List<List<Int>>`); F5
/// widens this to the doubly-nested **pointer-array** shapes
/// (`List<List<String>>` / `List<List<Schema>>`, and deeper
/// `List<List<List<…>>>`): the recursive input marshaller, relocation
/// walker, multi-region verifier, and in-place reader all decode them
/// bit-equal. The outer head must be `List<…>` whose generic is itself a
/// `List<…>`; the inner is resolved through the schema-aware
/// canonicaliser so a `List<List<Schema>>` inner schema lookup succeeds.
/// The layout pass (`inner_list_record_alignment`) is the final arbiter of
/// which inner element types are materialisable — an unsupported leaf
/// (Option / Result / Closure) is still a loud cap there.
///
/// Return-only: parameter canonicalisation already handles nested lists
/// via [`type_node_to_canonical_with_schemas`], and widening the shared
/// scalar canonicalizer would mis-accept nested lists in unrelated
/// surfaces (native-fn signatures, etc.).
///
/// A [`SchemaResolver`] (`Some`) lets a `List<List<Schema>>` return type
/// resolve its inner user schema; `None` resolves only scalar / String
/// inner elements via the resolver-free [`type_node_to_canonical`].
pub(super) fn return_nested_list_canonical(
    t: &TypeNode,
    resolver: Option<&SchemaResolver<'_>>,
) -> Option<TypeRepr> {
    if t.path.len() != 1
        || t.path[0].as_str() != "List"
        || t.generics.len() != 1
        || t.variant_fields.is_some()
    {
        return None;
    }
    // Outer is `List<…>`; the generic must itself be a `List<…>`.
    let inner = match resolver {
        Some(r) => type_node_to_canonical_with_schemas(&t.generics[0], r)?,
        None => type_node_to_canonical(&t.generics[0])?,
    };
    match &inner {
        TypeRepr::List { .. } => Some(TypeRepr::List {
            element: Box::new(inner),
        }),
        _ => None,
    }
}

/// Canonicalise a `-> List<Schema>` return type (S4). The outer head must
/// be `List<…>` with exactly one generic that names a user `#schema`; the
/// inner schema is resolved through the schema-aware canonicaliser. A
/// `List<List<…>>` or `List<scalar>` / `List<String>` generic returns
/// `None` here so those keep their own dedicated paths (the scalar/string
/// list canonicalisers, or the loud cap for nested schema lists).
///
/// Scoped narrowly to the single `List<Schema>` shape: the inner element
/// must be a `TypeRepr::Schema`, never itself a `List`. That keeps
/// `List<List<Schema>>` (a pointer-array-of-pointer-array the in-place
/// reader does not decode) rejected as `UnsupportedTypeInMain`.
pub(super) fn return_list_schema_canonical(t: &TypeNode, resolver: &SchemaResolver<'_>) -> Option<TypeRepr> {
    if t.path.len() != 1
        || t.path[0].as_str() != "List"
        || t.generics.len() != 1
        || t.variant_fields.is_some()
    {
        return None;
    }
    let inner = type_node_to_canonical_with_schemas(&t.generics[0], resolver)?;
    match &inner {
        TypeRepr::Schema { .. } => Some(TypeRepr::List {
            element: Box::new(inner),
        }),
        _ => None,
    }
}

/// True when an [`IrType`] is materialised through a buffer-relative
/// pointer slot rather than stored inline in a record's fixed area —
/// String and every `List<_>` variant. Such fields require an
/// `EmitTailRecordFromAbsoluteAddr` copy into the return buffer's tail
/// before the fixed-area slot can receive the (buffer-relative) offset.
pub(super) fn pointer_indirect_ir_type(t: IrType) -> bool {
    matches!(
        t,
        IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::ListList
    )
}

/// True when `t` is a **pointer-array** list type (`List<String>` /
/// `List<Schema>` / `List<List<_>>`) — i.e. its tail record is a
/// `[len][off_0]…[off_{N-1}]` header whose entries point at *further*
/// records carrying their own inner pointers.
///
/// These are the shapes the return marshaller cannot relocate
/// correctly for an arbitrary source: the compiled return path copies
/// the source block with a single rigid `delta` (see the cranelift
/// `copy_list_string_block` / `emit_tail_record_from_absolute`), which
/// is only sound when the whole reachable block is **contiguous** and
/// its inner offsets share one base — the layout the const-pool
/// `Op::ConstListString` emits. A pointer-array list sourced from a
/// `#main` parameter (or any non-const-pool producer) lives in the
/// input buffer with whole-buffer-relative, non-contiguous offsets;
/// feeding it through the rigid-block copy reads `off_0` as the block
/// start and computes a bogus span, segfaulting or returning corrupt
/// data. `List<Int/Float/Bool>` are *not* pointer-array (their payload
/// is one inline-fixed `[len][payload]` record), so identity-return of
/// a scalar-list param stays correct and is excluded here.
pub(super) fn pointer_array_list_ir_type(t: IrType) -> bool {
    matches!(
        t,
        IrType::ListString | IrType::ListSchema | IrType::ListList
    )
}

/// True when `expr` deterministically lowers to a **const-pool**
/// pointer-array list record — today only a list literal whose every
/// element is a String literal (`["a", "b", …]` → `Op::ConstListString`).
/// That is the one provenance whose tail block is contiguous and
/// single-base, so the rigid-block return copy is provably correct.
///
/// Everything else that can yield a `ListString` value at runtime — a
/// `#main` parameter reference, a field/index load, a function call, a
/// comprehension — produces a block the rigid copy cannot relocate
/// (see [`pointer_array_list_ir_type`]). The return marshaller must
/// reject those loudly rather than emit the silent-miscompile / segfault
/// path. `List<Schema>` / `List<List<_>>` have *no* const-pool producer
/// at all, so this returns `false` for every source carrying them — they
/// stay a loud cap unconditionally.
pub(super) fn pointer_array_list_source_is_const_pool(expr: &Expr) -> bool {
    let Expr::List(items) = expr else {
        return false;
    };
    // An all-String-literal list lowers to `Op::ConstListString`. A
    // mixed / empty / nested list does not reach a pointer-array
    // `ListString` return (it types as a scalar list or fails the
    // element classifier earlier), so requiring String literals here is
    // exactly the const-`ListString` provenance.
    !items.is_empty() && items.iter().all(|n| matches!(&*n.expr, Expr::String(_)))
}

/// True when `expr` is a bare `#main` parameter **identity** reference
/// (`xss` / `ss`, a single-segment `Expr::Variable`) — the value lowers
/// to a single `Load*Ptr` (`LoadListListPtr` / `LoadListStringPtr`) that
/// pushes the input-region root header's arena-relative offset.
///
/// This is the trigger for the in-place region-walk return ABI (S1/S2
/// `List<List<scalar>>`, S3 `List<String>`, S4 `List<Schema>`): the
/// pointer-array value is self-contained in the input buffer and its
/// outer/inner layout is exactly what the host writer emitted, so both
/// AOT backends report the root's arena-absolute offset and the host
/// verifies + decodes it in place — bit-equal to the tree-walk oracle,
/// including every string's bytes and every sub-record field.
///
/// A parameter-**field** walk (`o.tags` / `o.items` / `o.grid`) is the
/// F4 sibling, handled by [`pointer_array_param_field_walk`]. Post-F1 the
/// field-load no longer re-encodes the inner form: every pointer slot is
/// arena-absolute (the input marshaller bakes `in_ptr` recursively), so
/// `LoadFieldAtAbsolute` pushes the field list root's arena-absolute
/// offset directly — bit-equal to tree-walk under the same single-root
/// sentinel + verifier + reader the identity walk uses. Everything else
/// (a list literal, comprehension, call, binary expression) is not a
/// param walk and keeps the existing const-pool / loud-cap paths.
pub(super) fn pointer_array_param_identity_walk(expr: &Expr) -> bool {
    matches!(expr, Expr::Variable(path) if path.len() == 1)
}

/// Wave R3c: true when `expr` is a list higher-order call (`map` /
/// `filter`, in method form `xs.map(f)` or free form `_list_map(xs, f)` /
/// `_list_filter(xs, f)` — the latter is also what a list comprehension
/// desugars to) that, with a `List<String>` declared `#main` return,
/// lowers to one of the R3c String-result bundled bodies
/// (`list_string_map` / `list_int_map_to_string` /
/// `list_float_map_to_string` / `list_string_filter`).
///
/// These bodies build the result `List<String>` pointer-array record in
/// the **scratch** region, and every `off_i` slot they write is an
/// arena-absolute String handle the closure already produced (a const-pool
/// literal or a scratch-built `StrConcatN` / `IntToStr` record — all in the
/// same flat arena). The record is therefore self-contained under the
/// single global arena-relative pointer convention, exactly like a
/// param-sourced pointer array, so it qualifies for the **in-place
/// region-walk return** ABI: the backend reports the root header's
/// arena-absolute offset via the negative sentinel and the host
/// verifies + decodes it in place (over the scratch region) — no rigid
/// block copy / relocation, which is what made the old `List<String>`
/// computed-return path unsound.
///
/// Caller gates this on `ret_ir_ty == IrType::ListString`; we only need to
/// confirm the source is one of the String-result HOF surfaces (the
/// closure-return-type probe in `emit_list_hof_call` is what actually
/// selects a String-result body, so a `map` returning a numeric list never
/// reaches here with a `ListString` return type).
pub(super) fn string_result_list_hof_call(expr: &Expr) -> bool {
    // A list comprehension `[ element for id in src (if cond)? ]` desugars
    // in `lower_comprehension` onto the same `list_*_map` bundled body the
    // method / free forms use, so a `List<String>` comprehension result is
    // the same self-contained scratch pointer-array record (every `off_i`
    // an arena-absolute String handle). The caller gates on
    // `ret_ir_ty == IrType::ListString`, which a comprehension only reaches
    // by lowering through a String-result map body — the numeric / inline
    // shapes never produce a `ListString` return — so the in-place
    // region-walk return ABI applies unchanged.
    if matches!(expr, Expr::Comprehension { .. }) {
        return true;
    }
    let Expr::FnCall { path, .. } = expr else {
        return false;
    };
    // Free form / comprehension desugaring: `_list_map(...)` /
    // `_list_filter(...)` — the leading segment names the builtin.
    if let [TokenKey::String(name, _, _), ..] = path.as_slice() {
        if name == "_list_map" || name == "_list_filter" {
            return true;
        }
    }
    // Method form: `xs.map(f)` / `xs.filter(f)` — the trailing segment
    // names the method.
    matches!(
        path.last(),
        Some(TokenKey::String(m, _, _)) if m == "map" || m == "filter"
    )
}

/// Wave R15: true when `expr` is a `split` call on a String receiver
/// (`s.split(sep)`) that, with a `List<String>` declared `#main` return,
/// lowers to the `split` bundled body. That body builds the result
/// `List<String>` pointer-array record (header + per-segment String
/// records) entirely in the **scratch** region; every `off_i` slot is an
/// arena-absolute handle to a self-contained segment record, so the
/// result qualifies for the in-place region-walk return ABI exactly like
/// the R3c String-result HOF results (see [`string_result_list_hof_call`]).
///
/// The free-call wrapper `_string_split(s, sep)` is a tree-walk-only
/// surface (it routes through the evaluator's `std/string` module, not a
/// lowered free-call body), so only the method form reaches here. Caller
/// gates this on `ret_ir_ty == IrType::ListString`, which a `String`
/// receiver's `split` always produces.
pub(super) fn string_split_call(expr: &Expr) -> bool {
    let Expr::FnCall { path, .. } = expr else {
        return false;
    };
    matches!(
        path.last(),
        Some(TokenKey::String(m, _, _)) if m == "split"
    )
}

/// F4: detect a two-segment `#main` parameter **field** walk (`o.tags`
/// where `o: Outer` is a schema-typed param and `tags: List<…>` is a
/// pointer-array list field), returning the field's canonical `TypeRepr`
/// when matched. The field type is resolved through `main_schema` (the
/// param schema set, element schemas inlined).
///
/// Admission mirrors the identity walk envelope ([`anon_dict_cross_region_param_list`]):
/// `List<String>`, `List<Int|Float|Bool>` (inline-fixed scalar list),
/// `List<List<scalar>>` (nested-scalar pointer array), and `List<Schema>`
/// confined to the S4 sub-record decode scope. A deeper nesting
/// (`List<List<String|Schema>>`) or a non-list / non-pointer-array field
/// returns `None`, so the caller keeps it a loud cap.
///
/// Why this is safe (the F1 flip resolved the S3/S4 rebase cap): the
/// field-load path no longer materialises a re-encoded inner form. The
/// slot at `param_root + field_offset` holds an arena-absolute u32 (the
/// input marshaller's recursive `finish_arena_absolute` relocated it),
/// and `LoadFieldAtAbsolute` loads it verbatim. That offset IS the field
/// list root the verifier classifies (into the input region) and the
/// reader follows cross-region — proven byte-equal to the tree-walk
/// oracle on cranelift / llvm / wasm. F6 generalises this from a single
/// field segment to an arbitrary-depth chain (`o.inner.tags`,
/// `o.a.b.tags`) via [`cross_region_param_field_chain`]: every
/// intermediate segment must be a nested-schema field.
pub(super) fn pointer_array_param_field_walk<'s>(
    expr: &Expr,
    main_schema: &'s Schema,
) -> Option<&'s TypeRepr> {
    let Expr::Variable(path) = expr else {
        return None;
    };
    let [TokenKey::String(p, _, _), rest @ ..] = path.as_slice() else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    let param = main_schema.fields.iter().find(|fl| &fl.name == p)?;
    cross_region_param_field_chain(&param.ty, rest)
}

/// Resolve a `#main` parameter **field chain** (`o.inner.tags`,
/// `o.a.b.tags`, or the single-segment `o.tags`) down to its leaf
/// field's canonical [`TypeRepr`], returning it only when every
/// intermediate segment is a nested-schema field and the leaf field is a
/// pointer-array list inside the cross-region admission envelope
/// ([`cross_region_list_envelope`]).
///
/// `base` is the canonical type the head identifier resolves to (the
/// param's own type, with element schemas inlined); `segs` are the
/// remaining `.field` segments after the head. Each non-leaf segment must
/// name a `TypeRepr::Schema` field so the walk can descend into the
/// sub-record's field set; the leaf segment's type must satisfy
/// `cross_region_list_envelope`.
///
/// Why a deep chain is as safe as the single-segment F4 walk: the
/// `lower_variable` walker already emits one `LoadFieldAtAbsolute` per
/// segment, and post-F1 *every* pointer slot in the input region is
/// arena-absolute. An intermediate nested-schema field load therefore
/// pushes the sub-record's arena-absolute base, and the leaf list-field
/// load reads the list root's arena-absolute offset off that base —
/// exactly the single-root sentinel + multi-region verifier + reader
/// value the F4 / identity walks consume. No re-encode happens at any
/// link, so the host decode is unchanged and the result is byte-equal to
/// the tree-walk oracle at any depth. A non-schema intermediate segment
/// or an out-of-envelope leaf returns `None`, keeping it a loud cap.
pub(super) fn cross_region_param_field_chain<'s>(
    base: &'s TypeRepr,
    segs: &[TokenKey],
) -> Option<&'s TypeRepr> {
    let mut current = base;
    let last = segs.len().checked_sub(1)?;
    for (i, seg) in segs.iter().enumerate() {
        let TokenKey::String(name, _, _) = seg else {
            return None;
        };
        let TypeRepr::Schema { schema } = current else {
            return None;
        };
        let field = schema.fields.iter().find(|fl| &fl.name == name)?;
        if i == last {
            return cross_region_list_envelope(&field.ty).then_some(&field.ty);
        }
        current = &field.ty;
    }
    None
}

/// The cross-region list-field admission envelope, shared by every
/// classifier that decides whether a parameter-sourced `List<…>` value
/// (identity `servers` or field walk `o.tags`) is one the host's
/// multi-region verifier + reader can follow cross-region:
///   * `List<String>` — pointer array of String records,
///   * `List<Int|Float|Bool>` — inline-fixed scalar list,
///   * `List<List<scalar>>` — nested-scalar pointer array (inner element
///     must be an inline-fixed scalar; a deeper nesting or String / Schema
///     inner element is out of scope),
///   * `List<Schema{…}>` confined to the S4 sub-record decode scope, or
///   * `List<Option|Result|Enum>` variant-record elements.
///     (`list_schema_subrecord_in_s4_scope`).
///
/// F5 widens this to the double pointer array `List<List<String>>` /
/// `List<List<Schema>>` (and deeper nested lists): the recursive input
/// marshaller, relocation walker, multi-region verifier, and in-place
/// reader all follow them cross-region bit-equal. Variant-record elements
/// (`Option` / `Result` / custom `#enum`) use the same pointer-array path.
/// A leaf type the layout pass cannot materialise (Closure) returns `false`,
/// keeping it a loud cap.
pub(super) fn cross_region_list_envelope(ty: &TypeRepr) -> bool {
    let TypeRepr::List { element } = ty else {
        return false;
    };
    cross_region_list_element_ok(element.as_ref())
}

/// `true` when a `List<element>` element type is one the cross-region
/// host marshaller / verifier / reader handle. Recurses for nested lists
/// so `List<List<String>>` / `List<List<List<…>>>` are admitted to the
/// depth the layout pass materialises; a `List<Schema>` element still goes
/// through the S4 sub-record envelope.
pub(super) fn cross_region_list_element_ok(element: &TypeRepr) -> bool {
    match element {
        TypeRepr::String | TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => true,
        TypeRepr::Schema { .. } => list_schema_subrecord_in_s4_scope(&TypeRepr::List {
            element: Box::new(element.clone()),
        }),
        TypeRepr::List { element: inner } => cross_region_list_element_ok(inner.as_ref()),
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => true,
        _ => false,
    }
}

/// F1b / F3: classify a host-visible anon-Dict-return field whose value is
/// a bare `#main` parameter identity (`servers` / `tags` / `xs`) of a list
/// type that the cross-region object path can marshal. Returns the
/// parameter's canonical type when it is:
///   * `List<Schema{…}>` whose element sub-record stays inside the
///     in-place reader's decode envelope (`list_schema_subrecord_in_s4_scope`),
///   * `List<List<scalar>>` (the nested-scalar pointer array),
///   * `List<String>` (the pointer-array-of-string), or
///   * `List<Int>` / `List<Float>` / `List<Bool>` (the inline-fixed scalar
///     list — F3).
///
/// All of these live in the *input* region when sourced from a parameter
/// identity while the object head sits in the *output* region, so the field
/// slot must store the parameter list root's **arena-absolute** offset (no
/// tail copy) and the host's multi-region verifier + reader follow it
/// cross-region. The host decode side already handles every one of these
/// element types: the object positive-`bytes_written` path runs
/// `verify_object_return_multi` over the whole arena, and the `BufferReader`
/// field readers (`read_list_string` / `read_list_int` / `read_list_record`
/// / `read_list_list`) follow arena-absolute slot pointers cross-region, so
/// widening the lowering classifier is all that is needed.
///
/// Anything else — a scalar param, a deeper nested element list, a
/// `List<List<String|Schema>>`, or a non-parameter expression — returns
/// `None`, so the field falls through to the existing scalar classifier and
/// stays a loud cap. Mirrors the single-value return gate
/// (`pointer_array_param_identity_walk` + `list_schema_subrecord_in_s4_scope`).
/// F4 widens this to also accept a two-segment parameter **field** walk
/// (`o.tags` where `o` is a schema-typed param and `tags` is a
/// pointer-array list field), resolved through the param's element schema.
/// Both the identity and field walks land the same arena-absolute slot
/// offset on the object field (the field-load no longer re-encodes
/// post-F1), so the host decode is unchanged.
pub(super) fn anon_dict_cross_region_param_list<'p>(
    path: &[TokenKey],
    param_canonicals: &'p HashMap<&str, TypeRepr>,
) -> Option<&'p TypeRepr> {
    match path {
        // Identity walk (`servers` / `tags`): the param's own list type.
        [TokenKey::String(name, _, _)] => {
            let ty = param_canonicals.get(name.as_str())?;
            cross_region_list_envelope(ty).then_some(ty)
        }
        // F4/F6 field walk (`o.tags`, `o.inner.tags`, `o.a.b.tags`): the
        // head must be a schema param and the chain must descend through
        // nested-schema fields to a pointer-array leaf inside the
        // envelope. Resolved through the shared deep-chain walker.
        [TokenKey::String(p, _, _), rest @ ..] if !rest.is_empty() => {
            cross_region_param_field_chain(param_canonicals.get(p.as_str())?, rest)
        }
        _ => None,
    }
}

/// F3: classify a **branded-struct** return field (`#schema Wrapper {
/// servers: List<Server>, … }` returned via `#main(…) -> Wrapper { servers:
/// servers, … }`) as a cross-region parameter-list field. Returns `true`
/// when the field's declared `List<…>` type is one the cross-region object
/// path can marshal AND the value is a bare `#main` parameter identity of a
/// list type whose IR shape matches the field.
///
/// This is the branded-struct sibling of [`anon_dict_cross_region_param_list`]
/// (which works the anon-Dict lowering path); the difference is only how the
/// two paths reach the field — the field type / value-shape admission rules
/// and the resulting arena-absolute slot store are identical. The element
/// type envelope matches: `List<Schema>` confined to S4 sub-record scope,
/// `List<List<scalar>>`, `List<String>`, `List<Option|Result|Enum>`, and `List<Int|Float|Bool>`.
///
/// The value may be either a single-segment `Variable` resolving to a
/// `#main` param whose `IrType` matches the field (the F3 identity walk),
/// or — F4 — a two-segment param **field** walk (`w.items`) where the
/// param is a schema and the named field's IR list type matches. Both
/// land the same arena-absolute slot offset on the struct field post-F1.
/// A literal / load / call is **not** a cross-region walk and keeps the
/// existing const-pool / loud-cap paths.
pub(super) fn branded_field_cross_region_param_list(
    field_ty: &TypeRepr,
    value: &Node,
    ctx: &LowerCtx<'_>,
) -> bool {
    let TypeRepr::List { element } = field_ty else {
        return false;
    };
    if !cross_region_list_envelope(field_ty) {
        return false;
    }
    // Field element-type envelope (mirrors `anon_dict_cross_region_param_list`).
    let field_ir = match element.as_ref() {
        TypeRepr::Schema { .. } => IrType::ListSchema,
        TypeRepr::List { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => IrType::ListList,
        TypeRepr::String => IrType::ListString,
        TypeRepr::Int => IrType::ListInt,
        TypeRepr::Float => IrType::ListFloat,
        TypeRepr::Bool => IrType::ListBool,
        _ => return false,
    };
    let Expr::Variable(path) = &*value.expr else {
        return false;
    };
    match path.as_slice() {
        // F3 identity walk: a bare `#main` param whose IR list type matches.
        [TokenKey::String(name, _, _)] => ctx
            .params
            .iter()
            .any(|b| b.name == *name && b.ty == field_ir),
        // F4/F6 field walk (`w.items`, `w.inner.items`, `w.a.b.items`):
        // the head must be a schema param and the chain must descend
        // through nested-schema fields to a pointer-array leaf whose IR
        // type matches the struct field. The field-load chain pushes the
        // leaf list root's arena-absolute offset post-F1, identical to
        // the identity walk's slot value.
        [TokenKey::String(p, _, _), rest @ ..] if !rest.is_empty() => {
            let Some(binding) = ctx.params.iter().find(|b| b.name == *p) else {
                return false;
            };
            let Some(schema) = binding.schema.as_ref() else {
                return false;
            };
            let base = TypeRepr::Schema {
                schema: Box::new(schema.clone()),
            };
            cross_region_param_field_chain(&base, rest)
                .and_then(|leaf| type_repr_to_ir_type(leaf).ok())
                .map(|ir| ir == field_ir)
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// True when a `List<Schema>` return type's per-element sub-record carries
/// only fields the in-place sub-record reader can decode — **recursively,
/// to any depth** (F7). The admission is type-driven: a field is in scope
/// when its type is a scalar leaf (`Int` / `Float` / `Bool` /
/// `String`) or a `List<element>` whose element is itself in scope
/// ([`cross_region_list_element_ok`]). The list arm reaches back into this
/// predicate for a `List<Schema>` element, so a sub-record field that is
/// itself an object array (`members: List<Person>`) or a nested list
/// (`tags: List<List<Int>>`) — and whose own element schemas again carry
/// such fields — is accepted to whatever depth the element schemas nest.
///
/// The verifier ([`crate::verifier`] in `relon-eval-api`) walks the same
/// graph recursively with a `MAX_DEPTH` guard, and the in-place reader
/// decodes it type-driven, so admitting these here is sound: the host
/// verifies the whole reachable graph before any decode. A field type the
/// layout pass cannot materialise (`Option` / `Result` / `Closure`, or a
/// bare nested `Schema`) returns `false`, keeping that shape a loud cap.
/// `ty` is the full `List<Schema{…}>` return type (the element schema is
/// canonicalised inline, so the field set is available here).
pub(super) fn list_schema_subrecord_in_s4_scope(ty: &TypeRepr) -> bool {
    let TypeRepr::List { element } = ty else {
        return false;
    };
    let TypeRepr::Schema { schema } = element.as_ref() else {
        return false;
    };
    schema.fields.iter().all(|f| match &f.ty {
        TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool | TypeRepr::Unit | TypeRepr::String => {
            true
        }
        // F7: a list field recurses through the shared element predicate,
        // which re-enters this function for a `List<Schema>` element — so
        // `List<Person>` / `List<List<Int>>` / deeper nest are admitted to
        // the depth the element schemas materialise.
        TypeRepr::List { element } => cross_region_list_element_ok(element.as_ref()),
        // Bare nested Schema field, Option / Result / Closure — out of
        // scope (the bare-Schema sub-field reader path is not on the
        // return-side in-place surface yet).
        _ => false,
    })
}

/// Map a canonical [`TypeRepr`] to the matching [`IrType`]. Used both
/// when building the local index (so `Variable` references know their
/// type) and when synthesising the trailing `StoreField`.
pub(super) fn type_repr_to_ir_type(t: &TypeRepr) -> Result<IrType, LoweringError> {
    match t {
        TypeRepr::Int => Ok(IrType::I64),
        TypeRepr::Float => Ok(IrType::F64),
        TypeRepr::Bool => Ok(IrType::Bool),
        TypeRepr::Unit => Ok(IrType::Unit),
        TypeRepr::String => Ok(IrType::String),
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::Int => Ok(IrType::ListInt),
            TypeRepr::Float => Ok(IrType::ListFloat),
            TypeRepr::Bool => Ok(IrType::ListBool),
            TypeRepr::String => Ok(IrType::ListString),
            TypeRepr::Schema { .. } => Ok(IrType::ListSchema),
            TypeRepr::List { .. }
            | TypeRepr::Option { .. }
            | TypeRepr::Result { .. }
            | TypeRepr::Enum { .. } => Ok(IrType::ListList),
            _ => Err(cap!(
                "type_repr_to_ir_type.unsupported_type_in_main.1",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!("{t:?}"),
                    range: TokenRange::default(),
                }
            )),
        },
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            Ok(IrType::I32)
        }
        // Composite types rejected upstream reach this branch only from a
        // hand-crafted IR.
        _ => Err(cap!(
            "type_repr_to_ir_type.unsupported_type_in_main.2",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{t:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}

/// Map a host-fn signature's [`TypeNode`] onto the IR scalar/list
/// type lattice. Returns `None` for any shape outside the native-call
/// envelope (nested schemas, dicts, closures, enums) — the caller
/// treats `None` as "not lowerable as a native import" and lets the
/// name fall through to the stdlib-unknown error, so an unsupported
/// host-fn signature never silently mis-types a call.
pub(super) fn type_node_to_ir_type(t: &TypeNode) -> Option<IrType> {
    let name = t.path.last()?.as_str();
    Some(match name {
        "Int" => IrType::I64,
        "Float" => IrType::F64,
        "Bool" => IrType::Bool,
        "String" => IrType::String,
        "List" => {
            let elem = t.generics.first()?;
            match elem.path.last()?.as_str() {
                "Int" => IrType::ListInt,
                "Float" => IrType::ListFloat,
                "Bool" => IrType::ListBool,
                "String" => IrType::ListString,
                _ => return None,
            }
        }
        _ => return None,
    })
}
