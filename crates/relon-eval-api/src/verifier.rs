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
//!
//! ## Multi-region walk (F0 cross-region safety net)
//!
//! The S5 cross-region return shape (`-> Dict { servers: List<Cfg> }`)
//! builds the object head in `out_buf` while the parameter-sourced field
//! data still lives in `in_buf` — a single value graph that legitimately
//! spans two regions. The single-region wall would (correctly, for S1–S6)
//! reject it. [`MultiRegion`] + [`verify_value_at_multi`] /
//! [`verify_record_multi`] are the multi-region-aware sibling pass: they
//! walk over the **whole arena** in absolute coordinates, and at every
//! pointer they (1) read the slot value as an **arena-absolute** offset
//! (the convention F1 codegen will emit — see the plan's "F0 design
//! decision" note: a single global arena-relative pointer convention),
//! (2)
//! classify which region that absolute offset lands in, and (3)
//! bounds-check the introduced span against **that one** region. A
//! pointer that lands in no region, or whose span runs off the region it
//! starts in, is a loud [`VerifyError`] — never a wild read. The object
//! positive-`bytes_written` return path runs this pass before any decode,
//! closing the red-line "cross-region pointer into an object slot is not
//! verified" gap.
//!
//! **F0 does not release any capability.** The multi-region pass is the
//! safety net + the facilities that let F1 release the first cross-region
//! shape behind a verified gate; the cross-region object/struct lowering
//! stays capped until then.

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

/// The four arena regions a cross-region return value may reach, in the
/// fixed ABI layout order `[const_data | in_buf | out_buf | scratch]`.
/// Used only to label which region a multi-region span landed in for
/// diagnostics; the verifier itself treats them uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionTag {
    /// Const-data pool at arena offset 0.
    Const,
    /// Input buffer (`in_ptr..in_ptr+in_len`) — parameter-sourced data.
    In,
    /// Output buffer (`out_ptr..out_ptr+out_cap`) — the object head and
    /// const-pool / copied tails.
    Out,
    /// Scratch region (`scratch_base..arena_size`).
    Scratch,
}

impl RegionTag {
    /// Short region label for diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            RegionTag::Const => "const",
            RegionTag::In => "in",
            RegionTag::Out => "out",
            RegionTag::Scratch => "scratch",
        }
    }
}

/// A set of arena regions in **absolute arena coordinates**, the
/// multi-region sibling of [`Region`]. Where a [`Region`] confines an
/// entire value graph to one window, a `MultiRegion` lets the graph span
/// regions: every followed pointer is an arena-absolute offset that must
/// land fully inside *one* of these regions (`contains_span` picks the
/// region whose window the span starts in and requires the whole span to
/// fit it). A span that starts in no region, or starts in one region but
/// runs past its end, is rejected — there is no "fell through to the next
/// region" silent over-read.
///
/// The regions are half-open `[start, end)` absolute byte windows and may
/// be empty (`start == end`, e.g. a zero-length `in_buf`); an empty
/// region contains no span. They are expected non-overlapping (the ABI
/// arena layout guarantees this), but `contains_span` only ever requires
/// a span to fit *some* region, so a benign overlap could not cause a
/// missed bounds check.
#[derive(Debug, Clone, Copy)]
pub struct MultiRegion {
    regions: [(RegionTag, Region); 4],
}

impl MultiRegion {
    /// Build the four-region map from absolute arena boundaries. Each
    /// pair is a half-open `[start, end)` window; `start > end` for any
    /// region is a caller bug surfaced as [`VerifyError::DegenerateRegion`].
    pub fn new(
        const_data: (usize, usize),
        in_buf: (usize, usize),
        out_buf: (usize, usize),
        scratch: (usize, usize),
    ) -> Result<Self, VerifyError> {
        Ok(Self {
            regions: [
                (RegionTag::Const, Region::new(const_data.0, const_data.1)?),
                (RegionTag::In, Region::new(in_buf.0, in_buf.1)?),
                (RegionTag::Out, Region::new(out_buf.0, out_buf.1)?),
                (RegionTag::Scratch, Region::new(scratch.0, scratch.1)?),
            ],
        })
    }

    /// The largest `end` across all regions — the minimum byte length the
    /// verified arena slice must cover for every region bound to be
    /// satisfiable.
    fn max_end(&self) -> usize {
        self.regions.iter().map(|(_, r)| r.end).max().unwrap_or(0)
    }

