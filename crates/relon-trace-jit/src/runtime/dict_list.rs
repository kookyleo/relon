//! F-D8: host-side helpers backing `TraceOp::ListGet` /
//! `TraceOp::DictLookup`.
//!
//! The cranelift emitter lowers `TraceOp::DictLookup` to
//! `call __relon_trace_dict_lookup_v2(dict_ptr, record_len, key_ptr,
//! shape_hash, ctx)` and lowers `TraceOp::ListGet` to either:
//!
//! - An inline bounds-checked load against a flat
//!   `[len: u32 LE][pad: u32][i64 elements...]` record (the
//!   cranelift-AOT-shaped layout reused by the bench fixtures), OR
//! - A `call __relon_trace_list_get(list_ptr, idx, ctx)` helper for
//!   `Arc<Vec<Value>>`-backed lists where the per-element layout is
//!   `Value` rather than raw `i64` (the production case).
//!
//! The inline shape is what the W6 trace JIT bench exercises — every
//! per-iter cost is one bounds compare + one i64 load with the loop
//! body running inside cranelift's compiled trace. The helper path is
//! the fall-back for lists whose elements are not a flat i64 (Value
//! enum, tagged sums, …) and is documented here for completeness.
//!
//! ## Layout contract
//!
//! The flat list record is byte-identical to the one
//! `relon_ir::Op::ConstListInt` / `relon_ir::Op::LoadListIntPtr`
//! produce in the cranelift-AOT backend's data section:
//!
//! ```text
//! offset 0  : len  : u32 LE   (element count)
//! offset 4  : pad  : u32 zero (alignment slack, never read)
//! offset 8  : elements[0..len], 8 bytes each, little-endian i64
//! ```
//!
//! ## Dict layout
//!
//! Active trace dict lowering uses the v2 record envelope: the host
//! pre-computes the shape hash, stores key-hash metadata in the entry
//! table, and also stores key payload bytes in the same record. The
//! helper receives the record byte length from the recorder side table
//! and validates the entry table / payload ranges before comparing key
//! bytes on a hash hit. That makes payload-distinct FxHash collisions
//! safe: they continue scanning or deopt instead of returning the
//! wrong value.
//!
//! The older v1 helpers in this module are gated behind `#[cfg(test)]`
//! and only exist for in-crate regression tests that pin the legacy
//! layout (review #179 P3 — production builds carry zero v1 surface).
//! All active emitter / installer / bench paths use the v2 symbols
//! below.
//!
//! ## Why this lives in `relon-trace-jit`
//!
//! The trace JIT runtime owns the cranelift-side ABI; growing the
//! dep graph (e.g. routing through `relon-eval-api::Value`) would
//! pull a much larger dependency into a crate whose other helpers
//! (`__relon_trace_save_deopt`, `__relon_trace_inline_cache_lookup`)
//! stay deliberately small. The helpers here work on raw byte
//! pointers; the host is responsible for arranging the layout before
//! the trace runs (the bench fixtures hand-build the buffer; the
//! production path would lower `Value::List(Arc<Vec<i64>>)` into the
//! same shape inside the recorder driver).

use crate::runtime::TraceContext;

/// Sentinel returned by the list / dict helpers on out-of-range index,
/// IC shape mismatch, malformed record envelope, or missing key. The
/// cranelift emitter compares against this value and branches into the
/// shared deopt block when the helper signals failure.
///
/// `i64::MIN` is the most compact encoding for cranelift — `cmp r,
/// imm64` lowers to a single x86_64 `cmp` against an immediate that
/// the assembler folds into the encoding. Hosts must arrange for
/// `i64::MIN` to be an impossible legitimate dict / list value
/// (Relon's `Int` range is bounded; the parser rejects `-2^63` as
/// out-of-range, so this is a safe sentinel).
pub const DICT_LOOKUP_DEOPT: i64 = i64::MIN;

/// Inline-cached fast path for a `[len][pad][i64...]` list record.
///
/// Returns the i64 element at `idx`. Out-of-range access returns
/// [`DICT_LOOKUP_DEOPT`] so the trace deopts.
///
/// # Safety
///
/// - `list_ptr` must point at a record whose layout is
///   `[len: u32 LE][pad: u32 LE][i64 elements...]`, with at least
///   `8 + 8 * len` valid bytes.
/// - `ctx` must be a valid pointer for the calling trace; it is
///   currently unused but reserved so future revisions can plumb the
///   IC eviction path through the context's pending-write log
///   without an ABI break.
/// - Callers must run on the thread that owns `ctx`.
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_list_get(
    list_ptr: *const u8,
    idx: i64,
    ctx: *mut TraceContext,
) -> i64 {
    let _ = ctx; // reserved for future bookkeeping
    if list_ptr.is_null() {
        return DICT_LOOKUP_DEOPT;
    }
    // SAFETY: caller contract — the layout is canonical and the
    // pointer is live for the duration of the call.
    let len = unsafe { (list_ptr as *const u32).read_unaligned() };
    if idx < 0 || (idx as u64) >= u64::from(len) {
        return DICT_LOOKUP_DEOPT;
    }
    let elem_addr = unsafe { list_ptr.add(8).add((idx as usize) * 8) };
    // SAFETY: bounds-checked above; the elements are 8-byte aligned
    // by the layout contract.
    unsafe { (elem_addr as *const i64).read_unaligned() }
}

