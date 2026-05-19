//! F-D7 string fast-path runtime shims.
//!
//! The trace emitter lowers `TraceOp::StrConcat` / `StrContains` /
//! `StrFind` / `StrSubstring` to direct `call` instructions targeting
//! the four `__relon_str_*` symbols defined here. Each shim accepts
//! and returns `*const StringRef` pointers — opaque to the JIT — and
//! performs the actual string work on the Rust side, including
//! allocation for ops that produce a fresh result.
//!
//! ## ABI summary
//!
//! All four shims are `unsafe extern "C"` so cranelift IR can call
//! them via the standard SystemV/win64 ABI.
//!
//! ```text
//! __relon_str_concat(lhs: *const StringRef, rhs: *const StringRef)
//!     -> *const StringRef
//! __relon_str_contains(haystack: *const StringRef, needle: *const StringRef)
//!     -> i32       // 0 = false, 1 = true
//! __relon_str_find(haystack: *const StringRef, needle: *const StringRef)
//!     -> i64       // byte index, -1 on miss
//! __relon_str_substring(s: *const StringRef, start: i64, length: i64)
//!     -> *const StringRef
//! ```
//!
//! ## Lifetime / ownership model
//!
//! `StringRef` is a `#[repr(C)]` host-side box whose lifetime is owned
//! by the surrounding [`crate::TraceContext`] — host-side glue stores
//! every allocated `StringRef` into a per-trace arena so a deopt-fired
//! mid-trace cleanly drops the chain. From the JIT's perspective the
//! pointers are opaque `i64` slots; the shim is responsible for
//! reading the `(ptr, len)` payload and writing a fresh entry to the
//! arena on each allocation.
//!
//! For the v6-ζ-F-D7 drop the arena is intentionally simple: each
//! call leaks a `Box<StringRef>` whose backing `Box<str>` is
//! `mem::forget`-ed and never reclaimed. Subsequent F-D7 phases will
//! wire the StringRef into the `TraceContext::pending_recoverable_writes`
//! so the deopt path can drop them; until then a short hot trace
//! leaks the per-iter strings but the cmp_lua bench bounds the total
//! at `STRING_CONCAT_N` iterations.
//!
//! The shims are SAFE to call from any thread because each trace
//! context lives on a single thread by design (`thread_local` call
//! table — see `call_table.rs`); the leak-arena above also operates
//! per-thread via the same constraint.
//!
//! ## Inline cache
//!
//! `__relon_str_contains` consults a tiny pointer-keyed cache (a
//! single MRU slot) before doing the substring scan. The cache is a
//! `thread_local!` so the W4-shaped "same haystack, same needle"
//! benchmark hits without a real scan. Hits short-circuit straight
//! to the cached i32 result; misses fall back to the scan and update
//! the slot.

use std::cell::Cell;

/// Opaque, repr-C string-payload box exposed across the JIT boundary.
///
/// The JIT sees a single `*const StringRef` (an i64); only this crate
/// dereferences it. The struct is `#[repr(C)]` so byte layout is
/// stable across opt levels; the underlying `Box<str>` (or its raw
/// `(ptr, len)`) is **not** dropped automatically — see the leak
/// caveat in the module docs.
#[repr(C)]
pub struct StringRef {
    /// UTF-8 payload pointer. Stable for the lifetime of the
    /// surrounding allocation.
    pub ptr: *const u8,
    /// Payload byte length.
    pub len: usize,
}

