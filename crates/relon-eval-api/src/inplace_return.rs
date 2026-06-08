//! Backend-shared host side of the in-place region-walk return ABI
//! (S1/S2 `List<List<scalar>>`, S3 `List<String>`, S4 `List<Schema>`).
//! When a compiled
//! `#main` returns a parameter-sourced pointer-array list, the machine
//! code does **not** copy the value into `out_buf`. Instead the epilogue
//! reports the arena-absolute offset of the return root via the negative
//! sentinel `-(root_abs + 1)`. The host then:
//!
//! 1. recovers `root_abs` from the sentinel ([`decode_inplace_sentinel`]),
//! 2. selects the arena region `root_abs` lands in,
//! 3. runs the bounds [`verify_value_at`] over the whole reachable graph
//!    confined to that region — **a verify failure aborts the decode**,
//! 4. only on a clean verify decodes the value in place via the matching
//!    positional reader (`read_list_list_at` / `read_list_string_at` /
//!    `read_list_record_at`).
//!
//! Both the cranelift and llvm backends call into this one
//! implementation, so the sentinel → region-select → verifier → decode
//! pipeline is genuinely backend-shared rather than mirrored per crate.
//! The machine-code side (which negative sentinel to emit) is the only
//! per-backend half; the host decode is here.

use std::sync::Arc;

use crate::buffer::BufferReader;
use crate::layout::{OffsetTable, SchemaLayout};
use crate::schema_canonical::{Field, Schema, TypeRepr};
use crate::smol_str::SmolStr;
use crate::value::Value;
use crate::verifier::{verify_record_multi, verify_value_at_multi, MultiRegion, VerifyError};
use crate::RuntimeError;

/// Arena region boundaries the in-place decode selects between. The
/// arena layout (shared by both AOT backends) is
/// `[const_data | pad | in_buf | pad | out_buf | pad | scratch]`; the
/// returned root may live in any region (S1 only ever sees the
/// param-sourced `in_buf` list, but the selection is generic so S2+ can
/// return `out_buf` / `scratch` roots through the same gate).
#[derive(Debug, Clone, Copy)]
pub struct ArenaRegions {
    /// Length of the const-data section at arena offset 0.
    pub const_data_len: usize,
    /// Input region start (`in_ptr`) and length.
    pub in_ptr: u32,
    pub in_len: u32,
    /// Output region start (`out_ptr`) and capacity.
    pub out_ptr: u32,
    pub out_cap: u32,
    /// Scratch region start; it runs to `arena_size`.
    pub scratch_base: u32,
    /// Total arena size in bytes.
    pub arena_size: usize,
}

impl ArenaRegions {
    /// Build the four-region [`MultiRegion`] map in absolute arena
    /// coordinates from the dispatch's region boundaries. Every slot
    /// pointer the F1 ABI emits is arena-absolute, so the verifier walks
    /// the **whole arena** and classifies each followed span into one of
    /// `const` / `in` / `out` / `scratch`. The ABI lays the regions out
    /// disjointly as `[const | pad | in | pad | out | pad | scratch]`; we
    /// pass each as a half-open `[start, end)` window.
    pub fn multi_region(&self) -> Result<MultiRegion, VerifyError> {
        let in_start = self.in_ptr as usize;
        let in_end = in_start + self.in_len as usize;
        let out_start = self.out_ptr as usize;
        let out_end = out_start + self.out_cap as usize;
        let scratch_start = self.scratch_base as usize;
        MultiRegion::new(
            (0, self.const_data_len),
            (in_start, in_end),
            (out_start, out_end),
            (scratch_start, self.arena_size),
        )
    }
}