/// Legacy IC-guarded dict lookup helper (v1, hash-only). Test-only
/// — gated behind `#[cfg(test)]` so production builds carry zero v1
/// surface; the active emitter / installer paths route through the
/// collision-safe v2 helpers below.
///
/// `dict_ptr` is laid out as:
///
/// ```text
/// offset 0  : shape_hash : u64 LE      (recorder-time fingerprint)
/// offset 8  : entry_count : u32 LE
/// offset 12 : entries[0..entry_count]  each: [key_hash: u64][value: i64]
/// ```
///
/// On IC hit the helper hashes the supplied `key_ptr` (`[len:
/// u32][utf8...]` record) and scans the entry table for a matching
/// `key_hash`; on hit it returns the cached `value`. On shape
/// mismatch (`dict_ptr.shape_hash != shape_hash`) the helper returns
/// [`DICT_LOOKUP_DEOPT`] so the trace deopts and the recorder
/// re-specialises under the new shape.
///
/// # ⚠️ Hash collision caveat (review #175 P2)
///
/// The v1 layout stores **only** `(key_hash, value)` per entry and
/// has **no key payload** to byte-compare. The helper therefore
/// returns the first entry whose 64-bit FxHash matches the looked-up
/// key — a payload-distinct FxHash64 collision would silently return
/// the wrong value. Review #179 P3 closed that exposure by gating the
/// v1 helper behind `#[cfg(test)]`; only the regression tests below
/// reach it.
///
/// # Safety
///
/// Same shape contract as [`__relon_trace_list_get`]; both pointers
/// must satisfy the documented layouts and outlive the call.
#[cfg(test)]
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_dict_lookup(
    dict_ptr: *const u8,
    key_ptr: *const u8,
    shape_hash: u64,
    ctx: *mut TraceContext,
) -> i64 {
    let _ = ctx;
    if dict_ptr.is_null() || key_ptr.is_null() {
        return DICT_LOOKUP_DEOPT;
    }

    // SAFETY: caller-supplied layout guarantees the first 8 bytes are
    // the shape hash and the next 4 bytes are the entry count. The
    // emitter passes `shape_hash` as a per-trace immediate captured
    // at recording time; mismatch ⇒ deopt.
    let dict_shape = unsafe { (dict_ptr as *const u64).read_unaligned() };
    if dict_shape != shape_hash {
        return DICT_LOOKUP_DEOPT;
    }

    let entry_count = unsafe { (dict_ptr.add(8) as *const u32).read_unaligned() };
    let key_hash = unsafe { fx_hash_key_record(key_ptr) };
    let entries_base = unsafe { dict_ptr.add(12) };
    for i in 0..entry_count {
        // SAFETY: bounds-checked by `entry_count`.
        let entry_off = (i as usize) * 16;
        let entry_key_hash =
            unsafe { (entries_base.add(entry_off) as *const u64).read_unaligned() };
        if entry_key_hash == key_hash {
            let entry_val =
                unsafe { (entries_base.add(entry_off + 8) as *const i64).read_unaligned() };
            return entry_val;
        }
    }
    // Key not found: deopt and let the slow path raise the right
    // user-facing error. (Production wiring would route through the
    // BTreeMap lookup here for correctness; the bench fixtures
    // pre-populate so this branch is unreachable on the hit path.)
    DICT_LOOKUP_DEOPT
}

/// Legacy F-D8-E.2: "shape already checked" companion of
/// [`__relon_trace_dict_lookup`].
///
/// Historical cranelift lowering used this helper for
/// `TraceOp::DictLookupPrechecked`. It is byte-identical to the full
/// [`__relon_trace_dict_lookup`] except for the leading `dict_shape !=
/// shape_hash` compare — the caller must have already executed a
/// preceding `TraceOp::DictShapeGuard` (inline `load + cmp + brif
/// deopt`) for the same `(dict_ptr, shape_hash)` pair, so doing the
/// compare again on every iteration would be pure dead work.
///
/// When the optimizer's `dict_ic_hoist` pass identifies a
/// `TraceOp::DictLookup` whose `dict_ptr` SSA is loop-invariant, it
/// splits the op into:
///
/// - one `TraceOp::DictShapeGuard { dict_ptr, shape_hash }` that LICM
///   then lifts to the loop entry, executing the compare exactly once
///   per trace entry;
/// - one `TraceOp::DictLookupPrechecked { dst, dict_ptr, key_ptr }`
///   that stays inside the loop body and routes here.
///
/// The W5 cmp_lua benchmark's hot loop accesses a fixed dict with a
/// per-iter-varying string key (`d[keys[i % 10]]`); the shape compare
/// is the only invariant within the dict-lookup helper, so the
/// per-iter saving is just one `load.u64 + cmp + brif`. That is enough
/// to drop the trace_jit ratio from × 1.95 toward × 1.5 LuaJIT on the
/// F-D8-D recorder-driven path — see the F-D8-E.2 stage report.
///
/// # ⚠️ Legacy test-only safety (review #179 P3)
///
/// Inherits the hash-only collision caveat of
/// [`__relon_trace_dict_lookup`] **and** drops the shape-fingerprint
/// safety net. Active emitter / installer paths route through
/// [`__relon_trace_dict_lookup_prechecked_v2`]; this helper is now
/// gated behind `#[cfg(test)]` so production builds carry no v1
/// surface at all and the legacy regression tests can still pin the
/// historical layout contract.
///
/// # Safety
///
/// - Same shape contract as [`__relon_trace_dict_lookup`].
/// - Caller MUST have executed a matching `TraceOp::DictShapeGuard`
///   earlier in the trace, otherwise a dict whose layout drifted from
///   the recorder-time fingerprint would silently scan into garbage
///   entries. The optimizer is the only producer of
///   `DictLookupPrechecked` ops, and it pairs them by construction.
#[cfg(test)]
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_dict_lookup_prechecked(
    dict_ptr: *const u8,
    key_ptr: *const u8,
    ctx: *mut TraceContext,
) -> i64 {
    let _ = ctx;
    if dict_ptr.is_null() || key_ptr.is_null() {
        return DICT_LOOKUP_DEOPT;
    }

    // SAFETY: caller contract — the dict layout is canonical and a
    // matching `DictShapeGuard` already verified the shape header.
    let entry_count = unsafe { (dict_ptr.add(8) as *const u32).read_unaligned() };
    let key_hash = unsafe { fx_hash_key_record(key_ptr) };
    let entries_base = unsafe { dict_ptr.add(12) };
    for i in 0..entry_count {
        let entry_off = (i as usize) * 16;
        let entry_key_hash =
            unsafe { (entries_base.add(entry_off) as *const u64).read_unaligned() };
        if entry_key_hash == key_hash {
            let entry_val =
                unsafe { (entries_base.add(entry_off + 8) as *const i64).read_unaligned() };
            return entry_val;
        }
    }
    DICT_LOOKUP_DEOPT
}