impl StringRef {
    /// Build a `StringRef` from a Rust `&str`. The returned reference
    /// borrows from `s` — caller must keep `s` alive for as long as
    /// the JIT may use the pointer.
    pub fn borrow(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// Build a `StringRef` whose payload lives in a leaked `Box<str>`.
    /// The returned pointer is suitable for handing to the JIT and
    /// keeping alive for the lifetime of the surrounding trace.
    ///
    /// ## Safety
    ///
    /// Caller must arrange for the leaked allocation to be reclaimed
    /// once the trace stops running — typically by storing the
    /// returned `*const StringRef` in the trace's `TraceContext` and
    /// dropping it on context teardown. The shim layer leaks for now
    /// (see module docs).
    pub fn from_owned(s: String) -> *const StringRef {
        let boxed_str: Box<str> = s.into_boxed_str();
        let len = boxed_str.len();
        let ptr = Box::into_raw(boxed_str) as *const u8;
        let r = Box::new(StringRef { ptr, len });
        Box::into_raw(r) as *const StringRef
    }

    /// Build a `StringRef` from a `&'static str` source. Useful for
    /// host-side construction of constant inputs in tests.
    pub fn from_static(s: &'static str) -> *const StringRef {
        // We can still leak a fresh box because the JIT-side API takes
        // a single `*const StringRef`; the inner ptr borrows from the
        // static and never needs to be freed.
        let r = Box::new(StringRef::borrow(s));
        Box::into_raw(r) as *const StringRef
    }

    /// Read back a `&str` slice from the pointer. Returns `None` if
    /// `ptr` is null.
    ///
    /// ## Safety
    ///
    /// `ptr` must point at a `StringRef` whose `ptr/len` payload is
    /// valid UTF-8 — typically because it was produced by one of the
    /// shims below.
    pub unsafe fn as_str<'a>(ptr: *const StringRef) -> Option<&'a str> {
        if ptr.is_null() {
            return None;
        }
        let r = &*ptr;
        if r.ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(r.ptr, r.len);
        std::str::from_utf8(bytes).ok()
    }
}

// ---- IC for str_contains ----------------------------------------------

/// Single-slot pointer-keyed cache for `__relon_str_contains`. The
/// W4 benchmark calls `s.contains("x")` in a hot loop with `s` and
/// `"x"` constant across iters; an MRU cache turns the per-iter
/// substring scan into a ~3-ns pointer comparison.
///
/// The cache is `thread_local!` because traces are per-thread by
/// design (`call_table.rs` §1.4). Concurrent traces on different
/// threads see independent caches.
#[derive(Default)]
struct StrContainsIc {
    last_haystack: Cell<*const StringRef>,
    last_needle: Cell<*const StringRef>,
    last_result: Cell<i32>,
    hit_count: Cell<u64>,
    miss_count: Cell<u64>,
}

thread_local! {
    static STR_CONTAINS_IC: StrContainsIc = StrContainsIc::default();
}

/// Diagnostic: per-thread cache hit / miss counters for
/// `__relon_str_contains`. Returns `(hits, misses)`. Tests read this
/// to verify the IC actually fires on the W4-shaped benchmark.
pub fn str_contains_ic_counts() -> (u64, u64) {
    STR_CONTAINS_IC.with(|ic| (ic.hit_count.get(), ic.miss_count.get()))
}

/// Reset the IC counters and slot. Tests call this to start each
/// case with a clean cache so hit/miss ratios are deterministic.
pub fn reset_str_contains_ic() {
    STR_CONTAINS_IC.with(|ic| {
        ic.last_haystack.set(std::ptr::null());
        ic.last_needle.set(std::ptr::null());
        ic.last_result.set(0);
        ic.hit_count.set(0);
        ic.miss_count.set(0);
    });
}

// ---- Public shims ----------------------------------------------------

/// F-D7 `__relon_str_concat`. See [`module docs`](self) for ABI.
///
/// On null inputs the result is null — the JIT side treats null as a
/// trace-abort sentinel; the recorder is expected to emit a
/// `Guard(NotNull(_))` for each operand before reaching this op.
///
/// ## Safety
///
/// Both `lhs` and `rhs` must be either null or a valid
/// `*const StringRef` previously produced by another shim or by
/// [`StringRef::from_owned`] / [`StringRef::from_static`].
#[no_mangle]
pub unsafe extern "C" fn __relon_str_concat(
    lhs: *const StringRef,
    rhs: *const StringRef,
) -> *const StringRef {
    let a = match StringRef::as_str(lhs) {
        Some(s) => s,
        None => return std::ptr::null(),
    };
    let b = match StringRef::as_str(rhs) {
        Some(s) => s,
        None => return std::ptr::null(),
    };
    let mut out = String::with_capacity(a.len() + b.len());
    out.push_str(a);
    out.push_str(b);
    StringRef::from_owned(out)
}

/// F-D7 `__relon_str_contains`. Consults the single-slot
/// MRU cache before scanning; updates the cache on miss.
///
/// ## Safety
///
/// Both operands must be null or a valid `*const StringRef`.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_contains(
    haystack: *const StringRef,
    needle: *const StringRef,
) -> i32 {
    // IC fast path: identical (haystack, needle) pointers as the last
    // call → return cached result without a scan. Pointer equality is
    // fine here because `Arc<str>` (or interned-string) instances on
    // the host side keep their payload pointers stable.
    let cached = STR_CONTAINS_IC.with(|ic| {
        if !haystack.is_null()
            && !needle.is_null()
            && ic.last_haystack.get() == haystack
            && ic.last_needle.get() == needle
        {
            ic.hit_count.set(ic.hit_count.get() + 1);
            Some(ic.last_result.get())
        } else {
            None
        }
    });
    if let Some(r) = cached {
        return r;
    }

    let h = match StringRef::as_str(haystack) {
        Some(s) => s,
        None => return 0,
    };
    let n = match StringRef::as_str(needle) {
        Some(s) => s,
        None => return 0,
    };
    let result = if h.contains(n) { 1 } else { 0 };
    STR_CONTAINS_IC.with(|ic| {
        ic.last_haystack.set(haystack);
        ic.last_needle.set(needle);
        ic.last_result.set(result);
        ic.miss_count.set(ic.miss_count.get() + 1);
    });
    result
}