/// Decode the in-place region-walk return sentinel. The machine code
/// encodes an in-place return as `-(root_abs + 1)` (a value `<= -9`,
/// since `root_abs >= in_ptr >= 8`). Recover `root_abs = -ret - 1`,
/// rejecting a sentinel that doesn't round-trip into a non-negative
/// offset (a corrupt / impossible encoding).
///
/// `ret` must already be known negative (the caller distinguishes the
/// non-negative `bytes_written` path before calling).
pub fn decode_inplace_sentinel(ret: i32) -> Result<usize, RuntimeError> {
    // `ret` is negative here. `-(ret as i64) - 1` is the root offset;
    // i64 math avoids the `i32::MIN` negation overflow.
    let root = -(ret as i64) - 1;
    if root < 0 {
        return Err(RuntimeError::IoError(format!(
            "in-place return sentinel {ret} decodes to a negative root offset"
        )));
    }
    usize::try_from(root).map_err(|_| {
        RuntimeError::IoError(format!(
            "in-place return sentinel {ret} decodes to an out-of-range root offset"
        ))
    })
}

/// Decode an in-place region-walk return for a single-value return whose
/// root is a parameter-sourced pointer-array list: `List<List<scalar>>`
/// (S1/S2), `List<String>` (S3), or `List<Schema>` (S4). The machine
/// code reported `root_abs`
/// (the arena-absolute offset of the return root) via the negative
/// sentinel; the caller already recovered it with
/// [`decode_inplace_sentinel`].
///
/// `return_field` is the single return-schema field; `return_layout` /
/// `return_fields` are the matching return [`OffsetTable`] and fields the
/// [`BufferReader`] decodes against. `backend` is a short label
/// (`"cranelift"` / `"llvm"`) for diagnostics.
///
/// This is backend-agnostic: it owns the region-select + verifier +
/// decode pipeline so both AOT backends share exactly one host
/// implementation. It dispatches on the return field's declared type so a
/// single sentinel path covers every in-place shape; an unexpected type
/// reaching here is a lowering/ABI drift bug surfaced loudly.
pub fn decode_inplace_return(
    backend: &str,
    arena: &[u8],
    regions: ArenaRegions,
    root_abs: usize,
    return_field: &Field,
    return_layout: &OffsetTable,
    return_fields: &[Field],
) -> Result<Value, RuntimeError> {
    // The return must be a pointer-array list the in-place ABI emits:
    // `List<List<scalar>>`, `List<String>`, or `List<Schema>`. Classify it
    // up front so the region/verify pipeline below is shape-agnostic and
    // only the final decode branches. Anything else is ABI drift — surface
    // it loudly.
    // The `List` prefix on every variant is intentional: each names the
    // concrete pointer-array return shape (`List<List>` / `List<String>` /
    // `List<Schema>`) the sentinel can carry, so the shared prefix is the
    // point rather than noise.
    #[allow(clippy::enum_variant_names)]
    enum InplaceShape<'a> {
        /// `List<List<scalar>>`; carries the innermost scalar element type.
        ListListScalar(TypeRepr),
        /// `List<String>`.
        ListString,
        /// `List<Schema>`; carries the per-element sub-record schema.
        ListSchema(&'a Schema),
        /// F5: a doubly-nested pointer-array list — `List<List<String>>`
        /// / `List<List<Schema>>` (and deeper). Carries the **outer list
        /// element** type (`List<String>` / `List<Schema>`); the unified
        /// recursive reader walks one level deeper than the scalar path.
        ListListPointerArray(&'a TypeRepr),
    }
    let shape = match &return_field.ty {
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::List { element: inner } => match inner.as_ref() {
                // Inner inline-fixed scalar: the S1/S2 path.
                TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => {
                    InplaceShape::ListListScalar(inner.as_ref().clone())
                }
                // Inner pointer-array element (String / Schema / deeper
                // List): the F5 recursive path. `element` is the outer
                // list element (`List<String>` / `List<Schema>`).
                _ => InplaceShape::ListListPointerArray(element.as_ref()),
            },
            TypeRepr::String => InplaceShape::ListString,
            TypeRepr::Schema { schema } => InplaceShape::ListSchema(schema.as_ref()),
            other => {
                return Err(RuntimeError::IoError(format!(
                    "{backend} in-place return: unsupported list element {other:?} \
                     (expected List<scalar>, String, or Schema)"
                )));
            }
        },
        other => {
            return Err(RuntimeError::IoError(format!(
                "{backend} in-place return: expected a pointer-array List, got {other:?}"
            )));
        }
    };

    // 1. Build the multi-region map over the whole arena. Under the F1
    // arena-absolute slot convention every pointer the in-place root
    // reaches is an arena-absolute offset, so the verifier and reader
    // both walk the **whole arena** rather than a region slice; the
    // multi-region map classifies each followed span into the one region
    // it belongs to (param-sourced data in `in`, copied tails in `out`,
    // const-pool literals in `const`, scratch). The single-value root
    // still lives in exactly one region, but a cross-region link is now
    // legal and bounds-checked region-by-region rather than rejected.
    let arena_size = regions.arena_size;
    if arena_size > arena.len() {
        return Err(RuntimeError::IoError(format!(
            "{backend} in-place return arena_size {arena_size} exceeds arena slice {}",
            arena.len()
        )));
    }
    let arena = &arena[..arena_size];
    let multi = regions.multi_region().map_err(|e| {
        RuntimeError::IoError(format!(
            "{backend} in-place return arena regions invalid: {e}"
        ))
    })?;
    if root_abs >= arena_size {
        return Err(RuntimeError::IoError(format!(
            "{backend} in-place return root {root_abs} is past arena end {arena_size}"
        )));
    }

    // 2. Recompute the `ListElementKind` sidecar for the outer
    // `List<List<…>>` from the return layout so the verifier knows the
    // root is a pointer-array header.
    let list_element = return_layout.fields.first().and_then(|fo| fo.list_element);

    // 3. Verify the whole reachable graph stays inside the arena regions
    // BEFORE any decode. A failure is loud and aborts — never a wild read.
    verify_value_at_multi(arena, &return_field.ty, list_element, root_abs, multi).map_err(|e| {
        RuntimeError::IoError(format!(
            "{backend} in-place return verifier rejected the buffer (root_abs={root_abs}): {e}"
        ))
    })?;

    // 4. Verified — decode in place. The reader walks the whole arena the
    // verifier certified; every slot value is arena-absolute, so an
    // in-place decode is byte-equal (post-decode) to the field-slot
    // reader the tree-walk oracle's writer produced.
    let reader = BufferReader::new(return_layout, return_fields, arena)
        .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
    match shape {
        InplaceShape::ListListScalar(inner) => {
            let rows = reader
                .read_list_list_at(root_abs, &inner)
                .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
            Ok(Value::List(Arc::new(
                rows.into_iter().map(|r| Value::List(Arc::new(r))).collect(),
            )))
        }
        InplaceShape::ListString => {
            let items = reader
                .read_list_string_at(root_abs)
                .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
            Ok(Value::List(Arc::new(
                items.into_iter().map(|s| Value::String(s.into())).collect(),
            )))
        }
        InplaceShape::ListListPointerArray(outer_element) => {
            // The verifier already certified the whole graph (outer
            // entries → inner list headers → inner entries → String /
            // sub-record / String-field layer). Decode in place via the
            // unified recursive reader: `root_abs` is the outer header, and
            // `outer_element` (`List<String>` / `List<Schema>`) drives one
            // level of recursion per outer entry.
            let rows = reader
                .read_list_value_at(root_abs, outer_element)
                .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
            Ok(Value::List(Arc::new(rows)))
        }
        InplaceShape::ListSchema(schema) => {
            // The verifier already certified the whole graph: outer
            // entries, each sub-record's fixed area, and every String /
            // List field pointer the sub-record carries (see
            // `verify_pointer_target` -> `TypeRepr::Schema`). Decode each
            // entry's sub-record into a branded dict, positionally, sharing
            // the same region slice — bit-identical to the field-slot
            // `read_list_record` path the tree-walk oracle's writer feeds.
            let elem_layout = SchemaLayout::offsets_for(schema).map_err(|e| {
                RuntimeError::IoError(format!(
                    "{backend} in-place List<Schema> element `{}` layout: {e}",
                    schema.name
                ))
            })?;
            let sub_readers = reader
                .read_list_record_at(root_abs, &elem_layout, schema)
                .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
            let mut items = Vec::with_capacity(sub_readers.len());
            for sub in &sub_readers {
                if schema.is_tuple {
                    items.push(read_tuple_record_into_tuple(backend, sub, schema)?);
                } else {
                    let map = read_record_into_branded_map(backend, sub, schema)?;
                    items.push(Value::branded_dict(map, Some(schema.name.clone())));
                }
            }
            Ok(Value::List(Arc::new(items)))
        }
    }
}

