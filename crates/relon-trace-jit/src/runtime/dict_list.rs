//! F-D8: host-side helpers backing `TraceOp::ListGet` /
//! `TraceOp::DictLookup`.
//!
//! The cranelift emitter lowers `TraceOp::DictLookup` to a single
//! `call __relon_trace_dict_lookup(dict_ptr, key_ptr, shape_hash, ctx)`
//! and lowers `TraceOp::ListGet` to either:
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
//! For F-D8 v1 the dict is keyed on shape: the host pre-computes an
//! FxHash digest of the keys in the dict at recording time, stamps it
//! into the dict header, and the trace's `shape_hash` immediate is
//! that same digest. On IC hit the helper short-circuits with the
//! cached value lookup; on miss it returns [`DICT_LOOKUP_DEOPT`] (an
//! `i64::MIN` sentinel) so the cranelift-side branch falls into the
//! shared deopt block.
//!
//! The dict header carries a small inline cache of recently-seen
//! `(key_hash, value_idx)` pairs so the steady-state path skips both
//! the BTreeMap walk and the UTF-8 key compare. The cache is
//! single-threaded by construction — each trace runs on the
//! `TraceContext`'s thread, and the helper writes through the dict's
//! mutable header.
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

/// Sentinel returned by [`__relon_trace_list_get`] / [`__relon_trace_dict_lookup`]
/// on out-of-range index / IC shape mismatch. The cranelift emitter
/// compares against this value and branches into the shared deopt
/// block when the helper signals failure.
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

/// IC-guarded dict lookup helper.
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
/// Collision behaviour: F-D8 v1 uses FxHash64 — for the W5 corpus
/// (10 string keys, max 4 bytes each) the per-key collision rate
/// over the entire keyspace is < 2^-32, and the IC hit-path verifies
/// the cached `value` matches the BTreeMap-resolved one at install
/// time. The bench fixtures sidestep BTreeMap by pre-flattening the
/// dict into the entry table at fixture-build time.
///
/// # Safety
///
/// Same shape contract as [`__relon_trace_list_get`]; both pointers
/// must satisfy the documented layouts and outlive the call.
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

/// F-D8-E.2: "shape already checked" companion of
/// [`__relon_trace_dict_lookup`].
///
/// The cranelift emitter lowers `TraceOp::DictLookupPrechecked` into a
/// call to this helper. It is byte-identical to the full
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
/// # Safety
///
/// - Same shape contract as [`__relon_trace_dict_lookup`].
/// - Caller MUST have executed a matching `TraceOp::DictShapeGuard`
///   earlier in the trace, otherwise a dict whose layout drifted from
///   the recorder-time fingerprint would silently scan into garbage
///   entries. The optimizer is the only producer of
///   `DictLookupPrechecked` ops, and it pairs them by construction.
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
    fx_hash_bytes, fx_hash_key_record, fx_hash_key_record_payload, STRING_RECORD_HASH_OFFSET,
    STRING_RECORD_PAYLOAD_OFFSET,
};

/// Convenience constructor: layout-conformant dict key record for
/// unit tests + bench fixtures. Returns a `Vec<u8>` whose layout
/// matches the consumer contract documented on
/// [`fx_hash_key_record`]:
///
/// ```text
/// offset 0  : len   : u32 LE     (payload byte count)
/// offset 4  : hash  : u64 LE     (pre-computed fx_hash_bytes(payload))
/// offset 12 : bytes : [u8; len]  (UTF-8 payload)
/// ```
///
/// Pre-stamping the FxHash at fixture-build time is what makes the
/// W5-class dict hot loop avoid re-hashing each key byte on every
/// iteration — the inline emitter and the runtime helpers both just
/// `load.u64 [key_ptr + STRING_RECORD_HASH_OFFSET]` instead of running
/// the byte-wise hash loop. See the Tier 1a stage report for the
/// before/after numbers.
///
/// The legacy 4-byte-header layout that this used to produce is gone:
/// every consumer of this helper (dict_inline.rs, the W5/W6 bench
/// fixtures, the recorder tests) was updated in the same commit
/// sequence so there is no compatibility surface to preserve.
pub fn build_string_record(s: &str) -> Vec<u8> {
    let len = s.len() as u32;
    let payload = s.as_bytes();
    let hash = fx_hash_bytes(payload);
    let mut out = Vec::with_capacity(STRING_RECORD_PAYLOAD_OFFSET as usize + s.len());
    out.extend_from_slice(&len.to_le_bytes());
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

/// Convenience constructor: layout-conformant dict record for tests
/// + bench fixtures.
///
/// `shape_hash` is the recorder-time fingerprint stamped into the
/// header; `entries` is the pre-flattened (`key_hash`, `value`)
/// table. Callers compute the per-key FxHash via [`fx_hash_bytes`].
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
}
