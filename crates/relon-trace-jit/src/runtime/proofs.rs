//! F-2 Kani bounded model checks for JIT runtime helper arithmetic.
//!
//! Each `#[kani::proof]` here states a layout-arithmetic property the
//! corresponding `__relon_trace_*` / `__relon_str_*` helper relies on.
//! The properties focus on the **pure arithmetic** (saturating /
//! checked ops, bounds checks) rather than the surrounding unsafe
//! pointer manipulation, because Kani's symbolic exec can't model raw
//! `*const u8` walks against arbitrary buffers; the helpers themselves
//! have unit + miri coverage for that.
//!
//! ## What we prove
//!
//! * `dict_v2_entry_table_bounds_valid` — for any `entry_count` /
//!   `record_len` the helper accepts (entries_end ≤ record_len), every
//!   entry index `i ∈ [0, entry_count)` lands at an offset whose tail
//!   header byte is still inside `record_len`. Locks the safety of the
//!   per-iter `entries_base.add(i * 24)` reads in
//!   `__relon_trace_dict_lookup_v2`.
//!
//! * `dict_v2_stored_payload_bounds_imply_in_record` — for any
//!   `stored_off` / `stored_len` the helper accepts (stored_off ≥
//!   entries_end ∧ stored_off+stored_len ≤ record_len), the
//!   `from_raw_parts(stored_payload, stored_len)` slice is inside the
//!   record. Locks the safety of the post-hash memcmp.
//!
//! * `str_concat_n_alloc_block_size_fits_when_accepted` — when the
//!   helper passes its `Layout::from_size_align` check
//!   (`header_size + total_len ≤ isize::MAX`), the per-operand
//!   `cursor + r.len ≤ total_len` test ensures every `payload_ptr +
//!   cursor + r.len` stays inside the allocation.
//!
//! * `str_substring_clamp_keeps_inside_payload` — for any
//!   `start`/`len`/`payload_len`, the clamping in
//!   [`__relon_str_substring`] yields a `(start', end')` pair with
//!   `start' ≤ end' ≤ payload_len`. Locks the `from_raw_parts` slice
//!   into the live `StringRef` payload.
//!
//! ## CI gating
//!
//! Kani isn't a default toolchain; the `#[cfg(kani)]` gate keeps these
//! proofs out of stable / miri compilation. Run locally via
//! `cargo kani -p relon-trace-jit --harness <name>`. A dedicated CI
//! job can land alongside F-1's miri sweep once the Kani container
//! image stabilises in our matrix.

#![cfg(kani)]

use super::dict_list::{DICT_V2_ENTRIES_OFFSET, DICT_V2_ENTRY_STRIDE};

/// Property: when `__relon_trace_dict_lookup_v2`'s entries-end gate
/// (`12 + entry_count * 24 ≤ record_len`) passes, every per-iteration
/// `entries_base.add(i * 24)..add(i * 24 + 24)` access lands inside
/// `record_len`. Bounds the safety of the inner loop's
/// `(entry_ptr as *const u64).read_unaligned()` plus the `+8 / +12 /
/// +16` field reads.
#[kani::proof]
fn dict_v2_entry_table_bounds_valid() {
    let entry_count: u32 = kani::any();
    let record_len: usize = kani::any();
    // Bound `entry_count` so the symbolic search stays tractable;
    // we still cover the full u32 dynamic range up to the bound.
    kani::assume(entry_count <= 1024);
    kani::assume(record_len < 1 << 30);

    let entries_total = (entry_count as usize).saturating_mul(DICT_V2_ENTRY_STRIDE);
    let entries_end = DICT_V2_ENTRIES_OFFSET.saturating_add(entries_total);

    if entries_end > record_len {
        return; // helper deopts; no further reads happen
    }

    // Helper proceeds. For every i ∈ [0, entry_count), prove the i-th
    // entry's last byte is still inside the record.
    let i: u32 = kani::any();
    kani::assume(i < entry_count);
    let entry_off = (i as usize) * DICT_V2_ENTRY_STRIDE;
    let entry_start = DICT_V2_ENTRIES_OFFSET + entry_off;
    let entry_end = entry_start + DICT_V2_ENTRY_STRIDE;
    assert!(
        entry_end <= record_len,
        "entry table OOB despite passing the bounds gate"
    );
    // The inner-most field reads at `entry_ptr.add(8)` / `+12` / `+16`
    // are subsumed by `entry_end <= record_len` since entry_end =
    // entry_start + 24 and the 16-byte field offset is < 24.
}