/// Gate the **object** (positive-`bytes_written`) return path through the
/// multi-region bounds verifier before its `BufferReader` decode runs —
/// closing the red-line gap the S5 design flagged (an object return
/// previously trusted `out_buf` self-containment and ran no verifier at
/// all).
///
/// Under the F1 arena-absolute slot convention the object head (anchored
/// at the arena-absolute `record_base`, i.e. `out_ptr`) and every pointer
/// it reaches are arena-absolute offsets into the whole `arena`. The
/// `multi` map confines each followed span to one of `const` / `in` /
/// `out` / `scratch` and bounds-checks it there: today every object field
/// is still self-contained in `out_buf` (cross-region object fields stay
/// capped — F1b releases them), so every span lands in `out`, but the
/// gate is already cross-region-correct so F1b is a cap flip, not a
/// verifier change. A verify failure is a loud error — the decode must
/// not run.
pub fn verify_object_return_multi(
    backend: &str,
    arena: &[u8],
    record_base: usize,
    multi: MultiRegion,
    return_layout: &OffsetTable,
    return_fields: &[Field],
) -> Result<(), RuntimeError> {
    verify_record_multi(arena, return_layout, return_fields, record_base, multi).map_err(|e| {
        RuntimeError::IoError(format!(
            "{backend} cross-region object return verifier rejected the arena (base={record_base}): {e}"
        ))
    })
}

