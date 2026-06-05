//! Host-walk bounds verifier for the binary handshake buffer.
//!
//! Modelled on the FlatBuffers verifier / rkyv `bytecheck` discipline:
//! before the host dereferences any in-buffer offset it first proves the
//! offset (and the length / payload it introduces) lands inside the
//! buffer — and, for the return-side single-region invariant, inside the
//! **one** region the value is supposed to be self-contained in. A walk
//! that would step outside the region returns a precise
//! [`VerifyError`] rather than reading garbage or panicking.
//!
//! This is a **read-only structural pass**: it never decodes payload
//! semantics (no utf-8 validation, no integer interpretation). Its sole
//! job is to certify that a subsequent [`crate::buffer::BufferReader`]
//! walk over the same bytes can dereference every pointer it will follow
//! without an out-of-bounds access. The reader already performs the same
//! bounds checks inline (and returns its own `BufferError`); the
//! verifier exists so a host can run one cheap up-front pass — and so the
//! single-region invariant (every reachable offset is confined to
//! `[region_start, region_end)`) can be asserted independently of the
//! reader, which only checks against the whole-buffer end.
//!
//! ## Single-region invariant (the load-bearing wall)
//!
//! The compiled-backend ABI lays the arena out as
//! `[const_data | in_buf | out_buf | scratch]`. A `#main` return value
//! is required to be **self-contained inside one region**: a value built
//! in `out_buf` (const-pool literals, copied records) references only
//! `out_buf`; a value returned by identity from a parameter references
//! only `in_buf`. The verifier takes the region bounds explicitly and
//! rejects any offset that escapes them, so a cross-region pointer (the
//! one shape the return marshaller must keep behind a loud capability)
//! is caught as [`VerifyError::OutOfRegion`] instead of being silently
//! dereferenced.

use crate::layout::{FieldKind, ListElementKind, OffsetTable, SchemaLayout};
use crate::schema_canonical::{Field, Schema, TypeRepr};
use thiserror::Error;

/// A half-open byte window `[start, end)` inside the arena that a value
/// is required to be self-contained in. `start <= end` is enforced by
/// [`Region::new`]; every offset the verifier follows must satisfy
/// `start <= off` and the introduced span must end at or before `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// First byte of the region (inclusive), as an absolute index into
    /// the verified byte slice.
    pub start: usize,
    /// One past the last byte of the region (exclusive).
    pub end: usize,
}

impl Region {
    /// Build a region, returning [`VerifyError::DegenerateRegion`] when
    /// `start > end` (a caller bug — the arena layout never produces an
    /// inverted window).
    pub fn new(start: usize, end: usize) -> Result<Self, VerifyError> {
        if start > end {
            return Err(VerifyError::DegenerateRegion { start, end });
        }
        Ok(Self { start, end })
    }

    /// `true` when `[off, off+len)` is fully inside the region. `len`
    /// uses checked arithmetic so an overflowing span is reported as
    /// out-of-region rather than wrapping.
    fn contains_span(&self, off: usize, len: usize) -> bool {
        match off.checked_add(len) {
            Some(end) => off >= self.start && end <= self.end,
            None => false,
        }
    }
}

/// Why a verifier walk rejected a buffer. Every variant names the
/// field (and where useful the offending offset / span) so a host can
/// surface a precise diagnostic. The verifier returns these instead of
/// reading out of bounds or panicking.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// A `Region` was constructed with `start > end`.
    #[error("degenerate region: start {start} > end {end}")]
    DegenerateRegion {
        /// Requested region start.
        start: usize,
        /// Requested region end.
        end: usize,
    },
    /// The byte slice handed to the verifier is shorter than the
    /// region's `end` — the caller's region bounds don't fit the data.
    #[error("buffer of {have} bytes is shorter than region end {end}")]
    BufferShorterThanRegion {
        /// Actual byte length of the slice.
        have: usize,
        /// Region end the caller asserted.
        end: usize,
    },
    /// A fixed-area slot (or the `root_size` window) does not fit inside
    /// the region. Indicates a truncated record or a wrong region base.
    #[error("fixed-area slot for `{field}` at {offset}..+{size} escapes region [{start}, {end})")]
    FixedSlotOutOfRegion {
        /// Field whose fixed-area slot is out of bounds.
        field: String,
        /// Slot start offset.
        offset: usize,
        /// Slot byte size.
        size: usize,
        /// Region start.
        start: usize,
        /// Region end.
        end: usize,
    },
    /// A pointer-indirect offset (a `u32` slot value, a list-entry
    /// pointer, a length prefix, or a payload span) escapes the region.
    /// This is the cross-region / out-of-bounds catch.
    #[error("offset {offset}..+{len} for `{field}` ({what}) escapes region [{start}, {end})")]
    OutOfRegion {
        /// Field being walked.
        field: String,
        /// What kind of span tripped the check (e.g. `"length prefix"`).
        what: &'static str,
        /// Span start.
        offset: usize,
        /// Span byte length.
        len: usize,
        /// Region start.
        start: usize,
        /// Region end.
        end: usize,
    },
    /// A `usize` add overflowed while computing a span end — a hostile
    /// or corrupt buffer carrying a near-`u32::MAX` offset / length.
    #[error("arithmetic overflow computing span for `{field}` ({what})")]
    SpanOverflow {
        /// Field being walked.
        field: String,
        /// What span overflowed.
        what: &'static str,
    },
    /// The schema named a type the verifier does not model (it should
    /// never reach here — the layout pass rejects unsupported types
    /// first — but we surface it loudly rather than skip the check).
    #[error("verifier does not model type `{ty}` in field `{field}`")]
    UnsupportedType {
        /// Field carrying the unmodelled type.
        field: String,
        /// Human-readable type label.
        ty: &'static str,
    },
    /// A recursion-depth guard tripped: the schema nests deeper than
    /// the verifier's bound. Protects against a hand-built schema that
    /// recurses without bound (the runtime schemas are acyclic, but a
    /// verifier must not be DoS-able by a hostile layout).
    #[error("verifier recursion depth exceeded ({depth}) at field `{field}`")]
    DepthExceeded {
        /// Field at which the guard tripped.
        field: String,
        /// The depth bound that was hit.
        depth: usize,
    },
}