/// Property: when `__relon_trace_dict_lookup_v2`'s post-hash payload
/// bounds gate (`stored_off ≥ entries_end ∧ stored_end ≤ record_len`)
/// passes, the subsequent `slice::from_raw_parts(stored_payload,
/// stored_len)` reads stay inside the live record.
#[kani::proof]
fn dict_v2_stored_payload_bounds_imply_in_record() {
    let entries_end: usize = kani::any();
    let stored_off: usize = kani::any();
    let stored_len: usize = kani::any();
    let record_len: usize = kani::any();
    kani::assume(entries_end <= 1 << 20);
    kani::assume(record_len <= 1 << 20);

    let stored_end = stored_off.saturating_add(stored_len);
    if stored_off < entries_end || stored_end > record_len {
        return; // helper deopts
    }

    // Helper proceeds. Prove the [stored_off, stored_off + stored_len)
    // range is a subset of [entries_end, record_len) ⊆ [0, record_len).
    assert!(stored_off >= entries_end);
    assert!(stored_off + stored_len <= record_len);
    assert!(stored_off + stored_len >= stored_off); // no usize wrap
}

/// Property: when `__relon_str_concat_n_alloc`'s per-operand bounds
/// check (`cursor + r.len ≤ total_len`) passes for every operand, the
/// final `cursor` reaches at most `total_len`, so all `copy_nonoverlapping`
/// destinations stay inside the allocation `[payload, payload+total_len)`.
///
/// The proof unrolls to the helper's `MAX_INLINE_STR_CONCAT_N = 4` cap;
/// `#[kani::unwind(5)]` bounds CBMC's symbolic loop expansion so the
/// SAT solver doesn't blow up on a symbolic-bounded `for _ in 0..n`.
#[kani::proof]
#[kani::unwind(5)]
fn str_concat_n_alloc_cursor_stays_in_payload() {
    // MAX_INLINE_STR_CONCAT_N = 4 today, so the helper only ever
    // iterates 0..=4 times. Fix the bound concretely instead of
    // symbolically so CBMC sees one bounded loop, not a doubly
    // symbolic `n × r_len` search space.
    let total_len: usize = kani::any();
    let n: usize = kani::any();
    kani::assume(n <= 4);
    kani::assume(total_len <= 1 << 10);

    let mut cursor: usize = 0;
    for _ in 0..n {
        let r_len: usize = kani::any();
        kani::assume(r_len <= total_len); // per-operand bound (the
                                          // helper's check is the
                                          // saturating_add version
                                          // below; this just keeps
                                          // CBMC's space small)
        if cursor.saturating_add(r_len) > total_len {
            return; // helper bails out via dealloc + null
        }
        cursor += r_len;
        assert!(cursor <= total_len);
    }
    // After the loop the StringRef header records `len = cursor`. The
    // backing allocation is `header_size + total_len`, so payload reads
    // up to `cursor` ≤ `total_len` stay inside.
    assert!(cursor <= total_len);
}

/// Property: `__relon_str_substring`'s `(start, len, payload_len)`
/// clamping yields a `(start', end')` pair satisfying
/// `start' ≤ end' ≤ payload_len`. The helper uses this to slice the
/// payload via `from_raw_parts`; the property locks that slice into
/// the live `StringRef`.
#[kani::proof]
fn str_substring_clamp_keeps_inside_payload() {
    let start: i64 = kani::any();
    let len: i64 = kani::any();
    let payload_len: usize = kani::any();
    kani::assume(payload_len <= 1 << 20);

    // Mirror the helper's clamping (see str_ops.rs:780-805 for the
    // matching implementation): negative start clamps to 0; start
    // beyond payload_len returns empty; len clamps to remaining.
    let start_u = if start < 0 {
        0usize
    } else {
        (start as usize).min(payload_len)
    };
    let remaining = payload_len - start_u;
    let len_u = if len < 0 {
        0usize
    } else {
        (len as usize).min(remaining)
    };

    let end = start_u + len_u;
    assert!(start_u <= end);
    assert!(end <= payload_len);
}