/// Single central entry for the **object** (positive-`bytes_written`)
/// return path, shared by both AOT backends. It enforces the one correct
/// order — `verify_object_return_multi` **before** any decode — so no
/// present or future object-return caller can reach a `BufferReader`
/// without the multi-region bounds gate having run first. A verify
/// failure aborts loudly; the decode never runs on an unverified arena.
///
/// `arena` must already be sliced to `regions.arena_size` by the caller
/// (each backend owns the read-length bounds check against its own arena
/// allocation). `record_base` is the object head (`out_ptr`).
/// `single_value_wrapper` is the backend's `is_single_value_wrapper`
/// decision (it depends on `relon-ir` schema-name constants this leaf
/// crate intentionally does not pull in); when set, the single
/// return-schema field is decoded directly, otherwise the whole record is
/// drained into a branded dict.
///
/// The decode walks the shared [`read_object_field`] / [`BufferReader`]
/// path for both backends, so the produced [`Value`] is byte-identical
/// regardless of which AOT backend invoked it.
#[allow(clippy::too_many_arguments)]
pub fn decode_object_return(
    backend: &str,
    arena: &[u8],
    record_base: usize,
    regions: ArenaRegions,
    return_layout: &OffsetTable,
    return_schema: &Schema,
    single_value_wrapper: bool,
) -> Result<Value, RuntimeError> {
    let multi = regions.multi_region().map_err(|e| {
        RuntimeError::IoError(format!(
            "{backend} object return arena regions invalid: {e}"
        ))
    })?;
    // Bounds gate FIRST — must precede every decode below.
    verify_object_return_multi(
        backend,
        arena,
        record_base,
        multi,
        return_layout,
        &return_schema.fields,
    )?;
    let reader =
        BufferReader::new_at_base(return_layout, &return_schema.fields, arena, record_base)
            .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
    if single_value_wrapper {
        let field = &return_schema.fields[0];
        read_object_field(backend, &reader, field, return_schema)
    } else if return_schema.is_tuple {
        // A tuple schema is laid out and verified exactly like any other
        // record, but it decodes **positionally** to a `Value::Tuple`
        // (declaration-order slots, no field names). JSON projection still
        // collapses it to an array.
        read_tuple_record_into_tuple(backend, &reader, return_schema)
    } else {
        let map = read_object_record_into_map(backend, &reader, return_schema)?;
        Ok(Value::branded_dict(map, Some(return_schema.name.clone())))
    }
}