/// Maximum schema-nesting depth the verifier walks before bailing.
/// Runtime `#main` schemas never approach this; the cap only guards a
/// hand-built cyclic layout from spinning the verifier.
const MAX_DEPTH: usize = 64;

/// Verify that every offset reachable from a record laid out by
/// `layout` / `fields` — anchored at `record_base` — stays inside
/// `region` for the byte slice `bytes`.
///
/// This is the entry point a host calls before decoding a return value:
/// pass the return layout, the return schema's fields, the buffer-base
/// offset of the root record (`out_ptr`-relative `0` for a top-level
/// return), and the `out_buf` region. A clean `Ok(())` certifies that a
/// subsequent [`crate::buffer::BufferReader`] walk will not dereference
/// out of region.
///
/// `bytes` must cover at least `region.end` bytes; otherwise the
/// region bounds are themselves unsatisfiable and
/// [`VerifyError::BufferShorterThanRegion`] is returned.
pub fn verify_record(
    bytes: &[u8],
    layout: &OffsetTable,
    fields: &[Field],
    record_base: usize,
    region: Region,
) -> Result<(), VerifyError> {
    if bytes.len() < region.end {
        return Err(VerifyError::BufferShorterThanRegion {
            have: bytes.len(),
            end: region.end,
        });
    }
    verify_record_inner(bytes, layout, fields, record_base, region, 0)
}

/// Verify a **bare value** reachable from a direct pointer offset,
/// rather than from a record's fixed-area slot. This is the entry point
/// the in-place region-walk return ABI calls: the machine code reports
/// the arena-absolute offset of a value's root (e.g. a
/// `List<List<scalar>>` header) and the host rebases it to a
/// region-relative offset, then asks the verifier to certify the whole
/// reachable graph stays inside `region` before any decode.
///
/// `ty` is the declared type of the value at `root` (a pointer-indirect
/// type — `String` / `List<…>` / `Schema`); `list_element` is the
/// matching [`ListElementKind`] sidecar for a `List` root (recomputed by
/// the caller from the return layout). `root` is region-relative (an
/// offset into `bytes`, with `bytes` covering at least `region.end`).
///
/// A clean `Ok(())` means a subsequent direct in-place decode of the
/// same value will not dereference outside `region`. Any escape is a
/// loud [`VerifyError`] — the decode must not run.
pub fn verify_value_at(
    bytes: &[u8],
    ty: &TypeRepr,
    list_element: Option<ListElementKind>,
    root: usize,
    region: Region,
) -> Result<(), VerifyError> {
    if bytes.len() < region.end {
        return Err(VerifyError::BufferShorterThanRegion {
            have: bytes.len(),
            end: region.end,
        });
    }
    verify_pointer_target(bytes, "<in-place root>", ty, list_element, root, region, 0)
}

fn verify_record_inner(
    bytes: &[u8],
    layout: &OffsetTable,
    fields: &[Field],
    record_base: usize,
    region: Region,
    depth: usize,
) -> Result<(), VerifyError> {
    if depth >= MAX_DEPTH {
        return Err(VerifyError::DepthExceeded {
            field: "<record>".to_string(),
            depth: MAX_DEPTH,
        });
    }
    // The whole fixed area must land in-region before we read any slot.
    if !region.contains_span(record_base, layout.root_size) {
        return Err(VerifyError::FixedSlotOutOfRegion {
            field: "<root>".to_string(),
            offset: record_base,
            size: layout.root_size,
            start: region.start,
            end: region.end,
        });
    }
    for fo in &layout.fields {
        let slot_abs =
            record_base
                .checked_add(fo.offset)
                .ok_or_else(|| VerifyError::SpanOverflow {
                    field: fo.name.clone(),
                    what: "fixed slot offset",
                })?;
        if !region.contains_span(slot_abs, fo.size) {
            return Err(VerifyError::FixedSlotOutOfRegion {
                field: fo.name.clone(),
                offset: slot_abs,
                size: fo.size,
                start: region.start,
                end: region.end,
            });
        }
        let FieldKind::PointerIndirect { .. } = fo.kind else {
            // Inline scalar: the fixed-slot span check above is the
            // whole verification — no further pointer to follow.
            continue;
        };
        // Find the declared field type so we know what the pointer
        // introduces. A pointer-indirect slot whose name isn't in the
        // schema is a layout/schema drift bug — surface it.
        let declared = fields.iter().find(|f| f.name == fo.name);
        let ty = match declared {
            Some(f) => &f.ty,
            None => {
                return Err(VerifyError::UnsupportedType {
                    field: fo.name.clone(),
                    ty: "<missing schema field>",
                });
            }
        };
        // The pointer slot value is a buffer-relative u32 offset.
        let ptr = read_u32(bytes, slot_abs, &fo.name, "pointer slot", region)?;
        verify_pointer_target(bytes, &fo.name, ty, fo.list_element, ptr, region, depth)?;
    }
    Ok(())
}

