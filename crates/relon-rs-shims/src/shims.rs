//! Host shim symbols the LLVM AOT emitter references via extern
//! declarations.
//!
//! ## What lives here
//!
//! Today this module provides exactly one symbol:
//! [`relon_llvm_str_contains_arena`] — the F.1 fast path the LLVM
//! emitter routes `Op::Call { contains }` through. The emitted body
//! declares the symbol with `Linkage::External`; the static linker
//! resolves it against this crate's compiled `.rlib` when the build
//! script advertises us as a link target.
//!
//! ## Why not re-export from `relon-codegen-llvm`?
//!
//! `relon-codegen-llvm`'s `str_helpers` module owns the JIT-side
//! resolution path (`engine.add_global_mapping`) for the same shim,
//! but the consuming Rust binary should never link `inkwell` +
//! `llvm-sys` at runtime — those crates pull `libllvm-18` as a system
//! dep and inflate the binary by ~50 MiB. We re-implement the shim
//! verbatim here (algorithm + IC mirror the LLVM-side
//! implementation) so the runtime side stays a thin static-link
//! surface.
//!
//! Both crates' `#[no_mangle]` declarations carry the same symbol
//! name (`relon_llvm_str_contains_arena`); a downstream consumer that
//! accidentally links both will see a duplicate-symbol linker error
//! (loud failure), which is preferable to silent diamond-dependency
//! behaviour.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_llvm_str_contains_arena(
//!     haystack_ptr: *const u8,   // arena_base + haystack_off
//!     needle_ptr:   *const u8,   // arena_base + needle_off
//! ) -> i32                       // 1 = match, 0 = no match
//! ```
//!
//! Both pointers must point at the `[u32 len][bytes...]` header of a
//! valid arena record. On `null` operands the shim returns `0` — the
//! emitter guarantees both pointers are derived from the cached
//! `arena_base + offset` GEP so null never happens on the supported
//! surface; the explicit check is a defence-in-depth backstop.

use core::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

/// Read the `(len, payload_addr)` of an arena String record at `ptr`.
///
/// # Safety
///
/// `ptr` must be either null or point at the start of a well-formed
/// `[u32 len][bytes...]` arena record whose payload extends `len`
/// bytes past the header.
#[inline]
unsafe fn read_record(ptr: *const u8) -> Option<&'static [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller upholds the arena-record invariant.
    let len = unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) } as usize;
    let payload = unsafe { ptr.add(4) };
    Some(unsafe { core::slice::from_raw_parts(payload, len) })
}

/// Single-slot pointer-keyed inline cache for
/// [`relon_llvm_str_contains_arena`]. Mirrors the LLVM-side IC
/// (`STR_CONTAINS_ARENA_IC` in `relon-codegen-llvm::str_helpers`).
///
/// Relaxed atomics are sufficient — a torn read across racing threads
/// only triggers extra misses, never a wrong answer (same pointers
/// imply same arena records imply same `contains` answer).
struct StrContainsArenaIc {
    last_haystack: AtomicPtr<u8>,
    last_needle: AtomicPtr<u8>,
    last_result: AtomicI32,
}

static STR_CONTAINS_ARENA_IC: StrContainsArenaIc = StrContainsArenaIc {
    last_haystack: AtomicPtr::new(core::ptr::null_mut()),
    last_needle: AtomicPtr::new(core::ptr::null_mut()),
    last_result: AtomicI32::new(0),
};

/// LLVM AOT host shim for `str.contains`. Returns `1` if the needle
/// appears in the haystack, else `0`. See the module-level docs for
/// the ABI and arena-record contract.
///
/// # Safety
///
/// Both pointers must be either null or point at a well-formed arena
/// String record (`[u32 len][utf8 bytes]`). The emitter never produces
/// a null pointer on the supported surface — they are GEPs off the
/// cached `arena_base`, which is non-null whenever the entry
/// trampoline is live.
#[no_mangle]
pub unsafe extern "C" fn relon_llvm_str_contains_arena(
    haystack_ptr: *const u8,
    needle_ptr: *const u8,
) -> i32 {
    if let Some(r) = ic_hit_slot(haystack_ptr, needle_ptr) {
        return r;
    }
    // SAFETY: same contract as the outer function.
    unsafe { str_contains_arena_slow(haystack_ptr, needle_ptr) }
}