/// Drain a tuple schema's fields in declaration order into a positional
/// `Value::Tuple`. The slot readers are the same
/// [`read_object_field`] arms a branded record uses; only the container
/// shape differs (ordered tuple vs named map), which is exactly the
/// tuple-vs-record decode fork.
fn read_tuple_record_into_tuple(
    backend: &str,
    reader: &BufferReader<'_>,
    schema: &Schema,
) -> Result<Value, RuntimeError> {
    let mut items = Vec::with_capacity(schema.fields.len());
    for field in &schema.fields {
        items.push(read_object_field(backend, reader, field, schema)?);
    }
    Ok(Value::Tuple(Arc::new(items)))
}

/// Drain every field of `schema` from the object record `reader` into a
/// sorted `BTreeMap<SmolStr, Value>`. Backend-shared body of the object /
/// nested-record decode (replaces the two per-crate `read_record_into_map`
/// copies the cranelift / llvm evaluators carried).
fn read_object_record_into_map(
    backend: &str,
    reader: &BufferReader<'_>,
    schema: &Schema,
) -> Result<std::collections::BTreeMap<SmolStr, Value>, RuntimeError> {
    let mut map = std::collections::BTreeMap::new();
    for field in &schema.fields {
        let value = read_object_field(backend, reader, field, schema)?;
        map.insert(SmolStr::from(field.name.as_str()), value);
    }
    Ok(map)
}