/// Follow one pointer-indirect slot's target and validate the tail
/// record it introduces stays in-region. Dispatches on the declared
/// type: `String` / inline-fixed lists carry a single `[len][payload]`
/// record; pointer-array lists (`List<String>` / `List<Schema>` /
/// `List<List<_>>`) carry a `[len][off_0]…` header whose entries are
/// followed recursively; nested `Schema` slots recurse into the
/// sub-record.
fn verify_pointer_target(
    bytes: &[u8],
    field: &str,
    ty: &TypeRepr,
    list_element: Option<ListElementKind>,
    ptr: usize,
    region: Region,
    depth: usize,
) -> Result<(), VerifyError> {
    match ty {
        TypeRepr::String => {
            // `[len: u32][len bytes]`.
            let len = read_u32(bytes, ptr, field, "length prefix", region)?;
            let payload = ptr
                .checked_add(4)
                .ok_or_else(|| VerifyError::SpanOverflow {
                    field: field.to_string(),
                    what: "string payload start",
                })?;
            require_span(field, "string payload", payload, len, region)
        }
        TypeRepr::Schema { schema } => {
            // Nested sub-record: recurse with the sub-layout anchored at
            // `ptr`. We rebuild the sub-layout from the canonical schema
            // so the verifier doesn't depend on a cached table.
            let sub_layout =
                SchemaLayout::offsets_for(schema).map_err(|_| VerifyError::UnsupportedType {
                    field: field.to_string(),
                    ty: "Schema (unlayoutable)",
                })?;
            verify_record_inner(bytes, &sub_layout, &schema.fields, ptr, region, depth + 1)
        }
        TypeRepr::List { element } => {
            verify_list_target(bytes, field, element, list_element, ptr, region, depth)
        }
        other => Err(VerifyError::UnsupportedType {
            field: field.to_string(),
            ty: type_label(other),
        }),
    }
}

/// Validate a `List<T>` tail record. `inline_fixed` payloads are one
/// contiguous `[len][pad][elem*]` span; `pointer_array` payloads are a
/// `[len][off_0]…` header whose entries point at per-element records we
/// recurse into.
fn verify_list_target(
    bytes: &[u8],
    field: &str,
    element: &TypeRepr,
    list_element: Option<ListElementKind>,
    ptr: usize,
    region: Region,
    depth: usize,
) -> Result<(), VerifyError> {
    let count = read_u32(bytes, ptr, field, "list length prefix", region)?;
    let entries_start = ptr
        .checked_add(4)
        .ok_or_else(|| VerifyError::SpanOverflow {
            field: field.to_string(),
            what: "list entries start",
        })?;
    match list_element {
        Some(ListElementKind::InlineFixed {
            elem_size,
            elem_align,
        }) => {
            // Payload starts at `entries_start` padded up to `elem_align`.
            let payload_start =
                align_up(entries_start, elem_align).ok_or_else(|| VerifyError::SpanOverflow {
                    field: field.to_string(),
                    what: "inline list payload start",
                })?;
            let byte_len =
                count
                    .checked_mul(elem_size)
                    .ok_or_else(|| VerifyError::SpanOverflow {
                        field: field.to_string(),
                        what: "inline list byte length",
                    })?;
            require_span(
                field,
                "inline list payload",
                payload_start,
                byte_len,
                region,
            )
        }
        Some(ListElementKind::PointerArray { .. }) => {
            // `[len][off_0: u32]…[off_{count-1}]`; each entry points at a
            // per-element record we recurse into.
            for i in 0..count {
                let entry_off = entries_start
                    .checked_add(i.checked_mul(4).ok_or_else(|| VerifyError::SpanOverflow {
                        field: field.to_string(),
                        what: "list entry index",
                    })?)
                    .ok_or_else(|| VerifyError::SpanOverflow {
                        field: field.to_string(),
                        what: "list entry offset",
                    })?;
                let entry_ptr = read_u32(bytes, entry_off, field, "list entry pointer", region)?;
                // Recurse per element type. The element of a pointer-
                // array list is itself String / Schema / List.
                verify_pointer_target(
                    bytes,
                    field,
                    element,
                    element_list_kind(element),
                    entry_ptr,
                    region,
                    depth + 1,
                )?;
            }
            Ok(())
        }
        None => {
            // A pointer-indirect list slot must carry a list_element
            // sidecar; its absence is a layout-construction bug.
            Err(VerifyError::UnsupportedType {
                field: field.to_string(),
                ty: "List (missing element layout)",
            })
        }
    }
}