    /// Classify the absolute offset `off` (with span `len`) into the one
    /// region that fully contains `[off, off+len)`. Returns the region
    /// tag on success, or `None` when the span fits no single region.
    fn classify_span(&self, off: usize, len: usize) -> Option<RegionTag> {
        for (tag, region) in &self.regions {
            // Empty regions contain nothing; `contains_span` already
            // handles that via `off >= start && end <= end` with
            // `start == end`.
            if region.contains_span(off, len) {
                return Some(*tag);
            }
        }
        None
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
    /// A multi-region walk followed a pointer whose arena-absolute span
    /// `[offset, offset+len)` fits inside **no** arena region (or starts
    /// in one region but runs past its end). The cross-region catch: a
    /// pointer must land fully inside exactly one of const / in / out /
    /// scratch, never between or past them.
    #[error(
        "offset {offset}..+{len} for `{field}` ({what}) fits no arena region \
         (const, in, out, scratch are all disjoint windows)"
    )]
    NoRegion {
        /// Field being walked.
        field: String,
        /// What kind of span tripped the check.
        what: &'static str,
        /// Absolute span start.
        offset: usize,
        /// Span byte length.
        len: usize,
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

/// The bounds discipline a verifier walk enforces: either the
/// single-region wall (S1–S6: the whole graph must stay in one region) or
/// the multi-region map (F0 cross-region: each followed span must stay in
/// *some* region). Both expose the same two operations the walk needs —
/// "does this absolute span fit my bounds" and "what is the maximum end my
/// bounds require the slice to cover" — so the recursive walk is written
/// once and parameterised over this.
#[derive(Debug, Clone, Copy)]
enum Bounds {
    /// S1–S6 single-region wall. Every offset is region-relative into a
    /// region-sliced byte array; the whole graph must stay in `[start,
    /// end)`.
    Single(Region),
    /// F0 multi-region map. Every offset is arena-absolute into the whole
    /// arena slice; each span must fit one region, cross-region links
    /// allowed.
    Multi(MultiRegion),
}

impl Bounds {
    /// The minimum byte length the verified slice must cover.
    fn required_len(&self) -> usize {
        match self {
            Bounds::Single(r) => r.end,
            Bounds::Multi(m) => m.max_end(),
        }
    }

    /// Assert `[off, off+len)` (an absolute offset into the verified
    /// slice) is legal under this bounds discipline. `field` / `what`
    /// label the offending span for diagnostics.
    fn require_span(
        &self,
        field: &str,
        what: &'static str,
        off: usize,
        len: usize,
    ) -> Result<(), VerifyError> {
        if off.checked_add(len).is_none() {
            return Err(VerifyError::SpanOverflow {
                field: field.to_string(),
                what,
            });
        }
        match self {
            Bounds::Single(region) => {
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
            Bounds::Multi(multi) => {
                if multi.classify_span(off, len).is_some() {
                    Ok(())
                } else {
                    Err(VerifyError::NoRegion {
                        field: field.to_string(),
                        what,
                        offset: off,
                        len,
                    })
                }
            }
        }
    }

    /// Assert a record fixed-area slot `[off, off+size)` fits one region,
    /// reporting the dedicated [`VerifyError::FixedSlotOutOfRegion`] /
    /// [`VerifyError::NoRegion`] diagnostic (a truncated record / wrong
    /// base) rather than the generic pointer-span error.
    fn require_fixed_slot(&self, field: &str, off: usize, size: usize) -> Result<(), VerifyError> {
        match self {
            Bounds::Single(region) => {
                if region.contains_span(off, size) {
                    Ok(())
                } else {
                    Err(VerifyError::FixedSlotOutOfRegion {
                        field: field.to_string(),
                        offset: off,
                        size,
                        start: region.start,
                        end: region.end,
                    })
                }
            }
            Bounds::Multi(multi) => {
                if multi.classify_span(off, size).is_some() {
                    Ok(())
                } else {
                    Err(VerifyError::NoRegion {
                        field: field.to_string(),
                        what: "record fixed slot",
                        offset: off,
                        len: size,
                    })
                }
            }
        }
    }
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
    verify_record_with_bounds(bytes, layout, fields, record_base, Bounds::Single(region))
}

/// Multi-region sibling of [`verify_record`]: verify a record laid out at
/// the **arena-absolute** offset `record_base`, where every pointer slot
/// holds an arena-absolute offset and the graph may legitimately span the
/// `const` / `in` / `out` / `scratch` regions described by `multi`.
///
/// `bytes` is the **whole arena** slice (offsets are absolute into it),
/// and must cover at least the largest region end. This is the entry
/// point the object positive-`bytes_written` return path runs before
/// decoding a cross-region object head — the gap the S5 design flagged as
/// the red-line "object slot cross-region pointer is not verified".
pub fn verify_record_multi(
    bytes: &[u8],
    layout: &OffsetTable,
    fields: &[Field],
    record_base: usize,
    multi: MultiRegion,
) -> Result<(), VerifyError> {
    verify_record_with_bounds(bytes, layout, fields, record_base, Bounds::Multi(multi))
}

fn verify_record_with_bounds(
    bytes: &[u8],
    layout: &OffsetTable,
    fields: &[Field],
    record_base: usize,
    bounds: Bounds,
) -> Result<(), VerifyError> {
    let required = bounds.required_len();
    if bytes.len() < required {
        return Err(VerifyError::BufferShorterThanRegion {
            have: bytes.len(),
            end: required,
        });
    }
    verify_record_inner(bytes, layout, fields, record_base, bounds, 0)
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
    verify_value_at_with_bounds(bytes, ty, list_element, root, Bounds::Single(region))
}

/// Multi-region sibling of [`verify_value_at`]: certify a bare value
/// whose root pointer (and every pointer reachable from it) is an
/// arena-absolute offset and whose graph may span regions. `bytes` is the
/// whole arena slice; `root` is the arena-absolute offset of the value's
/// root header. Any escape (a span fitting no region, or running off the
/// region it starts in) is a loud [`VerifyError`] — the decode must not
/// run.
pub fn verify_value_at_multi(
    bytes: &[u8],
    ty: &TypeRepr,
    list_element: Option<ListElementKind>,
    root: usize,
    multi: MultiRegion,
) -> Result<(), VerifyError> {
    verify_value_at_with_bounds(bytes, ty, list_element, root, Bounds::Multi(multi))
}

fn verify_value_at_with_bounds(
    bytes: &[u8],
    ty: &TypeRepr,
    list_element: Option<ListElementKind>,
    root: usize,
    bounds: Bounds,
) -> Result<(), VerifyError> {
    let required = bounds.required_len();
    if bytes.len() < required {
        return Err(VerifyError::BufferShorterThanRegion {
            have: bytes.len(),
            end: required,
        });
    }
    verify_pointer_target(bytes, "<in-place root>", ty, list_element, root, bounds, 0)
}

fn verify_record_inner(
    bytes: &[u8],
    layout: &OffsetTable,
    fields: &[Field],
    record_base: usize,
    bounds: Bounds,
    depth: usize,
) -> Result<(), VerifyError> {
    if depth >= MAX_DEPTH {
        return Err(VerifyError::DepthExceeded {
            field: "<record>".to_string(),
            depth: MAX_DEPTH,
        });
    }
    // The whole fixed area must land in one region before we read any
    // slot. (Single-region: the one window; multi-region: the record
    // head and all its slots must sit together in a single region — a
    // record fixed area is never itself split across regions.)
    bounds.require_fixed_slot("<root>", record_base, layout.root_size)?;
    for fo in &layout.fields {
        let slot_abs =
            record_base
                .checked_add(fo.offset)
                .ok_or_else(|| VerifyError::SpanOverflow {
                    field: fo.name.clone(),
                    what: "fixed slot offset",
                })?;
        bounds.require_fixed_slot(&fo.name, slot_abs, fo.size)?;
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
        // The pointer slot value: a buffer-relative `u32` under the
        // single-region wall, an arena-absolute `u32` under the
        // multi-region map.
        let ptr = read_u32(bytes, slot_abs, &fo.name, "pointer slot", bounds)?;
        verify_pointer_target(bytes, &fo.name, ty, fo.list_element, ptr, bounds, depth)?;
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
    bounds: Bounds,
    depth: usize,
) -> Result<(), VerifyError> {
    match ty {
        TypeRepr::String => {
            // `[len: u32][len bytes]`.
            let len = read_u32(bytes, ptr, field, "length prefix", bounds)?;
            let payload = ptr
                .checked_add(4)
                .ok_or_else(|| VerifyError::SpanOverflow {
                    field: field.to_string(),
                    what: "string payload start",
                })?;
            bounds.require_span(field, "string payload", payload, len)
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
            verify_record_inner(bytes, &sub_layout, &schema.fields, ptr, bounds, depth + 1)
        }
        TypeRepr::List { element } => {
            verify_list_target(bytes, field, element, list_element, ptr, bounds, depth)
        }
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            verify_variant_target(bytes, field, ty, ptr, bounds, depth)
        }
        other => Err(VerifyError::UnsupportedType {
            field: field.to_string(),
            ty: type_label(other),
        }),
    }
}

fn verify_variant_target(
    bytes: &[u8],
    field: &str,
    ty: &TypeRepr,
    ptr: usize,
    bounds: Bounds,
    depth: usize,
) -> Result<(), VerifyError> {
    if depth >= MAX_DEPTH {
        return Err(VerifyError::DepthExceeded {
            field: field.to_string(),
            depth: MAX_DEPTH,
        });
    }
    bounds.require_span(field, "variant tag", ptr, 1)?;
    let tag = bytes[ptr];
    let Some(payload_ty) =
        variant_payload_type(ty, tag).ok_or_else(|| VerifyError::UnsupportedType {
            field: field.to_string(),
            ty: type_label(ty),
        })?
    else {
        return Ok(());
    };
    let (slot_size, slot_align) = variant_payload_slot_layout(&payload_ty);
    let slot = align_up(
        ptr.checked_add(1)
            .ok_or_else(|| VerifyError::SpanOverflow {
                field: field.to_string(),
                what: "variant payload slot start",
            })?,
        slot_align,
    )
    .ok_or_else(|| VerifyError::SpanOverflow {
        field: field.to_string(),
        what: "variant payload slot alignment",
    })?;
    bounds.require_span(field, "variant payload slot", slot, slot_size)?;
    match &payload_ty {
        TypeRepr::Unit | TypeRepr::Bool | TypeRepr::Int | TypeRepr::Float => Ok(()),
        TypeRepr::String
        | TypeRepr::Schema { .. }
        | TypeRepr::List { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => {
            let target = read_u32(bytes, slot, field, "variant payload pointer", bounds)?;
            verify_pointer_target(
                bytes,
                field,
                &payload_ty,
                list_kind_for_list_type(&payload_ty),
                target,
                bounds,
                depth + 1,
            )
        }
        TypeRepr::Closure { .. } => Err(VerifyError::UnsupportedType {
            field: field.to_string(),
            ty: "Closure",
        }),
    }
}

fn variant_payload_type(ty: &TypeRepr, tag: u8) -> Option<Option<TypeRepr>> {
    match ty {
        TypeRepr::Option { inner } => match tag {
            0 => Some(None),
            1 => Some(Some(inner.as_ref().clone())),
            _ => None,
        },
        TypeRepr::Result { ok, err } => match tag {
            0 => Some(Some(ok.as_ref().clone())),
            1 => Some(Some(err.as_ref().clone())),
            _ => None,
        },
        TypeRepr::Enum { name, variants } => variants
            .iter()
            .find(|variant| variant.tag == tag)
            .map(|variant| {
                variant.payload_schema(name).map(|schema| TypeRepr::Schema {
                    schema: Box::new(schema),
                })
            }),
        _ => None,
    }
}

fn variant_payload_slot_layout(ty: &TypeRepr) -> (usize, usize) {
    match ty {
        TypeRepr::Unit | TypeRepr::Bool => (1, 1),
        TypeRepr::Int | TypeRepr::Float => (8, 8),
        _ => (4, 4),
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
    bounds: Bounds,
    depth: usize,
) -> Result<(), VerifyError> {
    let count = read_u32(bytes, ptr, field, "list length prefix", bounds)?;
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
            bounds.require_span(field, "inline list payload", payload_start, byte_len)
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
                let entry_ptr = read_u32(bytes, entry_off, field, "list entry pointer", bounds)?;
                // Recurse per element type. The element of a pointer-
                // array list is itself String / Schema / List.
                verify_pointer_target(
                    bytes,
                    field,
                    element,
                    element_list_kind(element),
                    entry_ptr,
                    bounds,
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
        // String / Schema / Option / Result elements dispatch on the type
        // directly and never read a nested list sidecar.
        return None;
    };
    list_kind_for_list_type(element)
}

fn list_kind_for_list_type(ty: &TypeRepr) -> Option<ListElementKind> {
    let TypeRepr::List { .. } = ty else {
        return None;
    };
    let probe = Schema {
        name: "<probe>".to_string(),
        generics: vec![],
        is_tuple: false,
        fields: vec![Field {
            name: "f".to_string(),
            ty: ty.clone(),
            default: None,
        }],
    };
    SchemaLayout::offsets_for(&probe)
        .ok()
        .and_then(|t| t.fields.into_iter().next())
        .and_then(|fo| fo.list_element)
}

/// Read a little-endian `u32` at `off`, first proving `[off, off+4)` is
/// in-bounds under `bounds`. Returns the value as a `usize`.
fn read_u32(
    bytes: &[u8],
    off: usize,
    field: &str,
    what: &'static str,
    bounds: Bounds,
) -> Result<usize, VerifyError> {
    bounds.require_span(field, what, off, 4)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[off..off + 4]);
    Ok(u32::from_le_bytes(buf) as usize)
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
        TypeRepr::Unit => "Unit",
        TypeRepr::Bool => "Bool",
        TypeRepr::Int => "Int",
        TypeRepr::Float => "Float",
        TypeRepr::String => "String",
        TypeRepr::List { .. } => "List",
        TypeRepr::Option { .. } => "Option",
        TypeRepr::Result { .. } => "Result",
        TypeRepr::Enum { .. } => "Enum",
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("city", TypeRepr::String), field("zip", TypeRepr::Int)],
        };
        let usr = Schema {
            name: "Usr".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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

    // ---- F5 doubly-nested pointer array (`List<List<String>>`) ----------
    //
    // The verifier must recurse all the way to the **innermost** String
    // record: outer entry -> inner list header -> inner entry -> String
    // record. These adversarial probes smash a pointer at each depth and
    // assert a loud reject (never a wild read past the region).

    fn list_str(items: &[&str]) -> crate::value::Value {
        crate::value::Value::List(std::sync::Arc::new(
            items
                .iter()
                .map(|s| crate::value::Value::String((*s).into()))
                .collect(),
        ))
    }

    /// Build a `Ret { value: List<List<String>> }` buffer and return the
    /// bytes plus the `value` slot's `(list_element, outer header offset)`.
    fn list_list_string_buffer() -> (Vec<u8>, Option<ListElementKind>, usize) {
        let schema = Schema {
            name: "Ret".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field("value", list(list(TypeRepr::String)))],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        let rows = vec![list_str(&["a", "", "bb"]), list_str(&[]), list_str(&["zz"])];
        crate::buffer::write_nested_pointer_array_list(&mut b, "value", &TypeRepr::String, &rows)
            .expect("write nested pointer-array list");
        let bytes = b.finish();
        let fo = &layout.fields[0];
        let mut slot = [0u8; 4];
        slot.copy_from_slice(&bytes[fo.offset..fo.offset + 4]);
        let header_off = u32::from_le_bytes(slot) as usize;
        (bytes, fo.list_element, header_off)
    }

    fn list_list_string_ty() -> TypeRepr {
        list(list(TypeRepr::String))
    }

    #[test]
    fn inplace_list_list_string_verifies_clean() {
        let (bytes, le, root) = list_list_string_buffer();
        let region = Region::new(0, bytes.len()).expect("region");
        verify_value_at(&bytes, &list_list_string_ty(), le, root, region)
            .expect("a legal List<List<String>> must verify to the innermost String");
    }

    #[test]
    fn inplace_list_list_string_corrupt_outer_entry_rejected() {
        // Smash the first outer entry pointer so the inner list header it
        // names lands off-region.
        let (mut bytes, le, root) = list_list_string_buffer();
        let off0 = root + 4;
        let bogus = (bytes.len() as u32 + 4096).to_le_bytes();
        bytes[off0..off0 + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_list_string_ty(), le, root, region)
            .expect_err("a corrupt outer entry must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_list_string_corrupt_inner_entry_rejected() {
        // Follow the first outer entry to its inner list header, then smash
        // that header's first entry (which points at a String record) so the
        // String record escapes the region. Proves the verifier recurses
        // outer -> inner header -> inner entry.
        let (mut bytes, le, root) = list_list_string_buffer();
        // outer off_0 -> inner header.
        let mut o0 = [0u8; 4];
        o0.copy_from_slice(&bytes[root + 4..root + 8]);
        let inner_header = u32::from_le_bytes(o0) as usize;
        // inner header: [len][inner_off_0]…; smash inner_off_0.
        let inner_off0 = inner_header + 4;
        let bogus = (bytes.len() as u32 + 8192).to_le_bytes();
        bytes[inner_off0..inner_off0 + 4].copy_from_slice(&bogus);
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_list_string_ty(), le, root, region)
            .expect_err("a corrupt inner entry must be rejected");
        assert!(
            matches!(err, VerifyError::OutOfRegion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inplace_list_list_string_overlong_innermost_str_rejected() {
        // Reach the innermost String record (outer_0 -> inner header ->
        // inner_off_0 -> String record) and inflate its length prefix so the
        // utf8 payload runs off the region. The deepest-level bounds check
        // must catch it — proof the recursion reaches the String layer.
        let (mut bytes, le, root) = list_list_string_buffer();
        let mut o0 = [0u8; 4];
        o0.copy_from_slice(&bytes[root + 4..root + 8]);
        let inner_header = u32::from_le_bytes(o0) as usize;
        let mut i0 = [0u8; 4];
        i0.copy_from_slice(&bytes[inner_header + 4..inner_header + 8]);
        let str_rec = u32::from_le_bytes(i0) as usize;
        bytes[str_rec..str_rec + 4].copy_from_slice(&0xFFFF_F000u32.to_le_bytes());
        let region = Region::new(0, bytes.len()).expect("region");
        let err = verify_value_at(&bytes, &list_list_string_ty(), le, root, region)
            .expect_err("an overlong innermost String must be rejected");
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

    // ---- multi-region walk (F0 cross-region safety net) -----------------
    //
    // These build a synthetic arena by hand the way the F1 cross-region
    // codegen will (object head in `out`, parameter-sourced field data in
    // `in`, every pointer arena-absolute) and drive the multi-region
    // verifier directly — F0 ships the safety net, not the cap release,
    // so the bytes are laid out here rather than produced by a compiled
    // backend.

    /// Lay out a fixed four-region arena and return `(arena, multi)`.
    /// Layout (absolute byte offsets):
    /// `const = [0, 16)`, `in = [16, 16+in_len)`,
    /// `out = [out_start, out_start+out_len)`, `scratch = [.., end)`.
    /// Each region is generously padded so a test can place records freely.
    fn arena_with_regions(
        in_bytes: &[u8],
        out_bytes: &[u8],
    ) -> (Vec<u8>, MultiRegion, usize, usize) {
        let const_len = 16usize;
        let in_start = const_len;
        let in_len = in_bytes.len().max(4);
        let in_end = in_start + in_len;
        // 8-byte gap so the regions are clearly disjoint, never adjacent.
        let out_start = in_end + 8;
        let out_len = out_bytes.len().max(4);
        let out_end = out_start + out_len;
        let scratch_start = out_end + 8;
        let scratch_len = 16usize;
        let arena_size = scratch_start + scratch_len;

        let mut arena = vec![0u8; arena_size];
        arena[in_start..in_start + in_bytes.len()].copy_from_slice(in_bytes);
        arena[out_start..out_start + out_bytes.len()].copy_from_slice(out_bytes);

        let multi = MultiRegion::new(
            (0, const_len),
            (in_start, in_end),
            (out_start, out_end),
            (scratch_start, arena_size),
        )
        .expect("multi region");
        (arena, multi, in_start, out_start)
    }

    /// Encode a String record `[len: u32 LE][utf8]` into `buf` and return
    /// its byte length.
    fn string_record(s: &str) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + s.len());
        v.extend_from_slice(&(s.len() as u32).to_le_bytes());
        v.extend_from_slice(s.as_bytes());
        v
    }

    /// The F1 cross-region shape `Cfg { name: String, port: Int }`: the
    /// record head lives in `out`, but `name`'s String record lives in
    /// `in`, reached by an **arena-absolute** pointer in the head's slot.
    fn cross_region_cfg() -> Schema {
        Schema {
            name: "Cfg".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![
                field("name", TypeRepr::String),
                field("port", TypeRepr::Int),
            ],
        }
    }

    /// Build the cross-region `Cfg` arena. Returns `(arena, multi, layout,
    /// schema, out_record_base, name_string_abs)`.
    #[allow(clippy::type_complexity)]
    fn cross_region_cfg_arena() -> (Vec<u8>, MultiRegion, OffsetTable, Schema, usize, usize) {
        let schema = cross_region_cfg();
        let layout = SchemaLayout::offsets_for(&schema).expect("cfg layout");
        // `in` region carries just the String record for `name`.
        let in_bytes = string_record("ada");
        // `out` region carries the Cfg fixed area; fill the slots after we
        // know the absolute offsets.
        let out_bytes = vec![0u8; layout.root_size];
        let (mut arena, multi, in_start, out_start) = arena_with_regions(&in_bytes, &out_bytes);

        let name_fo = layout
            .fields
            .iter()
            .find(|fo| fo.name == "name")
            .expect("name fo");
        let port_fo = layout
            .fields
            .iter()
            .find(|fo| fo.name == "port")
            .expect("port fo");
        // `name` slot holds the arena-absolute offset of the String record
        // (which sits at `in_start`).
        let name_string_abs = in_start;
        let name_slot = out_start + name_fo.offset;
        arena[name_slot..name_slot + 4].copy_from_slice(&(name_string_abs as u32).to_le_bytes());
        // `port` is an inline Int.
        let port_slot = out_start + port_fo.offset;
        arena[port_slot..port_slot + 8].copy_from_slice(&8080i64.to_le_bytes());

        (arena, multi, layout, schema, out_start, name_string_abs)
    }

    #[test]
    fn multi_region_cross_region_record_verifies_clean() {
        // The legal F1 shape: head in `out`, String field in `in`. The
        // multi-region verifier must accept the cross-region pointer the
        // single-region wall would (correctly, for S1-S6) reject.
        let (arena, multi, layout, schema, base, _) = cross_region_cfg_arena();
        verify_record_multi(&arena, &layout, &schema.fields, base, multi)
            .expect("a legal cross-region record must verify under the multi-region map");
    }

    #[test]
    fn multi_region_pointer_to_no_region_rejected() {
        // Smash the `name` slot so it points into the inter-region gap
        // (between `in` and `out`) — a span that fits no region. The
        // multi-region verifier must reject loudly, never over-read.
        let (mut arena, multi, layout, schema, base, _) = cross_region_cfg_arena();
        let name_fo = layout.fields.iter().find(|fo| fo.name == "name").unwrap();
        let name_slot = base + name_fo.offset;
        // The gap between `in_end` and `out_start`: in_start=16, in_len=8
        // (string "ada" record is 7 bytes -> max(7,4)=7? string_record is
        // 4+3=7, so in_len=max(7,4)=7, in_end=23, gap=[23,31)). Point at 24.
        let in_start = 16usize;
        let in_record_len = string_record("ada").len();
        let in_len = in_record_len.max(4);
        let gap_off = in_start + in_len + 1; // strictly inside the gap
        arena[name_slot..name_slot + 4].copy_from_slice(&(gap_off as u32).to_le_bytes());
        let err = verify_record_multi(&arena, &layout, &schema.fields, base, multi)
            .expect_err("a pointer into the inter-region gap must be rejected");
        assert!(matches!(err, VerifyError::NoRegion { .. }), "got {err:?}");
    }

    #[test]
    fn multi_region_pointer_payload_runs_off_region_rejected() {
        // Keep the `name` pointer landing in `in`, but inflate the String
        // record's length prefix so the utf8 payload runs past `in_end`
        // into the gap. The payload span starts in `in` but escapes it —
        // must be rejected (no "fell into the next region" over-read).
        let (mut arena, multi, layout, schema, base, name_abs) = cross_region_cfg_arena();
        // The String record's len prefix sits at `name_abs`.
        arena[name_abs..name_abs + 4].copy_from_slice(&0xFFFF_F000u32.to_le_bytes());
        let err = verify_record_multi(&arena, &layout, &schema.fields, base, multi)
            .expect_err("an overlong String payload escaping its region must be rejected");
        assert!(matches!(err, VerifyError::NoRegion { .. }), "got {err:?}");
    }

    #[test]
    fn multi_region_record_head_in_no_region_rejected() {
        // Anchor the record head at an absolute offset that fits no
        // region (in the gap). The fixed-area check must reject before any
        // slot is read.
        let (arena, multi, layout, schema, _, _) = cross_region_cfg_arena();
        let in_start = 16usize;
        let in_len = string_record("ada").len().max(4);
        let gap_base = in_start + in_len + 1;
        let err = verify_record_multi(&arena, &layout, &schema.fields, gap_base, multi)
            .expect_err("a record head fitting no region must be rejected");
        assert!(matches!(err, VerifyError::NoRegion { .. }), "got {err:?}");
    }

    /// Build a cross-region `-> Dict { servers: List<Cfg>, n: Int }` style
    /// shape: an outer object head in `out`, a `servers` field pointing at
    /// a `List<Cfg>` header **in `in`** whose entries and sub-records all
    /// live in `in` (the parameter-sourced identity return). This is the
    /// canonical F1+ cross-region object the multi-region verifier must
    /// certify.
    #[allow(clippy::type_complexity)]
    fn cross_region_dict_of_list_cfg() -> (Vec<u8>, MultiRegion, OffsetTable, Schema, usize) {
        // Inner element schema.
        let cfg = cross_region_cfg();
        let cfg_layout = SchemaLayout::offsets_for(&cfg).expect("cfg layout");
        // Outer object schema: { servers: List<Cfg>, n: Int }.
        let outer = Schema {
            name: "Out".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![
                field(
                    "servers",
                    list(TypeRepr::Schema {
                        schema: Box::new(cfg.clone()),
                    }),
                ),
                field("n", TypeRepr::Int),
            ],
        };
        let outer_layout = SchemaLayout::offsets_for(&outer).expect("outer layout");

        // --- build the `in` region: a `List<Cfg>` with 2 entries ---------
        // Layout inside `in` (relative offsets we fix up to absolute):
        //   [list header: len=2][off_0][off_1]
        //   [cfg_0 fixed area][cfg_1 fixed area]
        //   [name_0 string][name_1 string]
        // We assemble it region-locally then place at `in_start`.
        let two = 2u32;
        let header_rel = 0usize;
        let entries_rel = header_rel + 4; // off_0, off_1
        let cfg0_rel = entries_rel + 8;
        let cfg1_rel = cfg0_rel + cfg_layout.root_size;
        let name0_rec = string_record("alpha");
        let name1_rec = string_record("beta");
        let name0_rel = cfg1_rel + cfg_layout.root_size;
        let name1_rel = name0_rel + name0_rec.len();
        let in_len = name1_rel + name1_rec.len();

        let name_fo = cfg_layout.fields.iter().find(|f| f.name == "name").unwrap();
        let port_fo = cfg_layout.fields.iter().find(|f| f.name == "port").unwrap();

        // We don't yet know in_start; build with a placeholder then patch.
        // To keep it simple, compute in_start from arena_with_regions by
        // building a zero in-buffer of the right length first.
        let in_placeholder = vec![0u8; in_len];
        let out_placeholder = vec![0u8; outer_layout.root_size];
        let (mut arena, multi, in_start, out_start) =
            arena_with_regions(&in_placeholder, &out_placeholder);

        // Now fill `in` with absolute offsets.
        let put_u32 = |arena: &mut [u8], abs: usize, v: u32| {
            arena[abs..abs + 4].copy_from_slice(&v.to_le_bytes());
        };
        let put_i64 = |arena: &mut [u8], abs: usize, v: i64| {
            arena[abs..abs + 8].copy_from_slice(&v.to_le_bytes());
        };
        // list header len
        put_u32(&mut arena, in_start + header_rel, two);
        // entry pointers (absolute) -> cfg fixed areas
        put_u32(
            &mut arena,
            in_start + entries_rel,
            (in_start + cfg0_rel) as u32,
        );
        put_u32(
            &mut arena,
            in_start + entries_rel + 4,
            (in_start + cfg1_rel) as u32,
        );
        // cfg_0: name -> name0 (absolute), port inline
        put_u32(
            &mut arena,
            in_start + cfg0_rel + name_fo.offset,
            (in_start + name0_rel) as u32,
        );
        put_i64(&mut arena, in_start + cfg0_rel + port_fo.offset, 1);
        // cfg_1: name -> name1 (absolute), port inline
        put_u32(
            &mut arena,
            in_start + cfg1_rel + name_fo.offset,
            (in_start + name1_rel) as u32,
        );
        put_i64(&mut arena, in_start + cfg1_rel + port_fo.offset, 2);
        // name strings
        arena[in_start + name0_rel..in_start + name0_rel + name0_rec.len()]
            .copy_from_slice(&name0_rec);
        arena[in_start + name1_rel..in_start + name1_rel + name1_rec.len()]
            .copy_from_slice(&name1_rec);

        // --- fill the `out` object head ----------------------------------
        let servers_fo = outer_layout
            .fields
            .iter()
            .find(|f| f.name == "servers")
            .unwrap();
        let n_fo = outer_layout.fields.iter().find(|f| f.name == "n").unwrap();
        // `servers` slot -> the List<Cfg> header in `in` (cross-region).
        put_u32(
            &mut arena,
            out_start + servers_fo.offset,
            (in_start + header_rel) as u32,
        );
        put_i64(&mut arena, out_start + n_fo.offset, 42);

        (arena, multi, outer_layout, outer, out_start)
    }

    #[test]
    fn multi_region_dict_of_list_cfg_verifies_clean() {
        // The full F1 cross-region object: object head in `out`, a
        // `List<Cfg>` field whose header, entries, sub-records, and every
        // String field all live in `in`. The multi-region verifier must
        // certify the whole graph clean.
        let (arena, multi, layout, schema, base) = cross_region_dict_of_list_cfg();
        verify_record_multi(&arena, &layout, &schema.fields, base, multi).expect(
            "a legal cross-region Dict { servers: List<Cfg>, n: Int } must verify multi-region",
        );
    }

    #[test]
    fn multi_region_dict_of_list_cfg_corrupt_subrecord_field_rejected() {
        // Smash one sub-record's `name` field pointer (deep in `in`) so it
        // points at no region; the multi-region verifier must follow the
        // cross-region link all the way into the sub-record field layer
        // and reject loudly. The most easily-missed depth.
        let (mut arena, multi, layout, schema, base) = cross_region_dict_of_list_cfg();
        // Re-derive the absolute offset of cfg_0's name slot the same way
        // the builder did, then smash it.
        let cfg = cross_region_cfg();
        let cfg_layout = SchemaLayout::offsets_for(&cfg).expect("cfg layout");
        let name_fo = cfg_layout.fields.iter().find(|f| f.name == "name").unwrap();
        let in_start = 16usize;
        let entries_rel = 4usize;
        let cfg0_rel = entries_rel + 8;
        let name_slot = in_start + cfg0_rel + name_fo.offset;
        // Point it far past the arena so it fits no region for sure
        // (recomputing the exact inter-region gap here is brittle; the
        // "fits no region" catch is what we assert).
        let bogus = (arena.len() as u32) + 4096;
        arena[name_slot..name_slot + 4].copy_from_slice(&bogus.to_le_bytes());
        let err = verify_record_multi(&arena, &layout, &schema.fields, base, multi)
            .expect_err("a corrupt cross-region sub-record field pointer must be rejected");
        assert!(matches!(err, VerifyError::NoRegion { .. }), "got {err:?}");
    }

    #[test]
    fn multi_region_buffer_shorter_than_regions_rejected() {
        // A `multi` whose largest region end exceeds the slice length must
        // be rejected up front, never indexed past.
        let multi = MultiRegion::new((0, 8), (8, 16), (16, 64), (64, 80)).expect("multi");
        let schema = cross_region_cfg();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let short = vec![0u8; 16];
        let err = verify_record_multi(&short, &layout, &schema.fields, 16, multi)
            .expect_err("a slice shorter than the region span must be rejected");
        assert!(
            matches!(
                err,
                VerifyError::BufferShorterThanRegion { have: 16, end: 80 }
            ),
            "got {err:?}"
        );
    }
}