/// Decode one object-return field via [`BufferReader`] into a [`Value`].
/// This is the single backend-shared copy of the object-path
/// `read_value_from_reader` the cranelift and llvm evaluators each carried
/// (debt 1): adding a new field type now changes exactly this one arm set.
///
/// It covers the full object-field type space — scalars (`Int` / `Float` /
/// `Bool` / `Null`), `String`, every `List<…>` shape, and a bare nested
/// `Schema` (sub-record) — by mapping each leaf onto the matching
/// [`BufferReader`] reader. The `List<List<scalar>>` arm keeps the
/// dedicated `read_list_list` reader (inline-fixed inner rows); every
/// pointer-array list (`List<Schema>` / `List<List<String|Schema>>`)
/// routes through the unified recursive `read_list_value`, exactly as both
/// backends did. `parent_schema` only feeds the unsupported-type
/// diagnostic. The verifier in [`decode_object_return`] has already
/// certified every pointer this reader follows.
fn read_object_field(
    backend: &str,
    reader: &BufferReader<'_>,
    field: &Field,
    parent_schema: &Schema,
) -> Result<Value, RuntimeError> {
    let map_err =
        |e: crate::buffer::BufferError| RuntimeError::IoError(format!("{backend} buffer: {e}"));
    match &field.ty {
        TypeRepr::Int => reader
            .read_int(&field.name)
            .map(Value::Int)
            .map_err(map_err),
        TypeRepr::Float => reader
            .read_float(&field.name)
            .map(|f| Value::Float(ordered_float::OrderedFloat(f)))
            .map_err(map_err),
        TypeRepr::Bool => reader
            .read_bool(&field.name)
            .map(Value::Bool)
            .map_err(map_err),
        TypeRepr::Null => reader
            .read_null(&field.name)
            .map(|()| Value::Null)
            .map_err(map_err),
        TypeRepr::String => reader
            .read_string(&field.name)
            .map(|s| Value::String(s.into()))
            .map_err(map_err),
        // Bare nested Schema: anchor a sub-reader at the slot's
        // sub-record fixed area and recurse into a branded dict.
        TypeRepr::Schema { schema } => {
            let sub_layout = SchemaLayout::offsets_for(schema).map_err(|e| {
                RuntimeError::IoError(format!(
                    "{backend} object return field `{}` layout: {e}",
                    field.name
                ))
            })?;
            let sub_reader = reader
                .sub_record(&field.name, &sub_layout, &schema.fields)
                .map_err(map_err)?;
            let map = read_object_record_into_map(backend, &sub_reader, schema)?;
            Ok(Value::branded_dict(map, Some(schema.name.clone())))
        }
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::Int => reader
                .read_list_int(&field.name)
                .map(|v| Value::List(Arc::new(v.into_iter().map(Value::Int).collect())))
                .map_err(map_err),
            TypeRepr::Float => reader
                .read_list_float(&field.name)
                .map(|v| {
                    Value::List(Arc::new(
                        v.into_iter()
                            .map(|f| Value::Float(ordered_float::OrderedFloat(f)))
                            .collect(),
                    ))
                })
                .map_err(map_err),
            TypeRepr::Bool => reader
                .read_list_bool(&field.name)
                .map(|v| Value::List(Arc::new(v.into_iter().map(Value::Bool).collect())))
                .map_err(map_err),
            TypeRepr::String => reader
                .read_list_string(&field.name)
                .map(|v| {
                    Value::List(Arc::new(
                        v.into_iter().map(|s| Value::String(s.into())).collect(),
                    ))
                })
                .map_err(map_err),
            TypeRepr::Schema { schema } => {
                let elem_layout = SchemaLayout::offsets_for(schema).map_err(|e| {
                    RuntimeError::IoError(format!(
                        "{backend} object return List<Schema> element `{}` layout: {e}",
                        schema.name
                    ))
                })?;
                let sub_readers = reader
                    .read_list_record(&field.name, &elem_layout, schema)
                    .map_err(map_err)?;
                let mut items = Vec::with_capacity(sub_readers.len());
                for sub in &sub_readers {
                    if schema.is_tuple {
                        items.push(read_tuple_record_into_tuple(backend, sub, schema)?);
                    } else {
                        let map = read_object_record_into_map(backend, sub, schema)?;
                        items.push(Value::branded_dict(map, Some(schema.name.clone())));
                    }
                }
                Ok(Value::List(Arc::new(items)))
            }
            // `List<List<…>>`: the inner inline-fixed scalar case keeps the
            // dedicated `read_list_list` reader; `List<List<String|Schema>>`
            // routes through the recursive `read_list_value` one level
            // deeper. Byte-identical to both backends' prior arms.
            TypeRepr::List { element: inner } => match inner.as_ref() {
                TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => reader
                    .read_list_list(&field.name)
                    .map(|rows| {
                        Value::List(Arc::new(
                            rows.into_iter().map(|r| Value::List(Arc::new(r))).collect(),
                        ))
                    })
                    .map_err(map_err),
                _ => reader
                    .read_list_value(&field.name, element.as_ref())
                    .map(|rows| Value::List(Arc::new(rows)))
                    .map_err(map_err),
            },
            other => Err(RuntimeError::IoError(format!(
                "{backend} object return: cannot decode list field `{field}` of element type \
                 `{ty:?}` in schema `{schema}`",
                field = field.name,
                ty = other,
                schema = parent_schema.name,
            ))),
        },
        // ----- add new object-return field type arm above this line -----
        other => Err(RuntimeError::IoError(format!(
            "{backend} object return: cannot decode field `{field}` of type `{ty:?}` in schema \
             `{schema}`",
            field = field.name,
            ty = other,
            schema = parent_schema.name,
        ))),
    }
}