/// Resolve the [`ListElementKind`] for the element of a pointer-array
/// list — used when recursing into a `List<List<_>>` inner list. We
/// recompute it from the inner element type via a one-field probe
/// schema so the verifier doesn't need the parent's cached sidecar for
/// the inner level.
fn element_list_kind(element: &TypeRepr) -> Option<ListElementKind> {
    let TypeRepr::List { .. } = element else {
        // String / Schema element of a pointer array: the recursion
        // into `verify_pointer_target` dispatches on the type directly
        // and never reads `list_element`.
        return None;
    };
    // Inner list: build a throwaway `List<inner>` field and read back
    // the layout's element kind.
    let probe = Schema {
        name: "<probe>".to_string(),
        generics: vec![],
        fields: vec![Field {
            name: "f".to_string(),
            ty: element.clone(),
            default: None,
        }],
    };
    SchemaLayout::offsets_for(&probe)
        .ok()
        .and_then(|t| t.fields.into_iter().next())
        .and_then(|fo| fo.list_element)
}

/// Read a little-endian `u32` at `off`, first proving `[off, off+4)` is
/// in-region. Returns the value as a `usize`.
fn read_u32(
    bytes: &[u8],
    off: usize,
    field: &str,
    what: &'static str,
    region: Region,
) -> Result<usize, VerifyError> {
    require_span(field, what, off, 4, region)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[off..off + 4]);
    Ok(u32::from_le_bytes(buf) as usize)
}

/// Assert `[off, off+len)` is inside `region`, returning a precise
/// [`VerifyError::OutOfRegion`] / [`VerifyError::SpanOverflow`] when not.
fn require_span(
    field: &str,
    what: &'static str,
    off: usize,
    len: usize,
    region: Region,
) -> Result<(), VerifyError> {
    if off.checked_add(len).is_none() {
        return Err(VerifyError::SpanOverflow {
            field: field.to_string(),
            what,
        });
    }
    if region.contains_span(off, len) {
        Ok(())
    } else {
        Err(VerifyError::OutOfRegion {
            field: field.to_string(),
            what,
            offset: off,
            len,
            start: region.start,
            end: region.end,
        })
    }
}

/// Round `off` up to the next multiple of `align` (no-op for `align <=
/// 1`). Returns `None` on overflow.
fn align_up(off: usize, align: usize) -> Option<usize> {
    if align <= 1 {
        return Some(off);
    }
    off.checked_next_multiple_of(align)
}