// Re-exported from `relon-trace-abi` so the producer side (an
// analyzer / IR pass that pre-stamps `Op::DictGetByStringKey::shape_hash`)
// and the consumer side (this runtime's IC dispatch) share the same
// canonical FxHash impl. Implementing the algorithm twice would risk
// silent IC misses; centralising in `relon-trace-abi` keeps both
// callers locked to the same bytes.
pub use relon_trace_abi::hash::{
    fx_hash_bytes, fx_hash_key_record, fx_hash_key_record_payload, is_ascii_bytes,
    is_ascii_flag_set, STRING_RECORD_ASCII_FLAG_BIT, STRING_RECORD_HASH_OFFSET,
    STRING_RECORD_LEN_MASK, STRING_RECORD_PAYLOAD_OFFSET,
};

/// Convenience constructor: layout-conformant dict key record for
/// unit tests + bench fixtures. Returns a `Vec<u8>` whose layout
/// matches the consumer contract documented on
/// [`fx_hash_key_record`]:
///
/// ```text
/// offset 0  : len_with_flags : u32 LE
///               bits 0..31 — payload byte count
///               bit 31     — Tier 2c ASCII-flag bit (set ⇒ payload
///                            is all `< 0x80`)
/// offset 4  : hash           : u64 LE  (pre-computed fx_hash_bytes(payload))
/// offset 12 : bytes          : [u8; len] (UTF-8 payload)
/// ```
///
/// Pre-stamping the FxHash at fixture-build time is what makes the
/// W5-class dict hot loop avoid re-hashing each key byte on every
/// iteration — the inline emitter and the runtime helpers both just
/// `load.u64 [key_ptr + STRING_RECORD_HASH_OFFSET]` instead of running
/// the byte-wise hash loop. See the Tier 1a stage report for the
/// before/after numbers.
///
/// Tier 2c adds the ASCII-flag bit in lockstep — the producer scans
/// the payload once here (~3 cycles / byte after auto-vectorisation)
/// and Unicode-heavy stdlib bodies (`upper` / `lower` / `title` /
/// `normalize`) probe the bit on the consumer side to skip the
/// per-codepoint UCD table walk on pure-ASCII inputs.
///
/// The legacy 4-byte-header layout that this used to produce is gone:
/// every consumer of this helper (dict_inline.rs, the W5/W6 bench
/// fixtures, the recorder tests) was updated in the same commit
/// sequence so there is no compatibility surface to preserve.
///
/// # Panics
///
/// Debug-mode panics when `s.len() >= 2^31`. The high bit of the
/// 32-bit header is reserved for the ASCII flag; a 2 GiB+ payload
/// would silently clobber it in release builds. Production dict keys
/// are interned identifiers many orders of magnitude under this cap.
pub fn build_string_record(s: &str) -> Vec<u8> {
    debug_assert!(
        (s.len() as u64) < (STRING_RECORD_ASCII_FLAG_BIT as u64),
        "dict key payload exceeds 2^31 bytes — ASCII flag bit would overflow into the length"
    );
    let payload = s.as_bytes();
    let mut len_with_flags = s.len() as u32;
    if is_ascii_bytes(payload) {
        len_with_flags |= STRING_RECORD_ASCII_FLAG_BIT;
    }
    let hash = fx_hash_bytes(payload);
    let mut out = Vec::with_capacity(STRING_RECORD_PAYLOAD_OFFSET as usize + s.len());
    out.extend_from_slice(&len_with_flags.to_le_bytes());
    out.extend_from_slice(&hash.to_le_bytes());
    out.extend_from_slice(payload);
    debug_assert_eq!(out.len(), STRING_RECORD_PAYLOAD_OFFSET as usize + s.len());
    out
}