/// Drain every field of `schema` from the sub-record `reader` into a
/// sorted `BTreeMap<SmolStr, Value>` — the branded-dict body for one
/// `List<Schema>` element. Mirrors the codegen evaluators'
/// `read_record_into_map`, but lives here so both AOT backends share the
/// one in-place sub-record decode. The field decode reuses the existing
/// [`BufferReader`] field-slot readers, which the verifier has already
/// proven stay in-region for every pointer they follow.
fn read_record_into_branded_map(
    backend: &str,
    reader: &BufferReader<'_>,
    schema: &Schema,
) -> Result<std::collections::BTreeMap<SmolStr, Value>, RuntimeError> {
    let mut map = std::collections::BTreeMap::new();
    for field in &schema.fields {
        let value = read_record_field(backend, reader, field)?;
        map.insert(SmolStr::from(field.name.as_str()), value);
    }
    Ok(map)
}

/// Decode one sub-record field (`reader` is the per-element sub-record
/// reader) into a [`Value`]. Covers the scalar leaves plus every
/// pointer-indirect list field a `List<Schema>` element can carry —
/// **recursively, to any depth** (F7): `String`, `List<scalar>`,
/// `List<String>`, and (new) `List<Schema>` / `List<List<…>>` element
/// fields whose own element schemas again carry such fields. The list
/// arms route through the buffer reader's unified recursive list reader
/// ([`BufferReader::read_list_value`]), which follows the pointer array
/// one level per element type — exactly the depth the multi-region
/// verifier already certified before this decode runs. The produced
/// values are bit-identical to the codegen evaluators' `read_list_value`
/// path the tree-walk oracle's writer feeds. A bare nested `Schema`
/// field, or any non-list non-scalar leaf, is capped at lowering, so a
/// reach here is ABI drift surfaced loudly rather than mis-decoded.
fn read_record_field(
    backend: &str,
    reader: &BufferReader<'_>,
    field: &Field,
) -> Result<Value, RuntimeError> {
    let name = field.name.as_str();
    let map_err = |e: crate::buffer::BufferError| {
        RuntimeError::IoError(format!(
            "{backend} in-place List<Schema> field `{name}`: {e}"
        ))
    };
    match &field.ty {
        TypeRepr::Int => reader.read_int(name).map(Value::Int).map_err(map_err),
        TypeRepr::Float => reader
            .read_float(name)
            .map(|f| Value::Float(ordered_float::OrderedFloat(f)))
            .map_err(map_err),
        TypeRepr::Bool => reader.read_bool(name).map(Value::Bool).map_err(map_err),
        TypeRepr::Null => reader
            .read_null(name)
            .map(|()| Value::Null)
            .map_err(map_err),
        TypeRepr::String => reader
            .read_string(name)
            .map(|s| Value::String(s.into()))
            .map_err(map_err),
        // Every list field — `List<scalar>` / `List<String>` /
        // `List<Schema>` / `List<List<…>>` — decodes through the unified
        // recursive list reader. It dispatches on `element` and recurses
        // per pointer-array entry (into sub-records via `read_record_at`,
        // into inner lists via itself), so nested object arrays and nested
        // lists inside the element schema are decoded to any depth.
        TypeRepr::List { element } => reader
            .read_list_value(name, element.as_ref())
            .map(|rows| Value::List(Arc::new(rows)))
            .map_err(map_err),
        other => Err(RuntimeError::IoError(format!(
            "{backend} in-place List<Schema> sub-record field `{name}` has unsupported type \
             {other:?}"
        ))),
    }
}