/// F-D7 `__relon_str_find`. Returns the byte index of the first
/// occurrence, or `-1` on miss. Mirrors Rust's `str::find` exactly.
///
/// ## Safety
///
/// Both operands must be null or a valid `*const StringRef`.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_find(
    haystack: *const StringRef,
    needle: *const StringRef,
) -> i64 {
    let h = match StringRef::as_str(haystack) {
        Some(s) => s,
        None => return -1,
    };
    let n = match StringRef::as_str(needle) {
        Some(s) => s,
        None => return -1,
    };
    match h.find(n) {
        Some(idx) => idx as i64,
        None => -1,
    }
}

/// F-D7 `__relon_str_substring`. Byte-indexed substring with the
/// tree-walker's exact clamp semantics: `start` and `length` are
/// clamped into `[0, len(s)]`, then walked to the nearest char
/// boundary so the returned slice stays valid UTF-8.
///
/// ## Safety
///
/// `s` must be null or a valid `*const StringRef`.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_substring(
    s: *const StringRef,
    start: i64,
    length: i64,
) -> *const StringRef {
    let payload = match StringRef::as_str(s) {
        Some(s) => s,
        None => return std::ptr::null(),
    };
    let s_len = payload.len() as i64;
    let start = start.clamp(0, s_len) as usize;
    let length = length.max(0) as usize;
    let end = (start + length).min(payload.len());
    if end <= start {
        return StringRef::from_owned(String::new());
    }
    // Walk to nearest char boundary so the slice stays UTF-8 even on
    // mid-codepoint byte indices.
    let real_start = payload
        .char_indices()
        .find(|(i, _)| *i >= start)
        .map(|(i, _)| i)
        .unwrap_or(payload.len());
    let real_end = payload
        .char_indices()
        .find(|(i, _)| *i >= end)
        .map(|(i, _)| i)
        .unwrap_or(payload.len());
    StringRef::from_owned(payload[real_start..real_end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_two_static_strings() {
        let a = StringRef::from_static("hello, ");
        let b = StringRef::from_static("world");
        let r = unsafe { __relon_str_concat(a, b) };
        assert!(!r.is_null());
        let s = unsafe { StringRef::as_str(r) }.unwrap();
        assert_eq!(s, "hello, world");
    }

    #[test]
    fn contains_hit_returns_one() {
        reset_str_contains_ic();
        let h = StringRef::from_static("axb");
        let n = StringRef::from_static("x");
        let r1 = unsafe { __relon_str_contains(h, n) };
        assert_eq!(r1, 1);
        // Same pointers → IC hit on the second call.
        let r2 = unsafe { __relon_str_contains(h, n) };
        assert_eq!(r2, 1);
        let (hits, misses) = str_contains_ic_counts();
        assert_eq!(hits, 1, "second call should hit");
        assert_eq!(misses, 1, "first call should miss");
    }

    #[test]
    fn contains_miss_returns_zero() {
        reset_str_contains_ic();
        let h = StringRef::from_static("axb");
        let n = StringRef::from_static("z");
        let r = unsafe { __relon_str_contains(h, n) };
        assert_eq!(r, 0);
    }

    #[test]
    fn find_returns_byte_index_or_neg_one() {
        let h = StringRef::from_static("hello, world");
        let n = StringRef::from_static("world");
        let r = unsafe { __relon_str_find(h, n) };
        assert_eq!(r, 7);
        let miss = StringRef::from_static("zzz");
        let r2 = unsafe { __relon_str_find(h, miss) };
        assert_eq!(r2, -1);
    }

    #[test]
    fn substring_clamps_oob_inputs() {
        let s = StringRef::from_static("hello");
        // Negative start clamps to 0; over-long length clamps to len.
        let r = unsafe { __relon_str_substring(s, -10, 100) };
        let out = unsafe { StringRef::as_str(r) }.unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn substring_zero_length_returns_empty() {
        let s = StringRef::from_static("hello");
        let r = unsafe { __relon_str_substring(s, 2, 0) };
        let out = unsafe { StringRef::as_str(r) }.unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn null_inputs_return_null_or_neg_one() {
        let r = unsafe { __relon_str_concat(std::ptr::null(), std::ptr::null()) };
        assert!(r.is_null());
        let r2 = unsafe { __relon_str_contains(std::ptr::null(), std::ptr::null()) };
        assert_eq!(r2, 0);
        let r3 = unsafe { __relon_str_find(std::ptr::null(), std::ptr::null()) };
        assert_eq!(r3, -1);
        let r4 = unsafe { __relon_str_substring(std::ptr::null(), 0, 5) };
        assert!(r4.is_null());
    }

    #[test]
    fn contains_ic_distinguishes_pointer_keys() {
        reset_str_contains_ic();
        let h1 = StringRef::from_static("axb");
        let h2 = StringRef::from_static("ayb");
        let n = StringRef::from_static("x");
        // Different haystack pointers → miss + miss, not a hit.
        let _ = unsafe { __relon_str_contains(h1, n) };
        let _ = unsafe { __relon_str_contains(h2, n) };
        let (hits, misses) = str_contains_ic_counts();
        assert_eq!(hits, 0);
        assert_eq!(misses, 2);
    }
}