/// Human-readable label for an unmodelled type in an error path.
fn type_label(ty: &TypeRepr) -> &'static str {
    match ty {
        TypeRepr::Null => "Null",
        TypeRepr::Bool => "Bool",
        TypeRepr::Int => "Int",
        TypeRepr::Float => "Float",
        TypeRepr::String => "String",
        TypeRepr::List { .. } => "List",
        TypeRepr::Option { .. } => "Option",
        TypeRepr::Result { .. } => "Result",
        TypeRepr::Schema { .. } => "Schema",
        TypeRepr::Closure { .. } => "Closure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufferBuilder;
    use crate::layout::SchemaLayout;
    use crate::schema_canonical::{Field, Schema};

    fn field(name: &str, ty: TypeRepr) -> Field {
        Field {
            name: name.into(),
            ty,
            default: None,
        }
    }

    fn list(inner: TypeRepr) -> TypeRepr {
        TypeRepr::List {
            element: Box::new(inner),
        }
    }

    /// `{ name: String, age: Int }` — a String tail record alongside an
    /// inline Int. The canonical "host reads out_buf" shape.
    fn user_schema() -> Schema {
        Schema {
            name: "User".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String), field("age", TypeRepr::Int)],
        }
    }

    fn full_region(bytes: &[u8]) -> Region {
        Region::new(0, bytes.len()).expect("region")
    }

    // ---- legal buffers verify clean -------------------------------------

    #[test]
    fn legal_string_int_record_verifies() {
        let schema = user_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_string("name", "ada").unwrap();
        b.write_int("age", 36).unwrap();
        let bytes = b.finish();
        verify_record(&bytes, &layout, &schema.fields, 0, full_region(&bytes))
            .expect("legal buffer must verify");
    }

    #[test]
    fn legal_list_string_record_verifies() {
        let schema = Schema {
            name: "Tags".into(),
            generics: vec![],
            fields: vec![field("tags", list(TypeRepr::String))],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_string("tags", &["a", "bb", "", "中文"])
            .unwrap();
        let bytes = b.finish();
        verify_record(&bytes, &layout, &schema.fields, 0, full_region(&bytes))
            .expect("legal list-string buffer must verify");
    }

    #[test]
    fn legal_list_int_record_verifies() {
        let schema = Schema {
            name: "Nums".into(),
            generics: vec![],
            fields: vec![field("nums", list(TypeRepr::Int))],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_int("nums", &[1, -2, 3, i64::MIN, i64::MAX])
            .unwrap();
        let bytes = b.finish();
        verify_record(&bytes, &layout, &schema.fields, 0, full_region(&bytes))
            .expect("legal list-int buffer must verify");
    }

    #[test]
    fn legal_nested_schema_record_verifies() {
        let addr = Schema {
            name: "Addr".into(),
            generics: vec![],
            fields: vec![field("city", TypeRepr::String), field("zip", TypeRepr::Int)],
        };
        let usr = Schema {
            name: "Usr".into(),
            generics: vec![],
            fields: vec![
                field(
                    "addr",
                    TypeRepr::Schema {
                        schema: Box::new(addr.clone()),
                    },
                ),
                field("name", TypeRepr::String),
            ],
        };
        let usr_layout = SchemaLayout::offsets_for(&usr).expect("usr layout");
        let addr_layout = SchemaLayout::offsets_for(&addr).expect("addr layout");
        let mut b = BufferBuilder::new(&usr_layout, &usr.fields);
        let mut sub = b.sub_record("addr", &addr_layout, &addr.fields).unwrap();
        sub.write_string("city", "BJ").unwrap();
        sub.write_int("zip", 100000).unwrap();
        b.finish_sub_record("addr", sub).unwrap();
        b.write_string("name", "Bob").unwrap();
        let bytes = b.finish();
        verify_record(&bytes, &usr_layout, &usr.fields, 0, full_region(&bytes))
            .expect("legal nested-schema buffer must verify");
    }

    // ---- malformed buffers report loudly (never panic / over-read) ------

    #[test]
    fn out_of_range_string_pointer_rejected() {
        // Patch the String slot to point past the buffer end.
        let schema = user_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_string("name", "ada").unwrap();
        b.write_int("age", 36).unwrap();
        let mut bytes = b.finish();
        // `name` slot is at offset 0; overwrite with a bogus far offset.
        let bogus = (bytes.len() as u32 + 9999).to_le_bytes();
        bytes[0..4].copy_from_slice(&bogus);
        let region = full_region(&bytes);
        let err = verify_record(&bytes, &layout, &schema.fields, 0, region)
            .expect_err("bogus string pointer must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::OutOfRegion {
                    what: "length prefix",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn overlong_string_len_prefix_rejected() {
        // Keep the pointer legal but inflate the length prefix so the
        // payload span runs off the end.
        let schema = user_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_string("name", "ada").unwrap();
        b.write_int("age", 36).unwrap();
        let mut bytes = b.finish();
        let ptr = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        // The len prefix lives at `ptr`; set it to a huge value.
        bytes[ptr..ptr + 4].copy_from_slice(&0xFFFF_F000u32.to_le_bytes());
        let region = full_region(&bytes);
        let err = verify_record(&bytes, &layout, &schema.fields, 0, region)
            .expect_err("overlong string len must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::OutOfRegion {
                    what: "string payload",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn cross_region_pointer_rejected() {
        // A legal whole-buffer walk, but with the region tightened to
        // just the fixed-area + part of the tail: the String payload now
        // lands *outside* the asserted region. This is the single-region
        // invariant catch — a pointer escaping its region is loud.
        let schema = user_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_string("name", "ada").unwrap();
        b.write_int("age", 36).unwrap();
        let bytes = b.finish();
        let ptr = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        // Region ends right after the len prefix but before the payload.
        let region = Region::new(0, ptr + 4).expect("region");
        let err = verify_record(&bytes, &layout, &schema.fields, 0, region)
            .expect_err("payload outside region must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::OutOfRegion {
                    what: "string payload",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn root_record_past_region_rejected() {
        // record_base shifted so the fixed area doesn't fit the region.
        let schema = user_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_string("name", "ada").unwrap();
        b.write_int("age", 36).unwrap();
        let bytes = b.finish();
        let region = Region::new(0, 4).expect("region"); // too small for root_size
        let err = verify_record(&bytes, &layout, &schema.fields, 0, region)
            .expect_err("undersized region must reject the root record");
        assert!(
            matches!(err, VerifyError::FixedSlotOutOfRegion { ref field, .. } if field == "<root>"),
            "got {err:?}"
        );
    }

    #[test]
    fn list_entry_pointer_out_of_range_rejected() {
        // Corrupt one pointer-array entry so it dereferences off-buffer.
        let schema = Schema {
            name: "Tags".into(),
            generics: vec![],
            fields: vec![field("tags", list(TypeRepr::String))],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_string("tags", &["a", "bb"]).unwrap();
        let mut bytes = b.finish();
        // header ptr at slot 0 -> [len][off_0][off_1]. Corrupt off_0.
        let header = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let off0_pos = header + 4;
        let bogus = (bytes.len() as u32 + 5000).to_le_bytes();
        bytes[off0_pos..off0_pos + 4].copy_from_slice(&bogus);
        let region = full_region(&bytes);
        let err = verify_record(&bytes, &layout, &schema.fields, 0, region)
            .expect_err("bogus list entry pointer must reject");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn buffer_shorter_than_region_rejected() {
        let schema = user_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let bytes = vec![0u8; 4];
        let region = Region::new(0, 64).expect("region");
        let err = verify_record(&bytes, &layout, &schema.fields, 0, region)
            .expect_err("region beyond slice must reject");
        assert!(matches!(
            err,
            VerifyError::BufferShorterThanRegion { have: 4, end: 64 }
        ));
    }

    #[test]
    fn degenerate_region_rejected() {
        let err = Region::new(10, 4).expect_err("inverted region");
        assert!(matches!(
            err,
            VerifyError::DegenerateRegion { start: 10, end: 4 }
        ));
    }

    // ---- in-place region-walk return ABI (`verify_value_at`) ------------

    /// Build a single-field `Ret { value: List<List<Int>> }` buffer, the
    /// shape the S1 in-place return decodes. Returns the bytes plus the
    /// `value` slot's `(list_element, root header offset)` so a test can
    /// drive `verify_value_at` directly the way the host does.
    fn list_list_int_buffer() -> (Vec<u8>, Option<ListElementKind>, usize) {
        let schema = Schema {
            name: "Ret".into(),
            generics: vec![],
            fields: vec![field("value", list(list(TypeRepr::Int)))],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        // Build the `Value` rows the host writer consumes:
        // `[[1, 2], [3], []]`.
        let rows: Vec<crate::value::Value> = vec![
            crate::value::Value::List(std::sync::Arc::new(vec![
                crate::value::Value::Int(1),
                crate::value::Value::Int(2),
            ])),
            crate::value::Value::List(std::sync::Arc::new(vec![crate::value::Value::Int(3)])),
            crate::value::Value::List(std::sync::Arc::new(vec![])),
        ];
        crate::buffer::write_nested_scalar_list(&mut b, "value", &TypeRepr::Int, &rows)
            .expect("write nested list");
        let bytes = b.finish();
        // The fixed-area slot for `value` holds the buffer-relative offset
        // of the outer pointer-array header — exactly the `root_abs` the
        // machine code reports (rebased to region-relative).
        let fo = &layout.fields[0];
        let mut slot = [0u8; 4];
        slot.copy_from_slice(&bytes[fo.offset..fo.offset + 4]);
        let header_off = u32::from_le_bytes(slot) as usize;
        (bytes, fo.list_element, header_off)
    }

    #[test]
    fn inplace_list_list_verifies_clean() {
        let (bytes, list_element, root) = list_list_int_buffer();
        let region = Region::new(0, bytes.len()).expect("region");
        verify_value_at(
            &bytes,
            &list(list(TypeRepr::Int)),
            list_element,
            root,
            region,
        )
        .expect("a legal in-place List<List<Int>> root must verify");
    }

    #[test]
    fn inplace_corrupt_inner_pointer_rejected() {
        // Corrupt one outer entry pointer so an inner row dereferences
        // off-buffer; `verify_value_at` must reject loudly, not over-read.
        let (mut bytes, list_element, root) = list_list_int_buffer();
        // Outer header at `root`: `[len][off_0][off_1][off_2]`. Smash
        // `off_0` (first entry) to a far offset.
        let off0_pos = root + 4;
        let bogus = (bytes.len() as u32 + 4096).to_le_bytes();
        bytes[off0_pos..off0_pos + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(
            &bytes,
            &list(list(TypeRepr::Int)),
            list_element,
            root,
            region,
        )
        .expect_err("a corrupt inner pointer must be rejected loudly");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_root_outside_region_rejected() {
        // The root header is legal whole-buffer, but the asserted region
        // ends before it — the single-region catch turns "root in the
        // wrong region" into a loud error instead of a decode.
        let (bytes, list_element, root) = list_list_int_buffer();
        let region = Region::new(0, root).expect("region"); // excludes header
        let err = verify_value_at(
            &bytes,
            &list(list(TypeRepr::Int)),
            list_element,
            root,
            region,
        )
        .expect_err("a root outside the region must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    // ---- in-place `List<String>` root (S3 String-element layer) ---------

    /// Build a single-field `Ret { value: List<String> }` buffer, the
    /// shape the S3 in-place return decodes. Returns the bytes plus the
    /// `value` slot's `(list_element, root header offset)` so a test can
    /// drive `verify_value_at` directly the way the host does — the root
    /// header is `[len][off_0..]` and each `off_i` points at a String
    /// `[slen][utf8]` record.
    fn list_string_buffer(items: &[&str]) -> (Vec<u8>, Option<ListElementKind>, usize) {
        let schema = Schema {
            name: "Ret".into(),
            generics: vec![],
            fields: vec![field("value", list(TypeRepr::String))],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_string("value", items).expect("write list str");
        let bytes = b.finish();
        let fo = &layout.fields[0];
        let mut slot = [0u8; 4];
        slot.copy_from_slice(&bytes[fo.offset..fo.offset + 4]);
        let header_off = u32::from_le_bytes(slot) as usize;
        (bytes, fo.list_element, header_off)
    }

    #[test]
    fn inplace_list_string_verifies_clean() {
        // Empty string, ascii, multibyte (built from code points so the
        // source stays ascii) — all must verify in-region.
        let multibyte: String = [0x4E2Du32, 0x6587]
            .iter()
            .map(|c| char::from_u32(*c).unwrap())
            .collect();
        let items = ["", "x", "abc", multibyte.as_str()];
        let (bytes, list_element, root) = list_string_buffer(&items);
        let region = Region::new(0, bytes.len()).expect("region");
        verify_value_at(&bytes, &list(TypeRepr::String), list_element, root, region)
            .expect("a legal in-place List<String> root must verify");
    }

    #[test]
    fn inplace_list_string_corrupt_entry_pointer_rejected() {
        // Smash `off_0` (the first entry pointer) so the String record it
        // points at lands off-buffer; the verifier must reject loudly
        // rather than let the decode over-read a String record.
        let (mut bytes, list_element, root) = list_string_buffer(&["a", "bb"]);
        let off0_pos = root + 4;
        let bogus = (bytes.len() as u32 + 4096).to_le_bytes();
        bytes[off0_pos..off0_pos + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list(TypeRepr::String), list_element, root, region)
            .expect_err("a corrupt entry pointer must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_string_overlong_str_len_rejected() {
        // Keep the entry pointers legal but inflate one String record's
        // length prefix so its utf8 payload span runs off the region —
        // the String-element bounds check must catch it.
        let (mut bytes, list_element, root) = list_string_buffer(&["ada", "bob"]);
        // off_0 lives at root+4; it points at the first String record's
        // `[slen][utf8]`. Overwrite that record's slen with a huge value.
        let mut o0 = [0u8; 4];
        o0.copy_from_slice(&bytes[root + 4..root + 8]);
        let rec0 = u32::from_le_bytes(o0) as usize;
        bytes[rec0..rec0 + 4].copy_from_slice(&0xFFFF_F000u32.to_le_bytes());
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list(TypeRepr::String), list_element, root, region)
            .expect_err("an overlong String len must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::OutOfRegion {
                    what: "string payload",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_string_outer_len_lies_rejected() {
        // Inflate the outer header's element count so the verifier walks
        // past the real entry array into unrelated bytes; the per-entry
        // pointer read (or its target) must escape the region loudly.
        let (mut bytes, list_element, root) = list_string_buffer(&["a"]);
        bytes[root..root + 4].copy_from_slice(&9999u32.to_le_bytes());
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list(TypeRepr::String), list_element, root, region)
            .expect_err("a lying outer len must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_string_root_outside_region_rejected() {
        // Legal whole-buffer header, but the asserted region ends before
        // it: the single-region catch turns "root in the wrong region"
        // into a loud error rather than a decode.
        let (bytes, list_element, root) = list_string_buffer(&["a", "b"]);
        let region = Region::new(0, root).expect("region"); // excludes header
        let err = verify_value_at(&bytes, &list(TypeRepr::String), list_element, root, region)
            .expect_err("a root outside the region must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    // ---- in-place `List<Schema>` root (S4 field-pointer layer) ----------

    /// A `Cfg` schema whose fields interleave inline scalars with String
    /// and List<scalar>/List<String> tails at varied offsets — the exact
    /// historically-tricky mixed-offset layout (a String hiding after a
    /// Bool, a List between two scalars).
    fn cfg_schema() -> Schema {
        Schema {
            name: "Cfg".into(),
            generics: vec![],
            fields: vec![
                field("flag", TypeRepr::Bool),
                field("name", TypeRepr::String),
                field("port", TypeRepr::Int),
                field("tags", list(TypeRepr::String)),
                field("nums", list(TypeRepr::Int)),
            ],
        }
    }

    /// Build a `Ret { value: List<Cfg> }` buffer with `n` sub-records, the
    /// shape the S4 in-place return decodes. Returns the bytes plus the
    /// `value` slot's `(list_element, root header offset)` so a test can
    /// drive `verify_value_at` the way the host does — the root header is
    /// `[len][off_i]` and each `off_i` points at a `Cfg` sub-record whose
    /// String / List fields point at their own tail records.
    fn list_cfg_buffer(n: usize) -> (Vec<u8>, Option<ListElementKind>, usize) {
        let cfg = cfg_schema();
        let cfg_layout = SchemaLayout::offsets_for(&cfg).expect("cfg layout");
        let ret = Schema {
            name: "Ret".into(),
            generics: vec![],
            fields: vec![field(
                "value",
                list(TypeRepr::Schema {
                    schema: Box::new(cfg.clone()),
                }),
            )],
        };
        let ret_layout = SchemaLayout::offsets_for(&ret).expect("ret layout");
        let mut b = BufferBuilder::new(&ret_layout, &ret.fields);
        let mut writer = b
            .list_record_writer("value", &cfg_layout, &cfg)
            .expect("list_record_writer");
        for i in 0..n {
            let mut child = writer.start_entry();
            child.write_bool("flag", i % 2 == 0).unwrap();
            child.write_string("name", &format!("cfg-{i}")).unwrap();
            child.write_int("port", (1000 + i) as i64).unwrap();
            child.write_list_string("tags", &["a", "", "bb"]).unwrap();
            child.write_list_int("nums", &[i as i64, -1, 7]).unwrap();
            writer.finish_entry(&mut b, child).expect("finish entry");
        }
        b.finish_list_record(writer).expect("finish list");
        let bytes = b.finish();
        let fo = &ret_layout.fields[0];
        let mut slot = [0u8; 4];
        slot.copy_from_slice(&bytes[fo.offset..fo.offset + 4]);
        let header_off = u32::from_le_bytes(slot) as usize;
        (bytes, fo.list_element, header_off)
    }

    fn list_cfg_ty() -> TypeRepr {
        list(TypeRepr::Schema {
            schema: Box::new(cfg_schema()),
        })
    }

    #[test]
    fn inplace_list_schema_verifies_clean() {
        let (bytes, list_element, root) = list_cfg_buffer(3);
        let region = Region::new(0, bytes.len()).expect("region");
        verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect("a legal in-place List<Schema> root must verify to the field-pointer layer");
    }

    #[test]
    fn inplace_list_schema_empty_verifies_clean() {
        let (bytes, list_element, root) = list_cfg_buffer(0);
        let region = Region::new(0, bytes.len()).expect("region");
        verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect("an empty in-place List<Schema> must verify");
    }

    #[test]
    fn inplace_list_schema_corrupt_entry_pointer_rejected() {
        // Smash `off_0` (the first sub-record pointer) so the Cfg record it
        // points at lands off-region; the verifier must reject before any
        // field-pointer is followed.
        let (mut bytes, list_element, root) = list_cfg_buffer(2);
        let off0_pos = root + 4;
        let bogus = (bytes.len() as u32 + 4096).to_le_bytes();
        bytes[off0_pos..off0_pos + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect_err("a corrupt sub-record pointer must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::OutOfRegion { .. } | VerifyError::FixedSlotOutOfRegion { .. }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_schema_corrupt_field_string_pointer_rejected() {
        // Keep every sub-record pointer legal, but smash one sub-record's
        // `name` String slot so the String record it points at escapes the
        // region. This is the field-pointer layer the verifier MUST reach —
        // the most easily-missed level.
        let (mut bytes, list_element, root) = list_cfg_buffer(1);
        // off_0 -> the single Cfg sub-record's fixed area.
        let mut o0 = [0u8; 4];
        o0.copy_from_slice(&bytes[root + 4..root + 8]);
        let sub_base = u32::from_le_bytes(o0) as usize;
        // Find the `name` slot offset within the Cfg fixed area.
        let cfg = cfg_schema();
        let cfg_layout = SchemaLayout::offsets_for(&cfg).expect("cfg layout");
        let name_fo = cfg_layout
            .fields
            .iter()
            .find(|fo| fo.name == "name")
            .expect("name field");
        let name_slot = sub_base + name_fo.offset;
        let bogus = (bytes.len() as u32 + 8192).to_le_bytes();
        bytes[name_slot..name_slot + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect_err("a corrupt sub-record String field pointer must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_schema_corrupt_field_list_pointer_rejected() {
        // Same as above, but smash a sub-record's `tags` (List<String>)
        // field slot so the list header escapes the region — proves the
        // verifier follows List field pointers inside the sub-record too.
        let (mut bytes, list_element, root) = list_cfg_buffer(1);
        let mut o0 = [0u8; 4];
        o0.copy_from_slice(&bytes[root + 4..root + 8]);
        let sub_base = u32::from_le_bytes(o0) as usize;
        let cfg = cfg_schema();
        let cfg_layout = SchemaLayout::offsets_for(&cfg).expect("cfg layout");
        let tags_fo = cfg_layout
            .fields
            .iter()
            .find(|fo| fo.name == "tags")
            .expect("tags field");
        let tags_slot = sub_base + tags_fo.offset;
        let bogus = (bytes.len() as u32 + 8192).to_le_bytes();
        bytes[tags_slot..tags_slot + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect_err("a corrupt sub-record List field pointer must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_schema_outer_len_lies_rejected() {
        // Inflate the outer header's element count so the verifier walks
        // past the real entry array; a per-entry pointer (or its target)
        // must escape the region loudly.
        let (mut bytes, list_element, root) = list_cfg_buffer(1);
        bytes[root..root + 4].copy_from_slice(&9999u32.to_le_bytes());
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect_err("a lying outer len must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::OutOfRegion { .. } | VerifyError::FixedSlotOutOfRegion { .. }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_schema_root_outside_region_rejected() {
        let (bytes, list_element, root) = list_cfg_buffer(2);
        let region = Region::new(0, root).expect("region"); // excludes header
        let err = verify_value_at(&bytes, &list_cfg_ty(), list_element, root, region)
            .expect_err("a root outside the region must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }
}