/// Convenience constructor: layout-conformant flat list record for
/// tests + bench fixtures. Returns
/// `[len: u32 LE][pad: u32 zero][i64 elements LE...]`.
pub fn build_flat_list_record(elements: &[i64]) -> Vec<u8> {
    let len = elements.len() as u32;
    let mut out = Vec::with_capacity(8 + 8 * elements.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // pad
    for v in elements {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Convenience constructor for the v1 dict record. Test-only —
/// gated behind `#[cfg(test)]` because v1 helpers / inline emit are
/// no longer compiled into production builds. Bench fixtures use
/// [`build_dict_record_v2`] instead.
///
/// `shape_hash` is the recorder-time fingerprint stamped into the
/// header; `entries` is the pre-flattened (`key_hash`, `value`)
/// table. Callers compute the per-key FxHash via [`fx_hash_bytes`].
#[cfg(test)]
pub fn build_dict_record(shape_hash: u64, entries: &[(u64, i64)]) -> Vec<u8> {
    let entry_count = entries.len() as u32;
    let mut out = Vec::with_capacity(12 + 16 * entries.len());
    out.extend_from_slice(&shape_hash.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    for (k, v) in entries {
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// =====================================================================
// v2 dict layout — key payload memcmp on hash hit (review #175 P2 fix)
// =====================================================================
//
// Motivation: the v1 layout above stores only `(key_hash, value)` per
// entry and is therefore vulnerable to a 64-bit FxHash collision
// silently returning the wrong value. v1 stays as the inline-emitter
// hot path because its bench-fixture call sites pre-validate every
// key set against collisions and the cranelift-side lowering depends
// on the 16-byte entry stride for the `lea` fold. v2 adds a second
// helper + builder pair that keeps the entry header backwards-
// compatible (same `[key_hash:u64]` first eight bytes) but appends a
// pointer + length pair into the entry body so the helper can do a
// `memcmp` of the looked-up key payload against the stored payload
// after the hash match. The cost is one extra cache line per entry
// plus a per-iter `memcmp` of ~`key.len()` bytes on the hit path —
// acceptable for production dicts where correctness trumps the
// last 5-10 ns/iter on the W5 bench.

/// Byte offset of the v2 dict header's `entry_count` field. See
/// [`build_dict_record_v2`] for the full layout.
pub(crate) const DICT_V2_ENTRY_COUNT_OFFSET: usize = 8;
/// Byte offset of the first entry in a v2 dict record.
pub(crate) const DICT_V2_ENTRIES_OFFSET: usize = 12;
/// v2 entry stride: `[key_hash:u64][key_payload_off:u32][key_payload_len:u32][value:i64]`.
pub(crate) const DICT_V2_ENTRY_STRIDE: usize = 24;

/// v2 dict-record constructor.
///
/// `entries` carries one `(key_payload, value)` per dict entry; the
/// builder pre-hashes each payload (via [`fx_hash_bytes`]) and
/// appends every payload byte-for-byte to the tail of the record so
/// [`__relon_trace_dict_lookup_v2`] can `memcmp` the looked-up key
/// payload against the stored one after the hash match. The returned
/// layout is:
///
/// ```text
/// offset 0  : shape_hash       : u64 LE
/// offset 8  : entry_count      : u32 LE
/// offset 12 : entries[0..N]    each 24 bytes:
///                 [key_hash       : u64 LE]
///                 [key_payload_off: u32 LE]  // bytes from record base
///                 [key_payload_len: u32 LE]
///                 [value          : i64 LE]
/// offset H  : payload_blob     : variable
/// ```
///
/// Each entry's `key_payload_off` is the absolute byte offset (from
/// the record base) of its payload bytes in the trailing blob. The
/// v2 helper validates `(off, len)` against the surrounding record
/// length before issuing the `memcmp`, so a corrupt record cannot
/// drive an out-of-bounds read.
///
/// # Panics
///
/// Debug-mode panics when any key payload's length exceeds
/// `u32::MAX` — production dict keys are interned identifiers many
/// orders of magnitude under this cap.
pub fn build_dict_record_v2(shape_hash: u64, entries: &[(&[u8], i64)]) -> Vec<u8> {
    let entry_count = entries.len() as u32;
    let payload_total: usize = entries.iter().map(|(k, _)| k.len()).sum();
    let header_size = DICT_V2_ENTRIES_OFFSET + DICT_V2_ENTRY_STRIDE * entries.len();
    let mut out = Vec::with_capacity(header_size + payload_total);
    out.extend_from_slice(&shape_hash.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());

    // First pass: emit zeroed entry headers so we can fill them once
    // we know each payload's final absolute offset.
    let mut payload_cursor = header_size;
    for (key, value) in entries {
        debug_assert!(
            (key.len() as u64) <= (u32::MAX as u64),
            "v2 dict key payload exceeds u32::MAX bytes"
        );
        let key_hash = fx_hash_bytes(key);
        let key_off = payload_cursor as u32;
        let key_len = key.len() as u32;
        out.extend_from_slice(&key_hash.to_le_bytes());
        out.extend_from_slice(&key_off.to_le_bytes());
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        payload_cursor += key.len();
    }
    debug_assert_eq!(out.len(), header_size);
    // Second pass: append the payload blob.
    for (key, _) in entries {
        out.extend_from_slice(key);
    }
    debug_assert_eq!(out.len(), header_size + payload_total);
    out
}

/// v2 dict-lookup helper. Performs a `memcmp` of the looked-up key
/// payload against the stored payload after the hash match, so
/// FxHash64 collisions cannot silently return the wrong value.
///
/// `dict_ptr` must point at a record produced by
/// [`build_dict_record_v2`] (or laid out identically). `key_ptr` must
/// point at a layout-conformant string record
/// (`[len_with_flags:u32][hash:u64][payload]`) — the same shape the v1
/// helper consumes.
///
/// On shape-fingerprint mismatch the helper returns
/// [`DICT_LOOKUP_DEOPT`] before reading any entry. On hash match the
/// helper validates `(key_off, key_len)` against the surrounding
/// record bounds (passed via `record_len`) and `memcmp`s the stored
/// payload against the looked-up key payload; mismatched payloads
/// continue the scan, so a hash collision degrades to one extra
/// `memcmp` per collision rather than corrupting the result.
///
/// # Safety
///
/// - `dict_ptr` must point at the first byte of a v2 record whose
///   total length is `record_len`. The helper reads at most
///   `record_len` bytes from `dict_ptr` and bounds-checks every
///   per-entry payload range against `record_len` before issuing the
///   `memcmp`.
/// - `key_ptr` must point at a layout-conformant string record (same
///   contract as the v1 helper).
/// - `ctx` must be a valid `TraceContext` pointer; currently unused
///   but reserved for IC bookkeeping.
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_dict_lookup_v2(
    dict_ptr: *const u8,
    record_len: usize,
    key_ptr: *const u8,
    shape_hash: u64,
    ctx: *mut TraceContext,
) -> i64 {
    let _ = ctx;
    if dict_ptr.is_null() || key_ptr.is_null() {
        return DICT_LOOKUP_DEOPT;
    }
    // Defensive: a record_len smaller than the fixed header is
    // structurally impossible; bail out before any read.
    if record_len < DICT_V2_ENTRIES_OFFSET {
        return DICT_LOOKUP_DEOPT;
    }

    let dict_shape = unsafe { (dict_ptr as *const u64).read_unaligned() };
    if dict_shape != shape_hash {
        return DICT_LOOKUP_DEOPT;
    }

    let entry_count =
        unsafe { (dict_ptr.add(DICT_V2_ENTRY_COUNT_OFFSET) as *const u32).read_unaligned() };
    // Validate the entry table itself fits inside `record_len` — a
    // malformed record could otherwise drive an OOB read on the very
    // first entry header.
    let entries_total = (entry_count as usize).saturating_mul(DICT_V2_ENTRY_STRIDE);
    let entries_end = DICT_V2_ENTRIES_OFFSET.saturating_add(entries_total);
    if entries_end > record_len {
        return DICT_LOOKUP_DEOPT;
    }

    let key_payload_len = unsafe { fx_hash_key_record_payload_len(key_ptr) };
    let key_payload_ptr = unsafe { key_ptr.add(STRING_RECORD_PAYLOAD_OFFSET as usize) };
    let key_hash = unsafe { fx_hash_key_record(key_ptr) };
    let entries_base = unsafe { dict_ptr.add(DICT_V2_ENTRIES_OFFSET) };

    for i in 0..entry_count {
        let entry_off = (i as usize) * DICT_V2_ENTRY_STRIDE;
        let entry_ptr = unsafe { entries_base.add(entry_off) };
        let entry_key_hash = unsafe { (entry_ptr as *const u64).read_unaligned() };
        if entry_key_hash != key_hash {
            continue;
        }
        let stored_off = unsafe { (entry_ptr.add(8) as *const u32).read_unaligned() } as usize;
        let stored_len = unsafe { (entry_ptr.add(12) as *const u32).read_unaligned() } as usize;
        // Bounds-check the stored payload range against the record so
        // a corrupt record can't drive an OOB compare.
        let stored_end = stored_off.saturating_add(stored_len);
        if stored_off < entries_end || stored_end > record_len {
            return DICT_LOOKUP_DEOPT;
        }
        if stored_len != key_payload_len {
            // Same hash, different payload length ⇒ collision; keep
            // scanning instead of returning the wrong value.
            continue;
        }
        let stored_payload_ptr = unsafe { dict_ptr.add(stored_off) };
        // SAFETY: bounds verified above; both slices point at
        // `stored_len` valid bytes inside the live record / key buffer.
        let stored_bytes = unsafe { std::slice::from_raw_parts(stored_payload_ptr, stored_len) };
        let lookup_bytes = unsafe { std::slice::from_raw_parts(key_payload_ptr, key_payload_len) };
        if stored_bytes != lookup_bytes {
            // Hash collision with distinct payload — keep scanning.
            continue;
        }
        let entry_val = unsafe { (entry_ptr.add(16) as *const i64).read_unaligned() };
        return entry_val;
    }
    DICT_LOOKUP_DEOPT
}

/// v2 "shape already checked" companion of
/// [`__relon_trace_dict_lookup_v2`]. Skips the leading shape compare
/// (a paired `DictShapeGuard` ran upstream) but keeps the key payload
/// `memcmp` so hash collisions still degrade safely.
///
/// # Safety
///
/// Same contract as [`__relon_trace_dict_lookup_v2`] plus the
/// upstream-`DictShapeGuard` requirement of the v1 prechecked
/// helper.
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_dict_lookup_prechecked_v2(
    dict_ptr: *const u8,
    record_len: usize,
    key_ptr: *const u8,
    ctx: *mut TraceContext,
) -> i64 {
    let _ = ctx;
    if dict_ptr.is_null() || key_ptr.is_null() {
        return DICT_LOOKUP_DEOPT;
    }
    if record_len < DICT_V2_ENTRIES_OFFSET {
        return DICT_LOOKUP_DEOPT;
    }
    let entry_count =
        unsafe { (dict_ptr.add(DICT_V2_ENTRY_COUNT_OFFSET) as *const u32).read_unaligned() };
    let entries_total = (entry_count as usize).saturating_mul(DICT_V2_ENTRY_STRIDE);
    let entries_end = DICT_V2_ENTRIES_OFFSET.saturating_add(entries_total);
    if entries_end > record_len {
        return DICT_LOOKUP_DEOPT;
    }

    let key_payload_len = unsafe { fx_hash_key_record_payload_len(key_ptr) };
    let key_payload_ptr = unsafe { key_ptr.add(STRING_RECORD_PAYLOAD_OFFSET as usize) };
    let key_hash = unsafe { fx_hash_key_record(key_ptr) };
    let entries_base = unsafe { dict_ptr.add(DICT_V2_ENTRIES_OFFSET) };

    for i in 0..entry_count {
        let entry_off = (i as usize) * DICT_V2_ENTRY_STRIDE;
        let entry_ptr = unsafe { entries_base.add(entry_off) };
        let entry_key_hash = unsafe { (entry_ptr as *const u64).read_unaligned() };
        if entry_key_hash != key_hash {
            continue;
        }
        let stored_off = unsafe { (entry_ptr.add(8) as *const u32).read_unaligned() } as usize;
        let stored_len = unsafe { (entry_ptr.add(12) as *const u32).read_unaligned() } as usize;
        let stored_end = stored_off.saturating_add(stored_len);
        if stored_off < entries_end || stored_end > record_len {
            return DICT_LOOKUP_DEOPT;
        }
        if stored_len != key_payload_len {
            continue;
        }
        let stored_payload_ptr = unsafe { dict_ptr.add(stored_off) };
        let stored_bytes = unsafe { std::slice::from_raw_parts(stored_payload_ptr, stored_len) };
        let lookup_bytes = unsafe { std::slice::from_raw_parts(key_payload_ptr, key_payload_len) };
        if stored_bytes != lookup_bytes {
            continue;
        }
        let entry_val = unsafe { (entry_ptr.add(16) as *const i64).read_unaligned() };
        return entry_val;
    }
    DICT_LOOKUP_DEOPT
}

/// Read the payload length stored in a layout-conformant string
/// record header (bits 0..31 of the `len_with_flags` word).
///
/// # Safety
///
/// Same shape contract as
/// [`relon_trace_abi::hash::fx_hash_key_record`]: `key_ptr` must point
/// at the first byte of a layout-conformant string record with at
/// least 4 valid header bytes plus `len` payload bytes.
#[inline]
unsafe fn fx_hash_key_record_payload_len(key_ptr: *const u8) -> usize {
    let len_with_flags = unsafe { (key_ptr as *const u32).read_unaligned() };
    (len_with_flags & STRING_RECORD_LEN_MASK) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fx_hash_is_deterministic() {
        let a = fx_hash_bytes(b"hello");
        let b = fx_hash_bytes(b"hello");
        assert_eq!(a, b);
        let c = fx_hash_bytes(b"world");
        assert_ne!(a, c);
    }

    #[test]
    fn list_get_returns_element() {
        let buf = build_flat_list_record(&[10, 20, 30, 40, 50]);
        let mut ctx = TraceContext::with_capacity(0);
        for (i, expected) in [10, 20, 30, 40, 50].iter().enumerate() {
            // SAFETY: buf is laid out per contract; ctx is stack-owned.
            let got = unsafe {
                __relon_trace_list_get(buf.as_ptr(), i as i64, &mut ctx as *mut TraceContext)
            };
            assert_eq!(got, *expected);
        }
    }

    #[test]
    fn list_get_out_of_range_returns_deopt_sentinel() {
        let buf = build_flat_list_record(&[1, 2, 3]);
        let mut ctx = TraceContext::with_capacity(0);
        // SAFETY: same contract as above.
        let got =
            unsafe { __relon_trace_list_get(buf.as_ptr(), 99, &mut ctx as *mut TraceContext) };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
        // Negative index also deopts.
        let neg =
            unsafe { __relon_trace_list_get(buf.as_ptr(), -1, &mut ctx as *mut TraceContext) };
        assert_eq!(neg, DICT_LOOKUP_DEOPT);
    }

    #[test]
    fn dict_lookup_hit_returns_value() {
        let key_records: Vec<Vec<u8>> = ["a", "b", "c"]
            .iter()
            .map(|s| build_string_record(s))
            .collect();
        let entries: Vec<(u64, i64)> = key_records
            .iter()
            .enumerate()
            .map(|(i, kr)| {
                // SAFETY: kr is a layout-conformant String record.
                let h = unsafe { fx_hash_key_record(kr.as_ptr()) };
                (h, (i as i64 + 1) * 10)
            })
            .collect();
        let shape: u64 = 0xfeed_face_dead_beef;
        let dict = build_dict_record(shape, &entries);
        let mut ctx = TraceContext::with_capacity(0);

        for (i, kr) in key_records.iter().enumerate() {
            // SAFETY: pointers are layout-conformant + outlive the
            // call; ctx is stack-owned.
            let got =
                unsafe { __relon_trace_dict_lookup(dict.as_ptr(), kr.as_ptr(), shape, &mut ctx) };
            assert_eq!(got, (i as i64 + 1) * 10, "key {} value mismatch", i);
        }
    }

    #[test]
    fn dict_lookup_shape_mismatch_returns_deopt_sentinel() {
        let kr = build_string_record("a");
        let h = unsafe { fx_hash_key_record(kr.as_ptr()) };
        let dict = build_dict_record(0xaaaa, &[(h, 42)]);
        let mut ctx = TraceContext::with_capacity(0);
        // Different shape -> deopt.
        let got =
            unsafe { __relon_trace_dict_lookup(dict.as_ptr(), kr.as_ptr(), 0xbbbb, &mut ctx) };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
        // Matching shape -> hit.
        let got2 =
            unsafe { __relon_trace_dict_lookup(dict.as_ptr(), kr.as_ptr(), 0xaaaa, &mut ctx) };
        assert_eq!(got2, 42);
    }

    #[test]
    fn dict_lookup_missing_key_returns_deopt_sentinel() {
        let kr_present = build_string_record("a");
        let kr_missing = build_string_record("z");
        let h = unsafe { fx_hash_key_record(kr_present.as_ptr()) };
        let dict = build_dict_record(0xc0de, &[(h, 7)]);
        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup(dict.as_ptr(), kr_missing.as_ptr(), 0xc0de, &mut ctx)
        };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
    }

    // ---- F-D8-E.2: prechecked-helper round-trip ---------------------

    #[test]
    fn dict_lookup_prechecked_hit_returns_value() {
        // The prechecked helper skips the shape compare but otherwise
        // matches the full helper's key-hash + scan path. Build the
        // same fixture the full-helper test uses and confirm hits.
        let key_records: Vec<Vec<u8>> = ["a", "b", "c"]
            .iter()
            .map(|s| build_string_record(s))
            .collect();
        let entries: Vec<(u64, i64)> = key_records
            .iter()
            .enumerate()
            .map(|(i, kr)| {
                let h = unsafe { fx_hash_key_record(kr.as_ptr()) };
                (h, (i as i64 + 1) * 10)
            })
            .collect();
        let dict = build_dict_record(0xfeed_face_dead_beef, &entries);
        let mut ctx = TraceContext::with_capacity(0);
        for (i, kr) in key_records.iter().enumerate() {
            let got = unsafe {
                __relon_trace_dict_lookup_prechecked(dict.as_ptr(), kr.as_ptr(), &mut ctx)
            };
            assert_eq!(got, (i as i64 + 1) * 10, "key {} value mismatch", i);
        }
    }

    #[test]
    fn dict_lookup_prechecked_ignores_shape_field() {
        // The whole point of the prechecked path: the caller has
        // already verified the shape via a hoisted DictShapeGuard, so
        // the helper must NOT re-compare. Stamp a deliberately wrong
        // shape into the header and confirm the helper still hits.
        let kr = build_string_record("a");
        let h = unsafe { fx_hash_key_record(kr.as_ptr()) };
        let dict = build_dict_record(0xaaaa_bbbb, &[(h, 42)]);
        let mut ctx = TraceContext::with_capacity(0);
        let got =
            unsafe { __relon_trace_dict_lookup_prechecked(dict.as_ptr(), kr.as_ptr(), &mut ctx) };
        assert_eq!(got, 42);
    }

    #[test]
    fn dict_lookup_prechecked_missing_key_returns_deopt_sentinel() {
        let kr_present = build_string_record("a");
        let kr_missing = build_string_record("z");
        let h = unsafe { fx_hash_key_record(kr_present.as_ptr()) };
        let dict = build_dict_record(0xc0de, &[(h, 7)]);
        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_prechecked(dict.as_ptr(), kr_missing.as_ptr(), &mut ctx)
        };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
    }

    // ---- Tier 2c: ASCII flag is stamped at build time --------------

    #[test]
    fn build_string_record_marks_pure_ascii_payload() {
        let kr = build_string_record("plainkey");
        // Producer side has set bit 31 of the header.
        assert!(unsafe { is_ascii_flag_set(kr.as_ptr()) });
        // Cached hash still matches the byte-wise reference.
        let cached = unsafe { fx_hash_key_record(kr.as_ptr()) };
        let recomputed = unsafe { fx_hash_key_record_payload(kr.as_ptr()) };
        assert_eq!(cached, recomputed);
    }

    #[test]
    fn build_string_record_clears_flag_for_non_ascii_payload() {
        // U+00E9 (é) encodes as 2 UTF-8 bytes both >= 0x80 — the
        // producer must NOT mark the record ASCII.
        let kr = build_string_record("caf\u{00E9}");
        assert!(!unsafe { is_ascii_flag_set(kr.as_ptr()) });
        // Hash + length must still round-trip via the flag-masked
        // length field.
        let cached = unsafe { fx_hash_key_record(kr.as_ptr()) };
        let recomputed = unsafe { fx_hash_key_record_payload(kr.as_ptr()) };
        assert_eq!(cached, recomputed);
    }

    #[test]
    fn build_string_record_empty_payload_is_ascii() {
        // Empty payload is vacuously ASCII; the flag is set so a
        // case_fold call on `""` skips the slow path right away.
        let kr = build_string_record("");
        assert!(unsafe { is_ascii_flag_set(kr.as_ptr()) });
    }

    #[test]
    fn dict_lookup_still_hits_with_ascii_flag_present() {
        // Belt-and-braces: the dict-lookup hot path must keep working
        // unchanged with the ASCII flag occupying bit 31 of the
        // length field. The hash is computed over the payload only,
        // so the flag should not perturb it.
        let kr = build_string_record("ascii_key");
        let h = unsafe { fx_hash_key_record(kr.as_ptr()) };
        let dict = build_dict_record(0xfeed, &[(h, 99)]);
        let mut ctx = TraceContext::with_capacity(0);
        let got =
            unsafe { __relon_trace_dict_lookup(dict.as_ptr(), kr.as_ptr(), 0xfeed, &mut ctx) };
        assert_eq!(got, 99);
    }

    // ---- v2 helper: hit, miss, shape mismatch ----------------------

    #[test]
    fn dict_lookup_v2_hit_returns_value() {
        let entries: Vec<(&[u8], i64)> = vec![(b"a", 10), (b"bb", 20), (b"ccc", 30)];
        let shape: u64 = 0xfeed_face_dead_beef;
        let dict = build_dict_record_v2(shape, &entries);
        let mut ctx = TraceContext::with_capacity(0);
        for (key, expected) in &entries {
            let kr = build_string_record(std::str::from_utf8(key).unwrap());
            let got = unsafe {
                __relon_trace_dict_lookup_v2(
                    dict.as_ptr(),
                    dict.len(),
                    kr.as_ptr(),
                    shape,
                    &mut ctx,
                )
            };
            assert_eq!(got, *expected, "v2 key {:?} value mismatch", key);
        }
    }

    #[test]
    fn dict_lookup_v2_shape_mismatch_returns_deopt() {
        let entries: Vec<(&[u8], i64)> = vec![(b"a", 42)];
        let dict = build_dict_record_v2(0xaaaa, &entries);
        let kr = build_string_record("a");
        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_v2(dict.as_ptr(), dict.len(), kr.as_ptr(), 0xbbbb, &mut ctx)
        };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
    }

    #[test]
    fn dict_lookup_v2_missing_key_returns_deopt() {
        let entries: Vec<(&[u8], i64)> = vec![(b"a", 7)];
        let dict = build_dict_record_v2(0xc0de, &entries);
        let kr_missing = build_string_record("z");
        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_v2(
                dict.as_ptr(),
                dict.len(),
                kr_missing.as_ptr(),
                0xc0de,
                &mut ctx,
            )
        };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
    }

    // ---- v2 helper: review #175 hash-collision regression ----------

    #[test]
    fn dict_lookup_v2_hash_collision_returns_correct_value() {
        // Review #175 P2 regression: forge two distinct key records
        // that hash to the same FxHash64 by *manually stamping* the
        // cached hash field in the lookup key's string record to the
        // FxHash of an unrelated payload. The v1 helper would return
        // the entry whose hash matches the stamped digest — silent
        // corruption. The v2 helper must `memcmp` the payload and
        // therefore return DICT_LOOKUP_DEOPT (no real entry matches
        // the looked-up payload).
        let entries: Vec<(&[u8], i64)> = vec![(b"alpha", 100), (b"bravo", 200), (b"charlie", 300)];
        let dict = build_dict_record_v2(0xdead, &entries);

        // Build a key record whose payload is "ghost" but whose cached
        // hash field points to the FxHash of "alpha" — i.e. a forged
        // FxHash collision. fx_hash_key_record loads the cached field
        // directly, so the helper sees the colliding hash but the
        // payload memcmp must catch the mismatch.
        let collision_hash = fx_hash_bytes(b"alpha");
        let payload: &[u8] = b"ghost";
        let mut forged = Vec::with_capacity(STRING_RECORD_PAYLOAD_OFFSET as usize + payload.len());
        let len_with_flags = payload.len() as u32 | STRING_RECORD_ASCII_FLAG_BIT;
        forged.extend_from_slice(&len_with_flags.to_le_bytes());
        forged.extend_from_slice(&collision_hash.to_le_bytes());
        forged.extend_from_slice(payload);

        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_v2(
                dict.as_ptr(),
                dict.len(),
                forged.as_ptr(),
                0xdead,
                &mut ctx,
            )
        };
        assert_eq!(
            got, DICT_LOOKUP_DEOPT,
            "v2 helper must reject a forged hash collision (payload memcmp catches it)"
        );

        // Belt-and-braces: a real lookup of "alpha" still hits.
        let kr_real = build_string_record("alpha");
        let got_real = unsafe {
            __relon_trace_dict_lookup_v2(
                dict.as_ptr(),
                dict.len(),
                kr_real.as_ptr(),
                0xdead,
                &mut ctx,
            )
        };
        assert_eq!(got_real, 100, "v2 helper still hits genuine payload");
    }

    #[test]
    fn dict_lookup_v2_hash_collision_keeps_scanning_for_real_entry() {
        // Construct two dict entries whose hashes collide (forge it by
        // sharing the same key_hash slot via the v2 builder's manual
        // entry placement) and verify the helper returns the
        // payload-matching entry instead of the first hash hit. We
        // build the record by hand because `build_dict_record_v2`
        // hashes via fx_hash_bytes (no API to inject collisions).
        let payload_a: &[u8] = b"alpha";
        let payload_b: &[u8] = b"bravo";
        let real_hash = fx_hash_bytes(payload_a); // re-used as the
                                                  // colliding fake hash for the b-entry.
                                                  // Manually assemble: shape | entry_count=2 | entry_a (hash=real_hash, value=999, payload=alpha)
                                                  // | entry_b (hash=real_hash, value=42, payload=bravo) | payload_a | payload_b.
        let shape: u64 = 0xbeef;
        let header_size = DICT_V2_ENTRIES_OFFSET + DICT_V2_ENTRY_STRIDE * 2;
        let off_a = header_size as u32;
        let off_b = (header_size + payload_a.len()) as u32;
        let mut record = Vec::with_capacity(header_size + payload_a.len() + payload_b.len());
        record.extend_from_slice(&shape.to_le_bytes());
        record.extend_from_slice(&2u32.to_le_bytes());
        // entry a — wrong value, used to verify we don't stop at the
        // first hash hit when the payload doesn't match.
        record.extend_from_slice(&real_hash.to_le_bytes());
        record.extend_from_slice(&off_a.to_le_bytes());
        record.extend_from_slice(&(payload_a.len() as u32).to_le_bytes());
        record.extend_from_slice(&999i64.to_le_bytes());
        // entry b — colliding hash, real value, payload "bravo".
        record.extend_from_slice(&real_hash.to_le_bytes());
        record.extend_from_slice(&off_b.to_le_bytes());
        record.extend_from_slice(&(payload_b.len() as u32).to_le_bytes());
        record.extend_from_slice(&42i64.to_le_bytes());
        record.extend_from_slice(payload_a);
        record.extend_from_slice(payload_b);

        // Lookup key "bravo" but with the cached hash forced to
        // real_hash (same as entry_a's hash). The v2 helper must skip
        // entry_a after the payload memcmp fails and return entry_b's
        // value (42).
        let payload: &[u8] = b"bravo";
        let len_with_flags = payload.len() as u32 | STRING_RECORD_ASCII_FLAG_BIT;
        let mut forged = Vec::with_capacity(STRING_RECORD_PAYLOAD_OFFSET as usize + payload.len());
        forged.extend_from_slice(&len_with_flags.to_le_bytes());
        forged.extend_from_slice(&real_hash.to_le_bytes());
        forged.extend_from_slice(payload);

        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_v2(
                record.as_ptr(),
                record.len(),
                forged.as_ptr(),
                shape,
                &mut ctx,
            )
        };
        assert_eq!(
            got, 42,
            "v2 helper must scan past a colliding entry and return the payload-matching one"
        );
    }

    #[test]
    fn dict_lookup_v2_rejects_truncated_record_len() {
        // record_len smaller than the minimum header → deopt.
        let entries: Vec<(&[u8], i64)> = vec![(b"a", 10)];
        let dict = build_dict_record_v2(0xfeed, &entries);
        let kr = build_string_record("a");
        let mut ctx = TraceContext::with_capacity(0);
        // record_len = 4 < DICT_V2_ENTRIES_OFFSET ⇒ deopt.
        let got = unsafe {
            __relon_trace_dict_lookup_v2(dict.as_ptr(), 4, kr.as_ptr(), 0xfeed, &mut ctx)
        };
        assert_eq!(got, DICT_LOOKUP_DEOPT);
        // record_len cut off mid-entry-table ⇒ deopt before any read.
        let got2 = unsafe {
            __relon_trace_dict_lookup_v2(
                dict.as_ptr(),
                DICT_V2_ENTRIES_OFFSET + 4,
                kr.as_ptr(),
                0xfeed,
                &mut ctx,
            )
        };
        assert_eq!(got2, DICT_LOOKUP_DEOPT);
    }

    #[test]
    fn dict_lookup_prechecked_v2_hit_returns_value() {
        let entries: Vec<(&[u8], i64)> = vec![(b"hello", 7), (b"world", 11)];
        let dict = build_dict_record_v2(0xdeadbeef, &entries);
        let kr = build_string_record("hello");
        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_prechecked_v2(
                dict.as_ptr(),
                dict.len(),
                kr.as_ptr(),
                &mut ctx,
            )
        };
        assert_eq!(got, 7);
    }

    #[test]
    fn dict_lookup_prechecked_v2_ignores_shape_field() {
        let entries: Vec<(&[u8], i64)> = vec![(b"k", 42)];
        // Stamp a deliberately wrong shape — the prechecked helper
        // must NOT compare it (paired DictShapeGuard owns that check).
        let dict = build_dict_record_v2(0xaaaa_bbbb, &entries);
        let kr = build_string_record("k");
        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_prechecked_v2(
                dict.as_ptr(),
                dict.len(),
                kr.as_ptr(),
                &mut ctx,
            )
        };
        assert_eq!(got, 42);
    }

    #[test]
    fn dict_lookup_prechecked_v2_hash_collision_returns_correct_value() {
        // Same forged-collision shape as the full-helper regression
        // test, but exercising the prechecked variant.
        let entries: Vec<(&[u8], i64)> = vec![(b"alpha", 100), (b"bravo", 200)];
        let dict = build_dict_record_v2(0x123, &entries);

        let collision_hash = fx_hash_bytes(b"alpha");
        let payload: &[u8] = b"ghost";
        let mut forged = Vec::with_capacity(STRING_RECORD_PAYLOAD_OFFSET as usize + payload.len());
        let len_with_flags = payload.len() as u32 | STRING_RECORD_ASCII_FLAG_BIT;
        forged.extend_from_slice(&len_with_flags.to_le_bytes());
        forged.extend_from_slice(&collision_hash.to_le_bytes());
        forged.extend_from_slice(payload);

        let mut ctx = TraceContext::with_capacity(0);
        let got = unsafe {
            __relon_trace_dict_lookup_prechecked_v2(
                dict.as_ptr(),
                dict.len(),
                forged.as_ptr(),
                &mut ctx,
            )
        };
        assert_eq!(
            got, DICT_LOOKUP_DEOPT,
            "prechecked v2 helper must reject a forged hash collision"
        );
    }
}