#[inline(always)]
fn ic_hit_slot(haystack_ptr: *const u8, needle_ptr: *const u8) -> Option<i32> {
    if haystack_ptr.is_null() || needle_ptr.is_null() {
        return None;
    }
    let cached_haystack = STR_CONTAINS_ARENA_IC.last_haystack.load(Ordering::Relaxed);
    if !core::ptr::eq(cached_haystack, haystack_ptr) {
        return None;
    }
    let cached_needle = STR_CONTAINS_ARENA_IC.last_needle.load(Ordering::Relaxed);
    if !core::ptr::eq(cached_needle, needle_ptr) {
        return None;
    }
    Some(STR_CONTAINS_ARENA_IC.last_result.load(Ordering::Relaxed))
}

#[cold]
#[inline(never)]
unsafe fn str_contains_arena_slow(haystack_ptr: *const u8, needle_ptr: *const u8) -> i32 {
    let h_bytes = match unsafe { read_record(haystack_ptr) } {
        Some(s) => s,
        None => return 0,
    };
    let n_bytes = match unsafe { read_record(needle_ptr) } {
        Some(s) => s,
        None => return 0,
    };
    let result = compute_contains(h_bytes, n_bytes);

    STR_CONTAINS_ARENA_IC
        .last_haystack
        .store(haystack_ptr as *mut u8, Ordering::Relaxed);
    STR_CONTAINS_ARENA_IC
        .last_needle
        .store(needle_ptr as *mut u8, Ordering::Relaxed);
    STR_CONTAINS_ARENA_IC
        .last_result
        .store(result, Ordering::Relaxed);
    result
}

#[inline]
fn compute_contains(h_bytes: &[u8], n_bytes: &[u8]) -> i32 {
    if n_bytes.is_empty() {
        return 1;
    }
    if n_bytes.len() > h_bytes.len() {
        return 0;
    }
    if n_bytes.len() == 1 {
        let needle_byte = n_bytes[0];
        return i32::from(h_bytes.contains(&needle_byte));
    }
    let h_str = match core::str::from_utf8(h_bytes) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let n_str = match core::str::from_utf8(n_bytes) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(h_str.contains(n_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_two_records(haystack: &[u8], needle: &[u8]) -> (Vec<u8>, usize, usize) {
        let mut buf = Vec::with_capacity(4 + haystack.len() + 4 + needle.len() + 16);
        let h_off = buf.len();
        buf.extend_from_slice(&(haystack.len() as u32).to_le_bytes());
        buf.extend_from_slice(haystack);
        let n_off = buf.len();
        buf.extend_from_slice(&(needle.len() as u32).to_le_bytes());
        buf.extend_from_slice(needle);
        (buf, h_off, n_off)
    }

    /// Serialize + reset the process-global `STR_CONTAINS_ARENA_IC` for the
    /// duration of a test. Without this, sibling tests run concurrently and
    /// the allocator can hand a fresh buffer the same address a prior test's
    /// freed buffer used; the pointer-keyed IC would then surface that prior
    /// test's stale result. Holding the guard for the whole test (not just the
    /// reset) keeps a concurrent sibling from re-polluting the slot mid-call.
    fn lock_and_reset_ic() -> std::sync::MutexGuard<'static, ()> {
        static IC_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = IC_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        STR_CONTAINS_ARENA_IC
            .last_haystack
            .store(core::ptr::null_mut(), Ordering::Relaxed);
        STR_CONTAINS_ARENA_IC
            .last_needle
            .store(core::ptr::null_mut(), Ordering::Relaxed);
        STR_CONTAINS_ARENA_IC
            .last_result
            .store(0, Ordering::Relaxed);
        guard
    }

    #[test]
    fn matches_short_needle() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"axb", b"x");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }

    #[test]
    fn misses_when_needle_absent() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"abc", b"z");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 0);
    }

    #[test]
    fn empty_needle_always_matches() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"anything", b"");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }

    #[test]
    fn null_pointers_return_zero() {
        let r = unsafe { relon_llvm_str_contains_arena(core::ptr::null(), core::ptr::null()) };
        assert_eq!(r, 0);
    }

    #[test]
    fn multibyte_utf8_needle() {
        let _ic = lock_and_reset_ic();
        let haystack = "hello 🦀 world".as_bytes();
        let needle = "🦀".as_bytes();
        let (buf, h_off, n_off) = build_two_records(haystack, needle);
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }
}
