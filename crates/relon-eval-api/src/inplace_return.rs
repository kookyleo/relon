//! Backend-shared host side of the in-place region-walk return ABI
//! (S1/S2 `List<List<scalar>>`, S3 `List<String>`). When a compiled
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
//!    positional reader (`read_list_list_at` / `read_list_string_at`).
//!
//! Both the cranelift and llvm backends call into this one
//! implementation, so the sentinel → region-select → verifier → decode
//! pipeline is genuinely backend-shared rather than mirrored per crate.
//! The machine-code side (which negative sentinel to emit) is the only
//! per-backend half; the host decode is here.

use std::sync::Arc;

use crate::buffer::BufferReader;
use crate::layout::OffsetTable;
use crate::schema_canonical::{Field, TypeRepr};
use crate::value::Value;
use crate::verifier::{verify_value_at, Region};
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
/// (S1/S2) or `List<String>` (S3). The machine code reported `root_abs`
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
    // `List<List<scalar>>` or `List<String>`. Classify it up front so the
    // region/verify pipeline below is shape-agnostic and only the final
    // decode branches. Anything else is ABI drift — surface it loudly.
    enum InplaceShape {
        /// `List<List<scalar>>`; carries the innermost scalar element type.
        ListListScalar(TypeRepr),
        /// `List<String>`.
        ListString,
    }
    let shape = match &return_field.ty {
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::List { element: inner } => {
                InplaceShape::ListListScalar(inner.as_ref().clone())
            }
            TypeRepr::String => InplaceShape::ListString,
            other => {
                return Err(RuntimeError::IoError(format!(
                    "{backend} in-place return: unsupported list element {other:?} \
                     (expected List<scalar> or String)"
                )));
            }
        },
        other => {
            return Err(RuntimeError::IoError(format!(
                "{backend} in-place return: expected a pointer-array List, got {other:?}"
            )));
        }
    };

    // 1. Select the region `root_abs` falls in.
    let const_end = regions.const_data_len;
    let in_start = regions.in_ptr as usize;
    let in_end = in_start + regions.in_len as usize;
    let out_start = regions.out_ptr as usize;
    let out_end = out_start + regions.out_cap as usize;
    let scratch_start = regions.scratch_base as usize;
    let arena_size = regions.arena_size;
    let (region_start, region_end) = if root_abs >= in_start && root_abs < in_end {
        (in_start, in_end)
    } else if root_abs >= out_start && root_abs < out_end {
        (out_start, out_end)
    } else if root_abs >= scratch_start && root_abs < arena_size {
        (scratch_start, arena_size)
    } else if root_abs < const_end {
        (0, const_end)
    } else {
        return Err(RuntimeError::IoError(format!(
            "{backend} in-place return root {root_abs} falls outside every arena region \
             (const_end={const_end}, in=[{in_start},{in_end}), out=[{out_start},{out_end}), \
             scratch=[{scratch_start},{arena_size}))"
        )));
    };

    if region_end > arena_size || region_end > arena.len() {
        return Err(RuntimeError::IoError(format!(
            "{backend} in-place return region exceeds arena bounds"
        )));
    }
    // Region slice; offsets inside are region-relative.
    let region_bytes = &arena[region_start..region_end];
    let root_rel = root_abs - region_start;

    // 2. Recompute the `ListElementKind` sidecar for the outer
    // `List<List<…>>` from the return layout so the verifier knows the
    // root is a pointer-array header.
    let list_element = return_layout.fields.first().and_then(|fo| fo.list_element);

    // 3. Verify the whole reachable graph stays in-region BEFORE any
    // decode. A failure is loud and aborts — never a wild read.
    let region = Region::new(0, region_bytes.len()).map_err(|e| {
        RuntimeError::IoError(format!("{backend} in-place return region invalid: {e}"))
    })?;
    verify_value_at(
        region_bytes,
        &return_field.ty,
        list_element,
        root_rel,
        region,
    )
    .map_err(|e| {
        RuntimeError::IoError(format!(
            "{backend} in-place return verifier rejected the buffer (root_abs={root_abs}, \
             region=[{region_start},{region_end})): {e}"
        ))
    })?;

    // 4. Verified — decode in place. The reader walks the same region
    // slice the verifier certified, so an in-place decode is byte-equal
    // to the field-slot reader the tree-walk oracle's writer produced.
    let reader = BufferReader::new(return_layout, return_fields, region_bytes)
        .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
    match shape {
        InplaceShape::ListListScalar(inner) => {
            let rows = reader
                .read_list_list_at(root_rel, &inner)
                .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
            Ok(Value::List(Arc::new(
                rows.into_iter().map(|r| Value::List(Arc::new(r))).collect(),
            )))
        }
        InplaceShape::ListString => {
            let items = reader
                .read_list_string_at(root_rel)
                .map_err(|e| RuntimeError::IoError(format!("{backend} buffer: {e}")))?;
            Ok(Value::List(Arc::new(
                items.into_iter().map(|s| Value::String(s.into())).collect(),
            )))
        }
    }
}
